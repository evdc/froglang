# No GC: ownership, regions, and one tree of scopes

Status: **brainstorm** (2026-11-05, branch `nogc`). Nothing here is implemented, and the proposal
is expected to change. It touches `MUTABILITY.md` (stage 7 COW is retired), `RUNTIME.md` Part 2
(stack maps are retired), `CONCURRENCY.md` (Rule B, unwinding and the heap model are rewritten),
`SELFHOST.md` (G6, the GC-roots crux, disappears), and `TRAITS.md` Part 7 (`Linear`).

## Summary

1. **The GC can be removed without losing memory safety, and the language already does most of
   the work.** Froglang has no references, no shared mutable cells, and no escaping closure
   environments, so every value graph is a tree. The GC only does two jobs today: it reclaims
   storage that unique ownership could free, and it keeps COW-shared buffers alive. The
   replacement is **unique ownership with inferred parameter conventions and statically placed
   drops**. Sharing becomes a representation choice behind value semantics, and not a semantic
   one. The language needs no new annotations. Five things beyond "MVS + Linear" are needed,
   listed in §1.4.
2. **Under MVS, regions need no region checker.** Nothing can point *into* a region, because
   nothing can point at all. A region's exit therefore only has to evacuate its result. The cost
   of escape is a copy and never a dangling pointer, so arenas become nearly free to add. There
   are three region shapes: the **task heap**, the **`with Arena` block**, and **`Pool<T>`**,
   a value that holds graph data addressed by generational `Id<T>`s.
3. **One structure covers memory, tasks, resources, capabilities and failure.** Structured
   concurrency, arenas, resource scopes and deadlines all reduce to lifetimes nested along one
   dynamic tree. A small kit of constructs (values, linear values, second-class handles, and
   `with` with a three-part protocol) expresses all of them. The tree also yields a cheap
   unwinding model: a task is at once the unit of concurrency, failure and memory reclamation,
   much like an Erlang process. A cancelled or panicking task runs its cleanup stack and drops
   its heap whole. This needs no landing pads and no per-frame destructors.

---

## 0. What the GC is actually doing today

| Heap object | Mutable? | Why it can be shared today | Cycles possible? |
| --- | --- | --- | --- |
| `Str` | no | free duplication: a copy is a pointer copy | no |
| `List` | yes, via `mut` root | COW: `shared` bit, `emit_unshare` | no |
| `Dict` | yes | COW, same as `List` | no |
| `FrogVariant` (boxed union) | no (rebinding only) | pointer copy | no: `data Tree is Leaf \| Node(l: Tree, r: Tree)` builds trees by construction |

No reference type exists, so no object in the language can be reached by two *semantic* paths.
Every second path is an *implementation* path: a COW share, an immutable `Str` pointer copied
by value, or a boxed variant copied by pointer. That means the reachability graph is a forest
plus sharing edges that the implementation introduced and could avoid introducing.

That is the observation everything else rests on. A tracing GC exists to discover reachability
the compiler cannot know. Froglang's compiler knows all of it.

The GC's tax on the codebase is large and mostly invisible in the language:

- Stack maps: `declare_value_needs_stack_map`, the `JitCode` registry, the AOT stack-map table
  and `frog_register_stackmaps`, and `RuntimeRoots`.
- The history of rooting bugs: the `Conditional` slot sharing and the loop back-edge hole, both
  in `MUTABILITY.md` stage 6.
- `heap_roots_in_leaves`, the `FrogState` cross-entry rooting bug, `FROG_GC_STRESS`, and the
  GC-stress test sweep.
- COW: the `shared` bit, `mark_shared_if_aliased`, `mark_shared_extracted`,
  `emit_unshare_nested`, `FROG_COW_VERIFY`/`count_refs`, and the transient-vs-owning compile
  split, along with the two soundness holes stage 8 found.
- In `CONCURRENCY.md`: per-task shadow-stack registries, safepoints for M:N, and "one heap per
  runtime" as the stated reason for M:1.
- In `SELFHOST.md`: G6, "the crux is GC roots".

---

## 1. Removing the GC

### 1.1 Three ways to implement the sharing that value semantics hides

| | Unique ownership + deep copy (Hylo, Mojo) | Reference counting (Swift, Koka/Perceus, Roc, Lean 4, Lobster, Nim ARC) | Tracing GC (today) |
| --- | --- | --- | --- |
| Duplication cost | O(size), only at real duplication | O(1) inc | O(1) |
| Mutation of a shared value | never shared, so always in place | copy if `rc > 1` | copy if `shared` (sticky, imprecise) |
| Per-object header | none needed | count word | `next`, `marked`, `kind`, `shared` |
| Free | static, at last use | dynamic, when count hits 0 | batch, at collection |
| Runtime machinery | an allocator | an allocator plus inc/dec | roots, stack maps, mark/sweep |
| Threading | free: owned memory is disjoint | non-atomic only while M:1; Swift and Arc pay atomics | stop-the-world |
| Cycles | impossible | impossible here (no refs) | n/a |

All three are sound for froglang, and none needs a cycle collector because no reference type
exists. They differ only in *what a duplication costs*. Duplications are exactly what the
liveness analysis already classifies: `Ownership::Copy` versus `Move` on every `Var` node.

**Recommendation: unique ownership as the model, with RC kept as a measured, per-type
optimization.**

- Unique ownership is the simplest runtime that exists (`malloc`/`free` at statically known
  points), makes M:N trivially race-free, and gives emitted C (`SELFHOST.md`) nothing to track.
- The copies it adds are the *residual* ones: a value both stored somewhere and used again.
  Inferred borrowing (§1.2) removes the large majority, which are call arguments and `let`
  aliases of values that are never mutated.
- If benchmarks show residual copies of one type dominating (the likely candidate is `Str`,
  which is immutable and often stored as a key or field), that type alone can become
  RC-shared. RC on an *immutable, acyclic* type needs no COW logic, and the change is invisible
  to programs. Lobster (by Goose's author) is the evidence that compile-time ownership analysis
  removes most RC traffic anyway.

Option B, Perceus-style precise RC on everything with reuse analysis, is the strongest
alternative, and it is what Roc and Lean ship. It wins over deep copy when large values are
genuinely duplicated and then only read, and it loses on per-object headers, inc/dec traffic,
and the threading story. Since froglang can make *every* copy and every drop point statically
visible, starting from zero runtime bookkeeping and adding counts only where measured is the
direction that stays simple. Open question Q1 is where to revisit this.

### 1.2 Parameter conventions, inferred

Hylo writes `let`/`inout`/`sink`/`set` on parameters, and Mojo writes `borrowed`/`inout`/`owned`.
Froglang already writes `mut` (inout), and whole-program compilation through one `FrogState` lets
it **infer** the other two per parameter from the callee body:

- **borrow** (the default): the callee only reads the parameter or passes it to other borrow
  positions. The caller passes a pointer and never copies.
- **consume**: the callee returns the parameter, stores it into a value that escapes, or passes
  it to a consume position. The caller *moves* it if this is the argument's last use and
  *copies* it otherwise. The callee frees it if it does not escape on some path.
- **mut**: as today. Copy-in/copy-out turns into a pass by pointer, because exclusivity is
  already checked.

Recursive SCCs take a fixpoint. Function values are already monomorphized per callee
(`lower_function_values`), so every call site knows its callee. Host functions declare their
conventions, and marshalling copies anyway. This is the same analysis Perceus uses to infer
borrowed parameters, and it replaces RC operations here exactly as it does there.

`let` bindings get the same treatment, following Hylo's `let`, which is a borrow and not a
copy. `let c = b` is a borrow of `b` when `b` is neither mutated nor moved during `c`'s live
range. Otherwise it is a copy. Liveness already computes everything this needs.

### 1.3 Drops

- **Placement.** Each owned value is freed at its last use (Perceus's "garbage-free"
  placement). Branches where a value was moved on one arm only get a drop on the other arm, as
  in Perceus. No runtime drop flags are needed, because control flow is structured and `?`,
  `return` and `break` are ordinary edges.
- **Glue.** The recursive free for a type is generated from the same `stride`/`ptr_mask`
  metadata the tracer walks today, so `gc_masks` becomes `drop_masks`. Long lists and deep trees
  are dropped with a worklist and not by recursion.
- **Extraction.** `let x = xs[i]` copies the element, as the semantics already says. If `xs` is
  dead afterwards, it becomes a partial move: take the element and drop the rest. That closes the
  "reading a leaf out of a container" gap from `MUTABILITY.md` stage 6 as a side effect.
- **Timing.** Freeing memory has no observable effect, so it happens eagerly at last use.
  Resource release (closing a file, releasing a lock) *is* observable, so it happens at scope
  end. See §3.6.

### 1.4 What is required beyond MVS + Linear

1. **Never add `Ref<T>` or any shared mutable cell.** `MUTABILITY.md` lists `Ref(T)` under
   "Deferred". It should move to "Rejected", because it is the one feature that reintroduces
   cycles and needs a collector. Graphs use `Pool` (§2.4).
2. **Escaping closures (tier 2) must own their environment.** Capture is already by value, so a
   closure becomes `(code, owned env)` with drop glue. It is still a tree and still needs no GC.
3. **A panic and cancellation memory story that needs no unwinding.** See §3.5: task-scoped
   heaps make this free.
4. **A host boundary rule.** `FrogValue` already deep-copies out. Host `Handle`s become linear or
   second-class (§3.2) instead of GC-traced opaque pointers.
5. **Copies must be visible to the programmer.** Under a GC, an implicit copy cost nothing.
   Under ownership, it is an O(n) operation the programmer may not have meant. **Decided (Q2):
   copies stay implicit, and a lint reports them.** Each residual deep copy of a heap value is a
   lint diagnostic (`implicit-copy`), off by default in the REPL. `frog check --copies` lists
   them all. The lint can be raised to an error per function or per module with an annotation
   (`#deny(implicit-copy)`). No explicit `copy()` is required. A `copy(x)` builtin exists only to
   silence the lint at a site where the copy is intended.

### 1.5 What it costs, honestly

- **Copy cost** where values really are duplicated. Today's sticky `shared` bit also copies on
  the first write after any alias, often unnecessarily, so this is mostly a wash. `Str` is the
  open risk. Measure on `orders`, `words` and `pipeline` before committing (Q1).
- **Drop cost** is O(objects) per free, where the GC's was O(live). Arenas (§2) are the answer
  for the bulk-temporary case where this matters.
- **Implementation work**: convention inference, drop insertion and drop glue. In exchange, the
  stack-map, root and COW machinery listed in §0 is deleted, and that is mostly the code that
  has produced soundness bugs.

---

## 2. Regions

### 2.1 Why regions are nearly free under MVS

Region systems are hard (Tofte–Talpin/MLKit, Cyclone, Rust lifetimes) because pointers into a
region can escape it. Froglang has no pointers to escape. The only way data leaves a region is as
a value: the block's result, a task's result, or an argument to a host call. The value is
evacuated, either copied out or allocated directly in the destination. Getting this wrong costs
a copy, never a dangling pointer.

So for *memory*, regions need **no type-level tracking**. The `CONCURRENCY.md` Rule B machinery
(region-bound types, upward contagion, retention checking) is not needed for memory. It is still
needed for non-value handles, and §3.2 replaces it there with something simpler.

### 2.2 The allocator interface

Each region is an allocator with two operations, `alloc` and `free`, plus `destroy`, which frees
everything. Every allocation goes to the **current allocator**, which is a dynamically bound
context entry like Odin's and Jai's `context.allocator`, but not user-visible by default.
`free(p)` finds its allocator from the segment header at `p & ~(SEGMENT_SIZE-1)` (mimalloc's
trick). One branch makes `free` a no-op inside a bump arena. Functions therefore do not need to
be compiled per allocator.

### 2.3 The three region shapes

**Task heap.** Every task, including `main` and every host-to-frog entry, gets a general
allocator: size classes plus free lists, like a mimalloc `mi_heap_t`, which supports both `free`
and `destroy`. Spawn arguments are moved or copied in, as Erlang does. Results are copied out at
join by the parent, so cross-heap frees never happen. When the task ends, its heap is destroyed
whole, *whatever it leaked*. Cancellation therefore needs no destructors (§3.5).

**`with Arena { body }`.** Bump allocation, frees are no-ops, and the arena is destroyed at
exit. There are two rules:

- **The body freezes outer state.** Places rooted outside the block are read-only inside it.
  This is Vale's region borrowing: the outer region is immutable for the duration. Because of
  it, nothing allocated in the arena can be stored into something that outlives the arena,
  checked statically and with no write barrier. The only way out is the block's value.

  ```
  for path in paths {
      let summary = with Arena { summarize(parse(read_file(path)!)) }   // AST freed in one store
      push(mut summaries, summary)                                       // the result lives outside
  }
  ```

  Code that wants to push to an outer list from inside an arena block pushes the block's result
  instead. The arena block then has the same shape as a function call, which is a deliberate
  property.

- **The result is allocated at its destination.** Goose's in-place construction guarantee and
  MLKit's region polymorphism point the same way. Each function takes a hidden *result
  allocator* alongside the current one. Allocation sites whose value flows to the function's
  result use the result allocator. In the block's tail position, the result allocator is the
  outer region. Value flow is a tree, so this analysis is precise up to branch merges, which
  conservatively use the result allocator. Evacuation by deep copy is the fallback, for example
  when a piece of a temporary is extracted into the result.

**The MLKit lesson.** Region inference failed in practice because loops accumulate garbage in
long-lived regions. Here the *default* is the heap with eager frees, and the arena is opt-in. The
arena's contract states the trade: "nothing is freed until exit". A loop doing `acc = acc + s`
inside an arena block is O(n²) memory by request. A lint can flag loop-carried reallocation
inside an arena.

**`Pool<T>` and `Id<T>`, for graph-shaped data.** This replaces "`List(Node)` plus `Int`
indices", the arena-and-handles pattern from `MUTABILITY.md` §6, with a typed version:

```
data Node(label: Str, edges: List<Id<Node>>)

mut g = Pool<Node>()
let a = g.add(Node(label = "a", edges = []))
let b = g.add(Node(label = "b", edges = [a]))
g[a].edges.push(b)          // a cycle; nothing is shared in the memory sense
g.remove(b)                 // frees b's slot and bumps its generation
g[a].edges[0]               // an Id still names b...
g[g[a].edges[0]]            // ...and dereferencing it panics: stale Id. `g.get(id)` returns `Node?`
```

- `Id<T>` is `(index: u32, generation: u32)`. It is plain data: `Eq`, `Hash`, `repr`/`read`
  and JSON round-trip all work, because it is position-independent. This is Goose's
  pool-relative link and the slotmap/generational-arena pattern, with Vale's generation check
  moved from every pointer onto the one indirection that needs it.
- **A pool is a value.** Copying a pool copies the graph, and every `Id` stays valid in the copy.
  A cyclic graph is still a tree of owned memory: the pool owns its nodes, and the edges are
  integers.
- `g[id]` is a *place*, so `g[id].x = 1` mutates in place through the existing stage-8 `Place`
  machinery. A general Hylo-style projection (`subscript` with `yield`) would let user types do
  the same thing (Q5).
- Removal is O(1), and a stale `Id` is detected and never memory-unsafe. Goose instead lets a
  stale read return "a different value of the correct type" to avoid generations. That is
  cheaper, but it turns a detectable bug into a silent one, so froglang should not adopt it.
- Ids are not statically branded to one pool instance. A path-dependent `Id<Node, g>` would
  catch cross-pool mixups, but it buys type-level complexity for a rare bug. An optional
  debug-mode pool tag covers it.

### 2.4 What Goose offers, and what to leave

Goose gets its performance from nested variable-size data stored *inline* (no pointer per inner
array), from N statically assigned bump stacks, and from freeing by resetting a bump pointer.

| Goose idea | Take? |
| --- | --- |
| Free by resetting a bump pointer | **Yes**, as `with Arena` and task heaps, opt-in and not universal |
| In-place construction at the destination | **Yes**, as the hidden result allocator (§2.3) |
| Pool-relative links that make structures position-independent | **Yes**, as `Id<T>` |
| References second-class, roots inferred per specialization, no annotations | **Yes**, for handles (§3.2), not for memory |
| Function values are compile-time entities | **Already done**: `lower_function_values` |
| Narrow `u8`/`u16` links | later, as an optional `Id<T, u16>` width |
| At most one resizable value per data stack, kept on top, with static stack assignment | **No.** It forbids keeping growable data across a recursive call, and a froglang compiler written in froglang is recursive throughout |
| Variable-size arrays iterable but not indexable; variable-mode enums | **No**: too many layout-dependent rules visible in the language |
| Type-safe stale reads instead of generations | **No** (see above) |
| `return err from f` non-local returns | **No**: `?` covers it |

The underlying lesson from Goose is about layout: fewer pointers and less per-object teardown.
Froglang already flattens structs and strides lists. Inline small strings, for `Str` of at most
15 bytes in two words, are the next layout win, and they also shrink the residual-copy problem in
§1.5.

---

## 3. `with`, scopes, and capabilities: one tree

### 3.1 The design today (`CONCURRENCY.md`), in brief

- `with E as x { body }` calls `enter`, binds `x`, and runs `exit(outcome)` on every path.
- `Scope` is a library type whose only parameter is a supervisor fold over child outcomes.
  `Deadline`, `Shield` and `Limit` are separate `with` contexts.
- Rule A: `spawn` needs a `Spawn` capability. `provides`/`can`/`without`, with `Fs`/`Net` granted
  to `main` by the host and narrowed downward.
- Rule B: region-bound types, so a `Task`, file or lock cannot escape its `with`.
- Unwinding is the mechanism for cancellation and panic, and it needs JIT unwind support and
  per-frame cleanup.
- A single GC heap makes the runtime M:1.

### 3.2 The observation

Every one of those features is **a lifetime nested inside its parent's lifetime**:

- A task lives inside its scope, which lives inside the enclosing function.
- An arena lives inside its block.
- A file handle lives inside its `with`, and a deadline inside its `with`.
- A capability grant lives inside its `with`, and a `without` inside its block.

Because they all nest, there is one dynamic tree, and every feature is a node kind in it.
Memory regions follow the tree: a task's heap is a child of the arena it was spawned in, so an
arena can bulk-free its tasks' memory too. Failure follows the tree as well: a task boundary
stops unwinding. So do authority (capabilities only narrow going down) and cleanup (exits run
leaf-to-root). Structured concurrency is the case that forced the tree into view, but the tree
is general.

### 3.3 The construct kit

There are five constructs. Everything else is a library type built from them.

1. **Values.** MVS, owned, moved or copied. Every type is a value by default.
2. **Linear values** (`provides Linear`). These must be consumed *exactly once*, by a function
   that takes them and may return something: Vale's Higher RAII. Examples are a transaction
   (`commit` or `rollback`, never implicit), `StrBuf` for `DATA.md`'s `Sink` (`finish` returns
   the `Str`), and an owned socket. They are first-class: storable and returnable. On
   *failure* paths, consumption is automatic through the `Abort` trait (§3.8), so a linear value
   is linear on success paths only.
3. **Second-class handles.** A handle can be passed *down* as an argument and used, but cannot be
   stored in a value, returned, or captured by an escaping closure. This follows Osvald & Rompf's
   second-class values and Zest's second-class references, and Goose's root inference does the
   same job. One syntactic, local rule **replaces Rule B entirely**: there is no region
   contagion and no retention analysis. Whatever `with ... as h` binds is second-class, so the
   handle cannot outlive its block, by construction.
4. **`with`**, the only construct that adds a node to the tree:

   ```
   trait Context {
       type Handle          // what `as h` binds: second-class
       type Out             // the value of the whole `with` expression
       func enter(mut c: Self): Handle
       func exit(c: Self, body: Outcome<Body>): Out      // runs on every path, including unwind
   }
   ```

   A context also declares two static properties:
   - `provides Cap, ...`: capabilities the body may use. Rule A, unchanged.
   - `freezes`: outer places are read-only in the body (§2.3). Some contexts need this to be
     sound: `Arena`, and `Parallel` below.
5. **Capabilities** (`can`). These are the static view of the tree: what a function needs some
   enclosing node to provide. They are inferred inside a module and written at API boundaries,
   as already designed.

### 3.4 Each feature in the kit

| Feature | Expressed as |
| --- | --- |
| Structured concurrency | `with Scope(all) { spawn f(x) }`, which provides `Spawn`; `exit` is the supervisor fold |
| Task results | **Decided (Q7): there is no `Task<T>` type.** Results come out in one of two ways. A homogeneous batch comes out through the supervisor's accumulator, which is the value of the `with Scope(...)` expression. A heterogeneous, named result comes out through a *deferred-init place*: `let u: User; with Scope(all) { u = spawn fetch(id) }`. Here `spawn` writes into a place the parent owns, and definite-initialization analysis treats `u` as initialized only after the block. `spawn e` is a statement-like form of type `None`, or the right-hand side of an assignment to a deferred place. It produces no value that could escape. This removes the last reason for Rule B |
| Arena | `with Arena { e }`: freezes outer state, rebinds the current allocator, `exit` evacuates and destroys |
| Data parallelism | `with Parallel(all) { for c in chunks(data, 1000) do spawn sum(c) }`: freezes outer state, so children *borrow* `data` across threads with no copy, no RC and no atomics. This is Vale's "seamless concurrency" |
| File or socket scope | `with open(p) as f { ... }`; or just `let f = open(p)?`, closed by drop at scope end (§3.6) |
| Transaction | `with db.begin() as tx { ... }`: `exit` commits on `Ok` and rolls back on `Err`, `Panicked` or `Cancelled` |
| Deadline, Shield, Limit | contexts with no handle; they modify the dynamic tree |
| Narrowing authority | `without Net { }` and `with Fs(read_only = "/tmp") { }` |
| `defer e` | `with OnExit(e) { rest of block }` |
| I/O implementation | a context entry (`with TestIo(seed) { }`), as already designed |

A `with` header can stack contexts: `with Deadline(5.s), Arena, Scope(race) { ... }` is three
nested nodes, outermost first. Order is meaningful, and the meaning is what nesting would give,
so the multi-context form needs no rule of its own.

### 3.5 Failure without landing pads

The tree makes unwinding cheap. Each task keeps a **cleanup stack** of `with` records, pushed at
`enter` and popped at normal `exit`. A panic or cancellation:

1. walks the cleanup stack, calling each `exit(..., Panicked | Cancelled)` on top of the current
   stack (the frames are still intact, so this is safe);
2. destroys the task heap and any arenas, so every value owned by any frame of the task is freed
   at once, with no per-frame drops;
3. switches to the parent fiber, which receives `Outcome.Panicked` / `Cancelled`.

Values carry no unwind obligations. Memory goes with the heap. Resources that need cleanup are
either registered by a `with`, or are host resources held in the task's **resource table**
(§3.8). **This removes the need for Cranelift exception tables, per-frame cleanup blocks and an
unwinder**, which `CONCURRENCY.md` listed as "must land before the scheduler is useful". It also
works the same in emitted C, through `setjmp` at the task root or a fiber switch.

The root task in a standalone program exits the process. A host-to-frog call is a task, so a
panic returns an error to the host and frees everything the call allocated.

### 3.6 Drop timing

| Kind | Dropped when | Why |
| --- | --- | --- |
| Memory-only values | eagerly, at last use | unobservable, so it should be as early as possible |
| Types `provides Drop` (affine resources: files) | at scope end | observable, so it should be predictable, like Rust |
| `Linear` | never implicitly on success paths; failing to consume is a compile error | the protocol is the point |
| `provides Abort`, on a `?` edge | `abort(x)` runs **instead of** drop | §3.8 |
| Anything, on panic or cancel | heap destroyed; `with` exits run; host resources' `abort` runs from the resource table | §3.5, §3.8 |

So `Linear` keeps its current checker (`frontend/linear.rs`), tightened from "consumed on every
branch or none" to "consumed on every *success* branch". A `?` edge counts as consuming through
`abort` (§3.8). `Drop` is the affine counterpart, and plain values need neither.

### 3.7 Concurrency consequences

- **M:N stops being a semantic question.** Tasks own disjoint heaps, and the only cross-task
  sharing is frozen, read-only borrows. No memory model, atomics or safepoints are needed, and
  there is no GC to stop the world. `CONCURRENCY.md`'s M:1 decision was mostly the GC's
  consequence. It can stay as the *first* implementation without being the design.
- "Tasks interleave only at suspension points" becomes moot under MVS. It was already moot
  (`MUTABILITY.md`), and per-task heaps make it structural.
- Spawn copies or moves its arguments into the child heap unless the enclosing context freezes
  them, in which case children borrow.

### 3.8 `Abort`: what `?` does to live values

**The problem**, found while writing `plans/nogc_sample.frog`: `?` is an early return. If a
`Linear` value is live at a `?`, that edge leaves it unconsumed, so the checker rejects the `?`.
Every function holding a linear resource then falls back to spelled-out handlers:

```
out.write(text) catch [e] -> { abort(out); return e }      // where `?` should have worked
```

That gives up exactly the "minimal error-handling syntax" froglang wants to keep, and gives it up
precisely on resource-heavy code.

**The rule.** `Abort` is an ordinary trait with one member:

```
trait Abort {
    func abort(x: Self)          // consumes x; returns nothing; must not fail
}
```

On every **error-propagation edge** (a `?` that fires), each live value whose type
`provides Abort` gets `abort(x)` called on it, in reverse order of acquisition, before the error
is returned. For those values, `abort` takes the place of drop on that edge. Values without
`Abort` are dropped as usual. Typeck desugars `?` into a `match` (`ERRORS.md`), and the
error arm of that `match` is exactly where the calls go. Liveness already knows the live set at
that point, so this is a typeck-level desugar, and codegen needs nothing new.

Consequences:

- **`Linear` requires `Abort`.** `data T provides Linear` without an `abort` member is an error,
  because a linear value with no failure path cannot survive a panic or cancellation either.
  `linear.rs` treats a `?` edge as consuming every live `Linear` value through `abort`, so `?`
  just works:

  ```
  let out = AtomicFile.create(path)?
  out.write(text)?               // fires: abort(out) deletes the tmp file, then returns the error
  commit(out)                    // success path: still must be consumed explicitly
  ```

- **`Abort` also works on non-linear types**, as "on error, undo; on success, keep". A
  `TempDir` that is kept on success can be deleted on failure. So can a partially built output
  file, or a staging row in a database. `Drop` is "always clean up", and `Abort` is "clean up
  differently if we're failing". A type can provide both: drop on success paths, abort on `?`
  edges.
- **Which edges count.** `?` that fires, `!` (it panics; see below), and cancellation are error
  edges. A plain `return` is *not*, even of an `Error`-typed value: an explicit `return` is
  ordinary control flow, so linear values must be consumed explicitly before it. `catch` does not
  exit the scope, so it aborts nothing. The rule is narrow on purpose: `?` is the one place where
  froglang decided an early exit should be invisible, so it is the one place where cleanup is
  implicit.
- **Relation to `with`.** `with` is the scoped form: `exit(c, outcome)` sees the outcome and can
  commit or roll back. `Abort` is the value form, for resources that move between functions
  without a lexical block. Both run on the same edges, and a transaction can be either. For
  example, `with db.begin() as tx { ... }` behaves the same as
  `let tx = db.begin()?; ...?; commit(tx)` where the transaction provides `Linear, Abort`.

**Panic and cancellation.** Here there is no static edge to put calls on (§3.5 runs no per-frame
code). Instead, each task has a **resource table**: host-backed resources (fds, sockets, temp
paths, foreign handles) are entered on creation and removed when consumed (`commit`, `close`,
`abort`, drop). On panic or cancel, the runtime calls the host-level `abort`/`drop` for every
entry still in the table, then destroys the heap. Resources are expensive anyway, so an O(1)
table entry per resource is free in comparison, and fds are never leaked by a dying task.

The trade, stated plainly: on panic, **only host-level abort logic runs**. A user-defined
`Abort` on a pure frog type, for example one that logs, does not run, though the host resources
it wraps are still cleaned up. Running user `abort` bodies on panic would require registering
every `Abort` value, user types included, and tracking it through every move between frames.
That is the dynamic ownership bookkeeping this design exists to avoid. Panics are bug paths;
`?` is the designed failure path. (Q9)

### 3.9 Smaller consequences surfaced by the sample program

- **Freezing is transitive through calls.** A function called from inside a `freezes` block may
  not mutate places rooted outside it. Whole-program compilation makes this free: a `mut`
  argument whose place is frozen is rejected at the call site, and a function cannot reach outer
  state any other way, because no globals are mutable and nothing has references. This is
  already implied by MVS and only needs to be stated.
- **Partial moves.** `build_graph(got.oks)` followed by `got.errs.len()` moves one field out of a
  struct and keeps using another. Liveness becomes per-field-path for struct-typed bindings, the
  same `Place { root, path }` shape stage 8 already uses for mutation. The remaining fields are
  dropped at their own last uses.
- **Deferred-init `let`.** `let robots: Str | CrawlError` with no initializer is new syntax,
  needed now that `Task<T>` is gone (§3.4). It requires a type annotation and is assigned exactly
  once on every path before any read: the standard definite-assignment analysis, run inside
  `linear.rs`'s walk, which already tracks per-branch consumption.

---

## 4. Borrowed from, and left with, the reference languages

| | Take | Leave |
| --- | --- | --- |
| **Hylo** | no references at all; `let` as borrow; `sink`; projections (`subscript`/`yield`) as the general form of stage-8 places; deterministic deinit without RC | writing conventions on every parameter (infer them instead); mandatory explicit `copy()` (make it a lint, Q2) |
| **Vale** | Higher RAII (`Linear` with argument-taking destructors); region borrowing / freezing; seamless parallelism over frozen data; generations, but only inside `Pool` | generational references on every pointer, because froglang has no pointers |
| **Goose** | bump-reset free; in-place result construction; position-independent pool links; second-class, annotation-free roots; compile-time function values | static data-stack assignment and its recursion restrictions; layout-visible array kinds; type-safe stale reads |
| **Zest** | second-class mutable references as the handle model; "fall back to RC+COW if the type system fails" as a *measured* escape hatch | — |
| **Koka/Roc/Lean** | borrow inference; drop at last use; a uniqueness-based mutation fast path | RC as the default representation |
| **MLKit** | region polymorphism as a hidden result allocator | inferring regions everywhere, which causes the loop region leaks |
| **Erlang** | per-process heaps; copy at the boundary; a dead process frees everything | — |

---

## 5. Open questions (for discussion before any code)

- **Q1. Deep copy versus RC for `Str` (and maybe everything).** **Status: leaning, not decided.**
  Measured in §7. For `List`, deep copy with inferred borrowing removes essentially all copying in
  the benchmarks. For `Str`, the only residual copies are 320,000 small copies of extracted list
  elements in `words`. Their bytes are negligible, so the open question is allocator cost, which
  only timing can answer. Recommendation: keep deep copy as the model, and consider `Str`-only RC
  if timing shows the extraction copies matter.
- **Q2. How visible should copies be?** **Decided: implicit, reported by a lint** that can be
  denied per function or module (§1.4 item 5). Explicit `copy()` stays possible later if the lint
  shows copies are a common surprise.
- **Q3. Is `freezes` too strict for `Arena`?** **Decided: keep the strict rule.** No write
  barrier. The arena block's value is the only exit, and freezing is transitive through calls
  (§3.9).
- **Q4. Spawn under a non-freezing `Scope`: copy or move the arguments?** Erlang copies. Moving
  is free, but the parent loses the value. The likely answer is to move on last use and copy
  otherwise, the same rule as consume parameters.
- **Q5. General projections.** Should `subscript` with `yield` be user-definable so that
  `Pool`, `Dict` and user containers expose places, or should places stay
  builtin-container-only?
- **Q6. The REPL and top-level bindings.** They live in the `FrogState` heap. A rebinding at a
  later prompt drops the old value, but drops need the "last use" that the REPL can never know,
  so top-level values are dropped only on rebinding or on state teardown. This resolves the
  `linear.rs` top-level gap the same way.
- **Q7. Does `Task<T>` survive?** **Decided: no.** Supervisor accumulators and deferred-init
  places cover every use (§3.4), and Rule B goes with it. Revisit only if `select` over
  in-flight tasks turns out to need a handle. If it does, that handle will be second-class.
- **Q8. Embedding.** Should the host be able to pass an arena into a call (`state.call_in(arena,
  ...)`) for request-scoped work, so the host owns bulk free? This is cheap to expose and fits
  game and server embedders.
- **Q9. User `abort` bodies on panic.** §3.8 runs only host-level aborts on panic and cancel.
  Is that asymmetry acceptable, or should `Abort` values be registered dynamically? A middle
  option is to register them only when their `abort` has a declared effect (`can Fs`, `can
  Net`), since a pure abort has nothing observable to do once the heap is gone anyway.
- **Q10. Can `abort` see the error?** `abort(x: Self)` cannot tell a cancelled operation from a
  validation failure. `abort(x: Self, why: Error)` costs nothing and helps logging, but it
  invites abort logic that branches on error types. Start without it.

## 6. A possible staging (not a commitment)

0. **Measure.** **Done** (§7). Instrumentation is committed on `nogc` as `e6f867d`. It counts the
   copies unique ownership would add, split by the flow the value goes into, for both `Var` reads
   and list/dict extractions. It is gated by `FROG_COPY_STATS`.
1. **Convention inference** (borrow/consume) in the frontend, with a `FROG_DUMP_CONVENTIONS`
   dump to check it. It is useful even with the GC, because it removes `mark_shared` calls.
   **Analysis done, not committed** (`frontend/conventions.rs`, `tests/test_conventions.rs`).
   Codegen does not use it yet; it only reports the `call-arg-borrow` site under `FROG_COPY_STATS`.
2. **Ownership codegen behind a flag:** drops at last use, drop glue from masks, deep copy at
   residual copies, no `shared` bit. Run the whole suite in both modes, with a leak checker
   (allocation count at exit is zero) as the oracle that replaces `FROG_GC_STRESS`.
   **Status: first slice done** (`FROG_OWNERSHIP`). Every site that marked a list or
   dict leaf `shared` (`Var` reads, extractions, for-loop elements) now deep-copies it, unless the
   read is a borrowed call argument or a transient target. The `shared` bit is never set in this
   mode. `values(d)` deep-copies its result. The GC still runs and frees nothing explicitly, so
   drops and the leak checker are not done yet. Both modes pass the suite (1192 tests). Under
   `FROG_OWNERSHIP` the `life` benchmark matches default mode, and so do `orders` and `words`.
   `Str` is deep-copied too, as the plan's Q1 default (`frog_clone_owned`). `keys(d)` and `values(d)`
   copy their result, and `words` costs about 8% more for it.
   The COW verifier does not catch a missing copy in `let b = a`, because both bindings share one
   SSA value. The value-semantics tests do catch it: skipping the copy fails 18 of them.

   **Status: second half done.** Drops and frees are in place under `FROG_OWNERSHIP`.
   - Every owned heap value lives in a slot. A block frame holds bindings, and a statement frame
     holds the temporaries it produced. A consumer that keeps a value takes it out of its frame,
     which clears the slot. Popping a frame frees whatever its slots still hold. An early `return`
     drops every open frame. A `Move` read clears the binding it leaves. Dropping a slot that holds 0
     is a no-op, so this needs no static drop flags.
   - Runtime: `frog_drop` frees an object and everything it owns (`GcHeap::drop_tree`, using the
     same pointer masks as the collector). `frog_drop_shallow` frees a list shell. The collector is
     off under ownership. `FROG_LEAK_CHECK` makes a second free panic, and reports at teardown any
     object still live. `FrogState` frees its environment on drop.
   - Results: 1192 suite tests plus 9 new ones in `tests/test_ownership.rs` pass in both modes, with
     zero leak reports and zero double frees. A sabotage check (drops disabled) makes the leak
     report appear, so the oracle is live. Both modes also pass the AOT build tests.
   - Deviations from the plan: drops happen at scope and statement end, not at each value's last
     use, so memory is held a little longer than the plan's model. Static drop placement from
     `dead_after` is not implemented. Calls take ownership by the callee's convention, and that
     convention is keyed by the body node. A callee from an earlier REPL entry is not known to the
     caller. The caller treats it as consuming, so the callee frees nothing it borrowed, which leaks
     rather than crashes.
   - Performance (release, best of 5): see "Tuning `orders`" below for the current numbers. `life`
     matches default and `words` is about 10% slower under ownership.
   - Known, not fixed: default mode (GC) still has the `values(d)` alias that ownership mode fixes.
     Collector unit tests and the GC-stress churn test now run only on a collected heap (`owned`
     is set false, or the sweep check is skipped).
   **Tuning `orders` (2026-11-05, committed).** Times are best of 5 in release.

   | Build | default | ownership |
   |---|---|---|
   | before tuning | 49 ms | 80 ms |
   | step 1: inline immediate check before clone calls (committed, `e9c4cd6`) | 49 ms | 73 ms |
   | item 1: drop the drop of a temporary taken at its own depth | 49 ms | 59 ms |

   Measurements behind the tuning:
   - Runtime counts on `orders` (before step 1): 2,000 `frog_drop` calls reach the runtime, since
     the inline heap test already filters the rest. `frog_clone_owned` runs 5,333,334 times, and
     every call receives an immediate. The copies were wasted calls, not work. The inline test
     removed them all.
   - Code size, per generated function (`FROG_DUMP_CLIF`): `modn`, `hash`, `category_of`,
     `discount_for` and `apply` compile identically in both modes. `checked_gross`,
     `price_item` and the loop body (`__frog_main_3`) grew: 14 → 106, 23 → 317 and 231 → 602
     instructions (blocks 4 → 14, 4 → 32 and 24 → 52). Each registered temporary got a drop guard
     at its frame's end, and each guard splits a block.
   - Attribution: sampled with macOS `sample` on a 40,000-round copy of `orders`. Symbols came
     from `FROG_JIT_SYMBOLS`, since the JIT code is anonymous. Each sample address was mapped to
     its function by the symbol file's start addresses, where length 0 means "until the next
     symbol". Shares of samples, default → ownership, before item 1: `checked_gross` 9.9% → 21.7%,
     `price_item` 7.4% → 23.4%, the loop body 13.1% → 22.7%, and the three unchanged functions
     together 53.7% → 32.4%.

   Item 1 (what it changed): a temporary registered at branch depth d, and taken at depth d, is
   taken on every path to its frame's end, since the depth counts only enclosing conditionals and
   loops. So its slot is removed from the frame and no drop is emitted. An early `return` still
   drops it while it is registered, which is right. Results: `checked_gross` 106 → 19
   instructions, with no drop sites. `price_item` 317 → 225, with 12 → 7 drop sites. The loop
   body 602 → 512, with 12 → 8. Re-sampled, `checked_gross` falls to 4.3% of samples (default
   10.2%). `price_item` is 19.4% (default 7.2%), and the loop body is 26.1% (default 15.9%).

   What remains, to tune next: the drops left in `price_item` and the loop body guard union
   results that can hold a `PricingError`. They look necessary, since the union's pointer column
   may hold an error object. Two smaller ideas follow. `is_heap_ty` treats every union as heap,
   including an inline union whose members are all scalars, which is correct only because the
   pointer columns of such a union don't exist. A stricter per-union test could avoid a few
   registrations. And several guards in one statement could share one block, to cut block
   splits.

   **Copy on keep (2026-11-05).** Codegen used to copy a value when it was read,
   unless the reading site marked itself as a view (`view_next`, `compile_operand`, the transient
   helpers). Every read-only site that was not marked made a deep copy, and several were not:
   `if xs` copied the list to read its length, `d[k]` copied the key, `Narrow` copied the payload,
   and a loop body whose tail was `xs = Cons(t=xs)` copied the whole list on every iteration, then
   dropped the copy. That last one was quadratic: 4,000 elements took 6.4 s in debug, against
   0.01 s under the GC.
   - Now a read is free. A compiled heap leaf is an owned temporary (registered with its statement)
     or a view (a binding, an element, a field, a payload, or an assignment's value). Only a
     consumer that keeps a value calls `own_vals`, which copies views and leaves temporaries alone,
     and then `take_vals` once it has stored the value. A view must not outlive what it reads, so a
     value leaving a frame is owned first: a block's tail, a conditional's branch, a function's
     result, the entry's result. The exception is a value nobody uses: `discard_next` now marks
     every non-tail statement and a discarding loop's body, and a discarded block or conditional
     owns nothing. A struct literal passes its fields' leaves through unchanged, since ownership is
     per leaf.
   - Removed: `view_next`, `compile_operand(_multi)`, `copy_owned`, the `transient` parameter of
     `compile_expr_multi_inner`, and the `Assign`-specific discard handling. The entry now gives
     each top-level statement its own frame, like a block.
   - Results (debug): the cons loop, `if xs` in a loop, and the `d[k]` loop now run within 2× of
     the GC (`if xs`: 31 s → 0.02 s). Release, best of 5: `orders` 40 ms default, 50 ms ownership.
     `words` 150 → 170 ms, `life` equal.
   - Known limit: a view taken by an operator's operand stays valid only until something writes to
     what it reads. A later operand of the same expression that writes there (`xs[0] + { xs[0] =
     "z"; "" }`) leaves the earlier operand dangling. Rebinding a name is safe, since liveness
     then makes the earlier read a `Move`.

   **Worklists and leak checking (2026-11-06).**
   - `drop_tree` and `clone_obj_with` recursed once per level, so dropping or copying a ~20,000-deep
     `Cons` list overflowed the stack. That is the ownership free path itself, so removing the GC
     would not have fixed it. Both now walk a worklist over `for_each_slot`, the slot iterator the
     collector's `count_refs` already used: `drop_tree` pushes each object's children and frees it,
     and `clone_obj_with` makes a `shallow_copy` of each object and then re-points its copy's slots
     at shallow copies of their own. A 1,000,000-deep list now copies and frees in both modes.
   - The 67 leak-check failures had one cause. Runtime primitives that `desugar_notation` and the
     `read`/`json` lowerings call by name (`__repr_str`, `__str_join`, `frog_read_*`,
     `frog_json_*`, ...) were not borrowers in `conventions::infer`, so their arguments were handed
     over and never freed. `Codegen::imported_callees` now lists every name that resolves to an
     imported symbol, runtime primitive or host shim, and `infer` treats each as a borrower. The list
     comes from the module's declarations, so it cannot drift from `declare_runtime_imports`.
   - How leaks are caught now:
     - `FrogState`'s teardown panics on a leak under `FROG_LEAK_CHECK`, so a leaking run exits
       nonzero and a leaking in-process test fails.
     - `tests/common`'s `run` (about 360 programs) runs every program twice, as configured and under
       `FROG_OWNERSHIP=1 FROG_LEAK_CHECK=1`, and requires the same output. A plain `cargo test`
       therefore checks for leaks and for differences between the two modes.
     - `codegen::compile_and_run`, the in-process test entry point, now runs the same conventions
       pass as `FrogState`, frees its bindings under ownership, and fails if anything other than the
       returned result is still live.
     - The full gate is `FROG_OWNERSHIP=1 FROG_LEAK_CHECK=1 cargo test -p froglang-core`, which also
       runs the in-process (`compile_and_run`, `FrogState::eval`) tests under ownership. It passes, as
       does the default run.

3. **Delete the GC:** stack maps, roots, COW, `RuntimeRoots`, and the AOT stack-map table.
   `SELFHOST.md` G6 disappears.

   **Scope (2026-11-06).** Ownership becomes the only mode, and then everything that exists only
   for the collector goes. The deliverable is less code. Do it in three slices, with the suite
   green after each.

   - **3a. One mode.**
     - Remove `FROG_OWNERSHIP` and its three readers:
       - `codegen::ownership_enabled`;
       - the `owned` field set in `GcHeap::new`;
       - the env check in `state.rs`.
     - Codegen always emits the ownership path. Delete the GC-mode branches: `mark_shared` calls,
       the `shared` bit, the COW checks before writes, and `CopySite` copies made only for stats.
     - The heap no longer needs an `owned` flag.
     - `tests/common::run` drops its second run, since there is only one mode.
     - Delete `run_gc_stress` and `run_cow_verify`, and the tests that only exercise
       collector or COW behavior.
     - Keep tests that check value semantics through the language.
   - **3b. No collector.** Delete:
     - mark/sweep, `mark_worklist`, `count_refs`, and the collection trigger in the allocator;
     - shadow frames and `push_root`/`pop_root`;
     - Cranelift stack maps and their safepoint bookkeeping;
     - `RuntimeRoots`, and the AOT stack-map table;
     - the `GC_HEAP` and `ACTIVE_HEAP` rooting the collector needed.

     Also:
     - `clone_obj_with` no longer roots its top copy.
     - `GcHeap` stays as the allocator, holding free lists, live accounting and `drop_tree`.
       Renaming it is optional and belongs in a separate commit.
     - Host marshalling (`ffi.rs`, `host.rs`, `froglang-macros`) and `FrogState` bindings must
       own or borrow explicitly, with no roots. A value handed to the host is either copied out
       or transferred, and the plan does not choose between them: pick whichever is simpler and
       say which one.
   - **3c. Close the known leaks** that the GC used to hide:
     - **Dict literal with a duplicate key:** the overwritten value is never freed.
     - **REPL cross-entry calls:** a callee compiled in an earlier `FrogState::eval` entry is
       treated as consuming by the caller, so its borrowed arguments leak. Keep each entry's
       inferred conventions in `FrogState`, keyed by function name, and feed them to `infer`
       for later entries.
   - **Leak checking after the change.**
     - Keep the live-object count unconditionally, since it is cheap.
     - Turn the teardown leak check on by default in debug builds (`cfg(debug_assertions)`),
       so plain `cargo test` covers the in-process tests too.
     - Double-free detection may stay behind `FROG_LEAK_CHECK` if it costs a set lookup.
   - **Acceptance.**
     - `cargo test` (workspace) and `cargo clippy` pass with no new warnings.
     - The AOT build tests pass. Run `cargo build --lib` first; see the stale staticlib note.
     - `orders`, `words` and `life` in release are no slower than the ownership column above:
       48/54, 157/175 and 14/15 ms. They should be faster, since frames and safepoints go.
     - Measure the net line count of the deletion and record it here.
   - **Out of scope.** Static drop placement at last use, the per-union heap test, sharing guard
     blocks, the operand-view limitation, and RC for `Str`.

   **Status (2026-11-06): done.** Ownership is the only mode and the collector is
   gone. Net change, excluding `plans/`: 651 lines added, 3,238 deleted (−2,587). Source alone
   is −2,149, tests and benches −431, the README −7.
   - **3a, one mode.** `FROG_OWNERSHIP` and its three readers are gone, and so are the GC-mode
     branches. That removed `mark_shared_if_aliased`, `share_extracted`, `emit_unshare(_nested)`,
     the `shared` header bit, `FROG_COW_VERIFY` (`frog_cow_verify`, `count_refs`), and the
     `FROG_COPY_STATS` machinery (`frog_copy_stat`, `liveness::CopySite`, the site constants). The
     place walk (`emit_place_ref`, `emit_mutable_container`) no longer copies or writes back
     anything. There is one clone, `frog_clone`, and it copies `Str`s too, since `clone_obj` lost
     its `copy_str` switch. The GC-mode copy-out and return paths are gone, so `mut_param_copyout`
     is the taking version. Conventions now always run. `tests/common::run` runs each program
     once. `run_gc_stress`, `run_cow_verify` and `test_gc_roots.rs` are deleted, along with the
     collector unit tests and the in-process `FROG_GC_STRESS` tests. A test whose only assertion
     ran under stress became a plain `run` test.
   - **3b, no collector.** Deleted: mark/sweep (`collect`, `mark_from`, `sweep`, the root
     walks), the threshold and `maybe_collect`/`force_collect`; `push_root`/`pop_roots`, scanned
     spans and `RuntimeRoots`; `JitFrameGuard`, `jit_frame_guard!`, `caller_frame_pointer!`;
     every `declare_gc_*` call, `take_stack_maps`, `JIT_CODE`/`JitCode`, and the AOT stack-map
     registry with `frog_register_stackmaps`. `preserve_frame_pointers` (JIT and AOT) and
     `.cargo/config.toml`'s `force-frame-pointers` existed only for the stack walk, so they went
     too. `tests/test_cranelift_stack_maps.rs` is deleted. `GcHeader` is just `kind` now: no
     `next`, `marked` or `shared`. So a `FrogStr` header is 8 bytes and a `FrogVariant` 24, down
     from 16 and 32. `GcHeap` keeps the free lists, live accounting, `drop_tree` and
     `clone_obj`, and one worklist shared by the tree walks. The name stays for now. `GC_HEAP` and
     `ACTIVE_HEAP` stay too: they choose which heap a runtime call allocates into, and they were
     never roots. `gc_dump()` and the REPL's `:gc` print the live object count and bytes, since
     there is no object list to walk any more.
   - **Host marshalling: arguments are borrowed, results are transferred.** A host function
     reads its arguments in place and copies them into Rust values (`FromFrog`). The caller keeps
     them and frees them, because `conventions::infer` treats every imported callee as a
     borrower. The result is built from fresh objects through `FrogCtx` and written to `out`, and
     the caller owns whatever it finds there, as for any other call. `FrogCtx::scope`,
     `HostRootScope` and the shim's argument rooting are deleted. `FrogValue` copies out of the
     heap, and `eval` frees the heap's copy of the result. Nothing new was needed: this is what
     ownership mode already did, minus the roots.
   - **3c, the known leaks.** A dict literal with a repeated key now frees the value it replaces.
     Every pair after the first loads the entry's pointer leaves and drops them before storing,
     and a new entry's slots hold 0. `FrogState` keeps `signatures` (`conventions::Signatures`,
     name to per-parameter conventions) and passes them to `infer` as `prior`. So a function from
     an earlier REPL entry is called with the conventions it was compiled with. A name defined
     twice in one entry is dropped from the map, and calls to it consume.
   - **Found on the way.** `keys(d)`/`values(d)` copied the result only when the element had a
     `Str`, `List` or `Dict` leaf. A boxed-union value was shared with the dict and freed twice,
     and HEAD aborts with a double free on `values` of a `Dict<Str, T>` where `T` has seven
     members. It now tests `is_heap_ty`, as `slice` does.
   - **Leak checking.** The teardown check (`FrogState`'s drop and `compile_and_run`) is on in
     every debug build. Double-free tracking costs a set lookup per allocation, so it stays
     behind `FROG_LEAK_CHECK` and adds to the teardown check in release. Sabotage check: skipping
     the free of strings shorter than 8 bytes makes plain `cargo test --workspace --no-fail-fast`
     fail 279 tests in 33 binaries, the in-process `test_run` and `test_state` among them.
   - **Results.** `cargo test --workspace`: 1,198 passed, 0 failed, 2 ignored, both plain and
     under `FROG_LEAK_CHECK=1`. That count includes the AOT build tests (libraries rebuilt first)
     and three new tests: the repeated dict key, `values` of boxed unions, and a cross-entry
     borrow. `cargo clippy --workspace --all-targets` has 18 warnings, against 23 at HEAD, and
     none of them are new.
   - **Performance.** Release, best of 7, wall clock including JIT compilation:

     | Benchmark | HEAD, GC | HEAD, ownership | step 3 |
     |---|---|---|---|
     | `orders` | 48 ms | 54 ms | 52 ms |
     | `words` | 157 ms | 176 ms | 147 ms |
     | `life` | 15 ms | 15 ms | 15 ms |

     `words` beats the GC now, likely from the smaller headers and from losing the frame
     pointers and spills. `orders` is still 4 ms slower than the GC. Its remaining drop guards
     are the per-union heap test and guard-sharing items above.
   - **Renamed (2026-11-10).** `GcHeap` is `Heap`, `gc.rs` is `heap.rs`, `GcHeader` is `ObjHeader`,
     `GC_HEAP` is `TASK_HEAP`, `gc_mask` is `ptr_mask_of`, `gc_dump` is `heap_dump`, the `gc_trace`
     feature is `heap_trace`, the REPL command `:gc` is `:heap`, and the `gc_pressure` workload is
     `alloc_pressure`.
   - **Left open.** JIT frames no longer carry
     frame pointers, so a native profiler's stack attribution may get worse.
     `FROG_JIT_SYMBOLS` is unaffected. The `pipeline.rs` workload `gc_pressure` keeps its name
     but no longer measures a collector (since renamed). `SELFHOST.md` G6 has not been edited.
4. **Allocator layer:** segment headers, task heap and bump arena; then `with Arena` with
   `freezes`, and the result allocator.

   **Scope (2026-11-08).** Step 3 left one task heap (`GcHeap`: size-class free lists over the
   system allocator). Step 4 adds the first region, `with Arena`, and nothing else. The task heap
   per task, segment headers and the result allocator wait for tasks and measurements.

   *Why no segment headers.* §2.2 needs `free(p)` to find its allocator from `p`. That is only
   needed if a free can meet a pointer from either allocator. The `freezes` rule rules it out:
   inside an arena block no outer place is written, moved from, or passed `mut`, so every free
   there targets an arena object. While an arena is active, `free` is a no-op, whatever the
   pointer. This is less code, and it is the property the design already guarantees.

   - **4a. Runtime.** `GcHeap` gets a stack of bump arenas (chunked; a chunk grows by doubling,
     and an oversized object gets its own chunk). `alloc_bytes` bumps from the top arena when
     there is one. `free_bytes`, `drop_tree` and `free_shallow` return at once while one is
     active, which makes drops in a block free. Entry points `frog_arena_enter()` and
     `frog_arena_exit(result_word, tymask...)`: exit deep-copies the result into the enclosing
     allocator (the next arena down, or the heap) using the existing `clone_obj` walk with the
     arena popped first, then releases the chunks in one go. The live-object count must stay
     right: objects allocated in an arena are subtracted at exit, so the leak oracle still
     proves everything outside was freed. Unit tests in Rust: allocate and drop in an arena,
     nested arenas, an oversized object, evacuation of a nested list/dict/union/`Str`, and the
     oracle across enter/exit.
   - **4b. Language.** `with Arena { body }` is an expression of the body's type. Parse it in
     the existing expression grammar (`with` is a keyword now; check the lexer and that no
     test program uses it as an identifier). In typeck, add the `freezes` check. Inside the
     body, a place rooted at a binding made outside it may only be read: assignment, `push`
     and the other mutators, `mut` arguments, and moves out of it (a `Move` read, a consuming
     call argument) are errors, and the message says to copy it or push the block's result.
     Reads that copy into the arena are fine, since copy-on-keep already does that. Bindings made
     inside the block are unrestricted. Reuse the places and conventions machinery; do not
     invent a second analysis.
   - **4c. Codegen.** Enter, body, exit, and the result is the evacuated word. A `return`, `?`
     or `break` that leaves the block must exit the arena on the way out, evacuating its value.
     If the frame-popping code makes that natural, do it. If it needs more than a few lines,
     reject such an exit from inside an arena block with a clear error, and record that as
     known. Under the arena, scope-end and statement-end drops still run and are no-ops, so
     codegen needs no special case.
   - **Tests.** Blocks in loops, nested blocks, a block returning each of `Int`, `Str`, `List`,
     `Dict`, a struct, a boxed union and `None`, and the freezes errors (one test per mutator
     kind). A test that a loop of arena blocks keeps `bytes_allocated` flat. An AOT test. The
     leak gate must still pass. Add a `pipeline.rs` workload that builds and discards a
     tree-shaped value per iteration, with and without `with Arena`, to show the effect.
   - **Acceptance.** `cargo test --workspace`, plain and with `FROG_LEAK_CHECK=1`; clippy no
     worse than 18 warnings; `orders` 52, `words` 147, `life` 15 ms not slower (arena support
     must cost nothing when no arena is active: the check in `alloc_bytes` is one branch, and
     look for any measured cost); net line count recorded.
   - **Out of scope.** Per-task heaps; segment headers; the result allocator (evacuation is by
     copy, for now); `Pool<T>`/`Id<T>` (step 5); other `with` forms (`Scope`, `Deadline`, ...);
     an arena lint for loop-carried reallocation; the `gc` rename.

   **Step 4 status (2026-11-08): done.** `with Arena { body }` works in the JIT and
   the AOT build, and the leak gate holds.
   - **4a, runtime** (`gc.rs`). `GcHeap` has a stack of bump arenas (first chunk 16 KiB, then
     doubling, an oversized object gets a chunk of its own, chunks are taken lazily on the
     first allocation). Three entry points replace the planned `frog_arena_exit(result, mask)`:
     `frog_arena_enter`, `frog_arena_evac` (the top arena stops accepting allocations, so the
     enclosing allocator is current) and `frog_arena_release`. Codegen clones each heap leaf of the
     result between the last two, with the same `emit_clone_word` that copy-on-keep uses. That
     needs no type mask and handles multi-leaf structs and inline unions for free.
     `drop_tree` and `free_shallow` return at once while an arena is active. The live-object and
     byte counts are snapshotted at enter and at evac, and release subtracts the difference, so
     the leak oracle still sees only outside objects. List and dict growth goes through
     `GcHeap::grow_buffer` (it was the free function `grow_raw_buffer`), which bump-allocates and
     copies in an arena. Dicts record themselves in their arena, because their hash index is a
     Rust `Box` that release has to drop.
   - **Zero cost when idle.** While an arena is active the real free lists are swapped out for
     empty ones, so `alloc_bytes`'s hot path is unchanged and the arena check lives only in
     `alloc_slow`, behind the free-list miss. Putting the check in `alloc_bytes` itself cost
     `words` about 5% (147 to 154 ms) through inlining, not through the branch.
   - **4b, language.** `with` is a keyword and `Expression::Arena` / `TypedExprKind::Arena`
     are new nodes (about 16 walker arms, all one line). Typeck records `ScopeStack` depth at
     entry (`arena_levels`) and every in-place write site (`lower_place`, plain reassignment,
     `check_mut_receiver`) calls `check_not_frozen`, which rejects a binding declared at or
     outside that depth. That covers assignment, place assignment, `push`, mutating methods and
     `mut` arguments with one rule and no second analysis. Parse errors for any other `with` form
     say that only `Arena` exists.
   - **Deviation: moves are not errors, they are copies.** The scope said a `Move` read or a
     consuming call argument of an outer binding is an error. That would make `with Arena
     { summarize(text) }` illegal whenever `text` is not used afterwards. Instead liveness treats
     the outer names as live through the whole body (`transfer`'s `Arena` arm, which also covers
     `return` in the body), so every read of one is a `Copy` and copy-on-keep copies it into the
     arena. Nothing outer is ever moved in, so the property the scope relied on still holds. This is
     six lines in `liveness.rs` and removes a class of error message.
   - **Found: shadowing.** Codegen keeps one Cranelift variable per name, so `let xs = ...` in an
     inner block overwrites an outer `xs` (true for plain blocks today: `mut n = 1; { mut n = 10 };
     print(n)` prints 10). In an arena that would leave the outer name pointing into released
     memory. A `let`/`mut` that shadows an outer binding inside an arena block is therefore
     rejected. Loop variables, parameters and match binders were not checked; shadowing an outer
     heap name with one of those inside an arena block was still unsound. Fixed properly
     afterwards: see "One variable per name" below.
   - **4c, codegen.** The arm is enter, body in a frame of its own (so a non-block body's
     temporaries end before release), pop, evac, clone leaves, release. `return` and `?`
     leaving the block fit in a few lines: `compile_return` evacuates once per open arena
     (`Ctx::arena_marks`), innermost first, and `drop_all_frames` drops only the frames outside the
     outermost arena. There is no `break` in the language, so `return` and `?` are the only
     exits; nothing was rejected. A discarded block releases without copying.
   - **Checked: the scope's "a free never meets an outer pointer" claim.** It holds. Writes are
     frozen by typeck, outer moves became copies, a callee's `mut` and consumed parameters can only
     be fed arena values or copies, and a function called inside runs against the same arena, so its
     allocations and drops are consistent. The one hole found was the shadowing conflation above,
     and the one bug found was `return outer` moving an outer local into the arena (the `Return`
     arm of liveness ignores `live_out`), fixed by the same liveness rule.
   - **Tests.** 24 in `tests/test_arena.rs` (each result type, loops, nesting, outer reads, last
     use of an outer name, consuming callee, return, nested return, `?`, bare `return`, a flat
     `bytes_allocated` across 3,000 blocks, one freeze error per write kind, the nested-arena
     freeze, shadowing, bad `with` form), 6 Rust unit tests in `gc.rs`, one AOT test, and the
     `tree_discard` / `tree_arena` criterion workloads. `cargo test --workspace`: 1,229 passed,
     0 failed, 2 ignored, plain and under `FROG_LEAK_CHECK=1`. Clippy: 18 warnings, none new.
   - **Results.** Release, best of 7, wall clock including JIT compilation:

     | Benchmark | step 3 (HEAD) | step 4 |
     |---|---|---|
     | `orders` | 52 ms | 52 ms |
     | `words` | 148 ms | 148 ms |
     | `life` | 15 ms | 15 ms |
     | `tree_discard` (300 trees of depth 9) | 37 ms | 37 ms |
     | `tree_arena` (same, in `with Arena`) | | 27 ms |

   - **Size.** `git diff` for `froglang-core/src` is +490/-50. Of that, 130 lines are the `gc.rs` unit
     tests, so about +310 net of real source (runtime about +165, frontend about +85, codegen
     about +60). Tests and benches add about +540 (test_arena 361, unit tests 130, pipeline 38,
     AOT 9).
   - **Left open.** The result is copied out, not built in
     place, so a block whose result is large pays a copy. No arena lint for loop-carried
     reallocation. No arena-aware `gc_dump` line. (The shadowing hole above is closed: see
     "One variable per name".)
   - **One variable per name (2026-11-09).** Fixes the conflation found above, so
     an inner declaration, loop variable, comprehension variable, match binder or `catch` binder
     that shadows an outer name no longer touches it. The arena restriction in `lower_assign`
     and its test are gone.
     - *Design.* `TypedExprKind::Assign` gained `decl: bool` (true for `let`/`mut` and every
       synthesized binding, false for `name = value`). Codegen's name map is now `Vars`
       (`codegen/mod.rs`): `declare_fresh_var` gives each declaring occurrence a new Cranelift
       `Variable` and logs what it displaced; `Vars::open`/`close` bracket a scope and put the
       displaced entries back. Scopes opened: `Block`, each loop iteration (the loop variable is
       declared inside it), the whole iterator-protocol loop, a conditional branch, a `with Arena`
       body. These mirror `TypeChecker::in_scope`. `get_or_declare_var` stays for reassignment.
       Match binders and `catch` binders needed no code of their own: typeck lowers them to
       `decl` assigns at the top of an arm `Block`.
     - *Name-keyed state.* `Ctx::owned_names` became `owned_vars` keyed by `Variable`, so a
       shadow is its own owner (a borrowed parameter shadowed by an owned `let` no longer makes
       later reads of the parameter look owned). `mut_param_copyout` reads the parameters' own
       `Variable`s (`Ctx::mut_vars`), so an early `return` inside a shadow of a `mut` parameter
       hands back the parameter. `known`, `temp_of` and the frame slots were already keyed by
       `Variable` or `Value`.
     - *Liveness.* The unsound case was real: a declaration killed its name, so for
       `let x = ..; read(x); { let x = ..}; use(x)` the first read looked like the last use of `x`
       and moved the outer binding away. `transfer`'s `Block` arm now keeps a name live through
       the block when it is declared there and also live after it (`keep`); a bare `let` as a
       branch, loop body or arena body gets the same through `transfer_scoped`. Cost: reads of
       the inner binding are copies when the outer name is live after the block. The for-loop
       transfer needed nothing, since its header set already re-adds `live_out`.
       `conventions.rs` and `linear.rs` stay name-based: for conventions a shadow's uses only add
       escapes to the parameter (conservative); `linear.rs` was not changed or audited beyond the
       tests passing.
     - *Top level.* A top-level `let` of a name the entry or an earlier entry already bound
       releases the old leaves first (`release_top_level_binding`) and forgets their keys, so
       redeclaring at another type works and nothing leaks. `build_main_body` keeps one binding
       per name. Inside blocks the old variable stays in its frame and is dropped at the block's
       end.
     - *Two fixes that fell out.* The parser unwrapped `{ e }` to `e`, so `{ mut n = 10 }` was
       not a scope and redeclared `n` (the original repro). A lone `let`/`mut` now stays a
       `Block`. And a bare `let` as an `if` branch (usually under a `Widen`) had no frame to
       own its value and leaked; the branch now gets one.
     - *Tests.* `tests/test_shadowing.rs`: 44 tests, most run over seven value kinds (Int, Str,
       List, Dict, struct, boxed union, inline union): blocks, `mut`, reassignment, three-deep
       nesting, if/else, loop bodies, loop/comprehension/match/`catch` binders, same-scope
       redeclaration (all 49 ordered type pairs, top level and block), parameters (incl. a `mut`
       parameter with early `return`), arenas, outer-read-then-shadow (the liveness case), moved
       shadows, and four REPL tests. All under the leak oracle.
     - *Results.* `cargo test --workspace`: 56 test binaries, 0 failed, plain and under
       `FROG_LEAK_CHECK=1`. Clippy: 18 warnings, none new. Release, best of 7: `orders` 52 ms,
       `words` 149 ms, `life` 15 ms (HEAD: 52, 147, 15).
     - *Left open.* `linear.rs` is name-based (above). An else-less `if` whose branch is a bare
       `let` of a union-typed value hits the existing "widening a union into a different union"
       error, as any such branch does. Dropped-late same-scope redeclaration: the shadowed value
       lives to the end of its block rather than to the redeclaration.
5. **`Pool<T>`/`Id<T>`.**

   **Step 5 status (2026-11-10): implemented, uncommitted; full workspace run pending.**
   - **Shape.** Three prelude generic structs, installed by `stdlib::install` from
     `frontend/typeck/pool.rs`: `__PoolSlot<T>(gen, next, value)`, `Id<T>(index, gen)` and
     `Pool<T>(slots, head)`. The free list is threaded through `slots` (`head` and each slot's
     `next`), so no list pop is needed. Nothing new reaches codegen. Each operation is a typeck
     template parsed from source and lowered through the existing `if`, assign, index and place
     machinery, the same way `Dict`'s `get`/`remove` are built. A generic `func` cannot read a
     struct field before instantiation, which is why these are builtins and not prelude functions.
   - **Surface.** `Pool()`, `g.add(v): Id<T>`, `g.remove(id)`, `g.get(id): T | None`, and `g[id]` as
     a read and as a place (`g[id] = v`, `g[id].f.push(x)`). A stale id panics on read, write and
     remove, and `get` returns `none`. The receiver must be a bare `mut` binding, as `remove`
     already requires on `Dict`. An arena block may read a pool but not mutate an outer one, and
     this falls out of `check_not_frozen`.
   - **Deviations.** `remove` cannot release the freed slot's `T`, since there is no placeholder
     to leave behind. The old value stays in the slot until the slot is reused or the pool is
     dropped. Id is not `Hash` (`Trait::Hash` is scalar-only), so an Id cannot key a `Dict` yet.
     `Pool` and `Id` are reserved prelude names, so a user `data Pool` or `data Id` collides with the prelude (the internal slot type is the reserved `__PoolSlot`). A whole `Linear` element cannot be read out of a pool (`let v = g[id]`, `g.get(id)`) since that copies it; field reads (`g[id].f`) are fine. Free
     `add(mut g, v)` / `remove(mut g, id)` forms are not supported, only the method form.
   - **Tests.** `tests/test_pool.rs`: 22 tests, passing. Covers basics, scalars and strings,
     writes through ids, cycles via `List<Id<Node>>`, mut parameters, reuse with a bumped
     generation, LIFO free list, stale-id panics, heap-value leaks under the oracle, typing
     errors, and arenas.
   - **Not yet verified.** `cargo test --workspace` and clippy could not run to completion. The
     disk filled (about 285 MB free, 20 GB in the worktree's `target/`), and linking failed with
     `ENOSPC` in unrelated test binaries. The `froglang-core` binary and `test_pool` both build and
     pass, so the regression run is still owed.
6. **`Abort` and `?`.** This needs no allocator work and can land early, against today's GC
   runtime. It adds the trait, the error-arm desugar in typeck, the `Linear` requires `Abort`
   check, and `linear.rs` treating `?` edges as consuming. Also add the `implicit-copy` lint, which
   is useful with or without the GC as a report of where COW marks happen.

   **Status (2026-11-10): 6a (`Abort` and `?`) and 6b (the `implicit-copy` report) done; the deny annotation and `copy()` are not.**
   - **The trait.** `Abort` is an ordinary source-declared trait, seeded next to `Iterable` and
     `Container` (`trait Abort { func abort(x: Self) }`), so `provides Abort { func abort(...) }`
     goes through the normal impl path. `data T provides Linear` without `Abort` is a type error at
     the declaration. The rule is checked once an entry's impls are all registered, so it holds
     however `Linear` is granted (a declaration's or union's `provides`, or a standalone `provides
     Linear for T`), and the two may be granted separately.
   - **The `?` edge.** Typeck cannot know which values are owned at a `?`, so the error arm is
     `{ abort!edge(); return e }`, where `abort!edge` is a reserved builtin. `linear::insert_aborts`
     runs after `number_nodes` (it reads liveness), replaces each placeholder with `abort(x)` calls,
     newest binding first, and `number_nodes` runs again. It runs in `frontend::pipeline::lower_checked`,
     which every compile path (JIT, `frog build`, `check`, `check --copies`) shares. This differs from the plan's "typeck-level desugar", which assumed a live set
     typeck does not have; codegen still needs nothing new.
   - **What is aborted.** A binding of the enclosing function (a `let`/`mut` or a non-`mut`
     parameter it owns) in scope at the edge, whose type provides `Abort`, with no `Move`-classified
     read before the edge that handed it off. Ownership follows the parameter conventions, inferred
     *before* the calls are inserted: a borrowed parameter is its caller's, and a last use lent to a
     borrowing position is not a hand-off, so the owner aborts it, once. `Linear` values ignore
     conventions (no aliasing, so a `Move` read always hands off). `lower_checked` re-infers
     conventions after insertion, because they are keyed by node id. For `Linear` a move is consumption, so this is exact. For a plain `Abort` type a
     move is only the last use, so a value whose last use is the failing expression itself is dropped,
     not aborted. `catch`, `!`, a plain `return` and panic abort nothing.
   - **Not aborted (conservative, never twice):** a binding shadowed, at the edge, by a later
     declaration of the same name; a loop or comprehension variable; an anonymous-union binding
     whose members have different `abort` impls (a nominal union's one impl is found through its
     variants); and any binding declared outside the enclosing `with Arena` block, since outer places are
     frozen there. A `Linear` value in one of these cases makes the `?` a type error (rule 3 below)
     rather than a leak. Bindings are tracked per declaration, not per name, so a shadow that has
     gone out of scope no longer hides the outer binding.
   - **`linear.rs` rule 2** no longer counts a branch that ends in `Never` (the `?` error arm), and
     no longer counts a binding declared inside the branch's own block. The second was an existing bug:
     a Linear `let` used inside one arm read as "consumed on only one branch".
   - **`linear.rs` rule 3 (2026-11-10)**, §3.6's "failing to consume is a compile error":
     `check_exits` requires every `Linear` binding of a function to be consumed before its scope ends
     by falling through (end of its block, end of the body for a parameter) and before every
     `return`. The `return` in a `?` arm sees the inserted `abort(x)` calls, so a `?` that cannot abort
     a live `Linear` value is rejected. Exempt: `mut` parameters (copied out), the argument of a
     type's own `abort`, and paths that panic. Still loose: a value declared outside a loop and
     consumed inside it counts as consumed. Top-level entries are unchecked (see `linear.rs`).
   - **`match` and containment (2026-11-10).** A `match` consumes its subject once, at the move into
     its temporary, whose per-arm reads are exempt from all three passes. The arm owes what it got:
     the narrowed subject, or each Linear field it bound. Taking a value apart consumes it, so an arm
     may not leave a Linear part unbound. `?` inside a narrowed arm aborts the narrowed value, widened
     back into its union. `Linear` is now contained: a struct, variant, `List` or `Dict` holding a
     Linear value is Linear, and a union is Linear if any member is. A field read inspects a live
     value; at its last use it takes the value apart, which must not drop another Linear field. A
     type that is Linear only by containment needs its own `Abort` impl for `?` to abort it (there is
     no derived one yet). Still open: indexing a `List` of Linear values copies the element, and
     consuming the list drops the rest.
   - **Not done:** the resource table and panic-time aborts belong to step 7.
   - **Tests.** `tests/test_abort.rs`, 17 tests, observing order and absence of calls through printing
     `abort` bodies, all under the leak oracle. `test_linear.rs` gained `Abort` impls. `cargo test
     --workspace`: 1,289 passed. Clippy: 18 warnings, none new. `orders`, `words` and `life` match the
     previous commit measured side by side (56/186/17 ms against 56/191/19 on a slower machine day).
   - **6b, the lint.** `frog check --copies <file|expr>` compiles to a native object (no link, no run)
     and prints one warning per implicit deep copy, with file, line, column and the copied type, then a
     count. The findings come from codegen, which is where copies are decided, so they cannot drift
     from what actually runs: `own_vals` records a `CopySite` (thread-local log, off unless
     `start_copy_log` was called) with `Ctx::cur_span`, the expression that kept the value. Reporting
     the keeper rather than the read is deliberate: `let b = a`, `push(mut s, a)` and a by-value call
     argument each point at the statement that stores the value. Arena evacuation copies are not
     reported; they are the block's contract. `benches`: `orders` 2 sites, `words` 2, `life` 1.
     Top-level bindings stay live for the whole entry (Q6), so reads of them are flagged more often
     than the same code inside a function.
   - **Side fix.** `frog build` skipped `insert_aborts` and the conventions pass, so a native build
     of a `?` over an `Abort` value called a placeholder, and every call was compiled as consuming.
     `build` and `check --copies` now share `lower_for_object` in `main.rs`.
   - **Not done:** `#deny(implicit-copy)` per function or module (annotations exist only on `data`
     fields today, so this needs parser support for function-level annotations), the `copy(x)`
     silencing builtin (nothing to silence until a lint can be denied), and a REPL setting.
   - **Tests.** `tests/test_copy_lint.rs` (6), one AOT test in `test_build_cli.rs`. `cargo test
     --workspace`: 1,296 passed, plain and under `FROG_LEAK_CHECK=1`. Clippy: 18, none new.
7. **The `with` protocol, second-class handles, the cleanup stack and the resource table.** Then
   `CONCURRENCY.md` stages 2 onward run on this foundation, without stage 1's unwinder, and with
   deferred-init `let` instead of `Task<T>`.

## 7. Step 0 and step 1 measurements (2026-11-05, branch `nogc`)

Method. `FROG_COPY_STATS=1` counts, at each `Copy`-classified read or list/dict extraction of a
`Str`, `List` or `Dict` leaf, a deep copy of the value, and records its bytes. The copy is
discarded, so output is unchanged. Each event is tagged with the flow its value goes into
(`liveness::CopySite`). Extractions are tagged `extract`, and `Var` reads `var`.
`FROG_DUMP_CONVENTIONS=1` prints the borrow/consume result for each function parameter. The
program runs with `cargo run --release -- run`, and the benchmarks are `froglang-core/benches`.
`fib` is not included, because its source is under `tests/`, and it has no heap data anyway.

| Benchmark | `var` copies | `extract` copies | Notes |
|---|---|---|---|
| `orders` | 0 | 0 | no `Str`/`List` data flows through reads |
| `words` | 642,400 events, 50.8 MB (`Str` 14.8 MB, `List` 35.9 MB), all `call-arg` | 320,000 `Str`, 10.6 MB, all `return` | the `Str` extractions are `word_for`'s `vocab[i]` |
| `life` | 829,480 events, 17.6 GB, all `call-arg` | 806,560 `List`, 348 MB, all `other` | `other` is transient: the `rows[y]` target of `rows[y][x]` |

Step 1, under the rules in `conventions.rs`:
- Every `call-arg` `Var` read in `words` and `life` is passed to a parameter that only reads. So
  borrowing removes all of them. In `life` every function parameter is `Borrow`. In `words`,
  `build_doc`, `word_for` and `score` are `Borrow`.
- The `extract return` copies in `words` are not removed by borrowing. `word_for` returns an
  element, so the caller needs an owned value.

**Residual copies under the design:** `life` 0, `orders` 0, and `words` 320,000 small `Str`
copies, which are extractions returned from a borrowed list.

Recommendations:
- **Q1.** Keep deep copy with inferred borrowing as the model, since it removes all the large
  `List` copies. Do not adopt RC for everything. For `Str`, consider RC only if timing shows that
  the 320,000 extraction allocations cost enough to matter. Those copies average about 33 bytes.
- **Next measurement:** time `words` with extraction copies counted against an RC variant, using
  `pipeline.rs` as well, since it has not been run.

Caveats:
- Byte counts are clone allocation sizes. Stats runs perturb GC timing, so they are for counts
  and bytes only, not timing.
- Top-level bindings stay live for the whole entry (Q6). Reads of them inside top-level loops are
  therefore counted as `Copy`. In these benchmarks the loops are mostly inside functions, so the
  effect is small, but this has not been measured.
- Inline unions with heap members are not counted. Struct field extraction (`p.f`) is not counted
  either, because its codegen has several early returns.
- Convention analysis assumes host and stdlib functions borrow their arguments, since marshalling
  copies them. It assumes `push` consumes its value argument. A `let` alias counts as consuming,
  which is conservative. §1.2 allows a borrow when the alias is neither mutated nor moved.
- The analysis runs per program. A file is one entry, so it is sound for `run`. For REPL entries,
  callees from earlier entries are unknown and are treated as host functions, which is unsound.
  Codegen does not use the result yet, so nothing depends on it.
- The extraction rule in §1.3 (extraction copies) is the design, not current codegen behavior.
  Current codegen shares the element pointer (MUTABILITY.md stage 7).
