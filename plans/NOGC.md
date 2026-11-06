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
   **Status: first slice done, uncommitted** (`FROG_OWNERSHIP`). Every site that marked a list or
   dict leaf `shared` (`Var` reads, extractions, for-loop elements) now deep-copies it, unless the
   read is a borrowed call argument or a transient target. The `shared` bit is never set in this
   mode. `values(d)` deep-copies its result. The GC still runs and frees nothing explicitly, so
   drops and the leak checker are not done yet. Both modes pass the suite (1192 tests). Under
   `FROG_OWNERSHIP` the `life` benchmark matches default mode, and so do `orders` and `words`.
   The COW verifier does not catch a missing copy in `let b = a`, because both bindings share one
   SSA value. The value-semantics tests do catch it: skipping the copy fails 18 of them.
3. **Delete the GC:** stack maps, roots, COW, `RuntimeRoots`, and the AOT stack-map table.
   `SELFHOST.md` G6 disappears.
4. **Allocator layer:** segment headers, task heap and bump arena; then `with Arena` with
   `freezes`, and the result allocator.
5. **`Pool<T>`/`Id<T>`.**
6. **`Abort` and `?`.** This needs no allocator work and can land early, against today's GC
   runtime. It adds the trait, the error-arm desugar in typeck, the `Linear` requires `Abort`
   check, and `linear.rs` treating `?` edges as consuming. Also add the `implicit-copy` lint, which
   is useful with or without the GC as a report of where COW marks happen.
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
