# Concurrency

Status: **design**. Nothing here is implemented. Syntax in this document is a sketch and
should be expected to change; the *decisions* in "Foundational choices" are the part meant
to be stable, since they are the ones that are expensive to revisit later.

This supersedes the "Effects, Capabilities, Concurrency" section of `DESIGN.md`.

## Goals

- Structured concurrency as a language feature, not a library convention: a task cannot
  outlive the scope that created it, and this is checked, not documented.
- No function color. All I/O is asynchronous; there is exactly one `read`, one `get`, one
  standard library. Python's sync/async schism is the failure mode being avoided.
- Composable, not a bag of hardcoded knobs. `Scope` should be an ordinary library type built
  on a small set of primitives, so a user can write their own supervisor and have `spawn`
  work inside it.
- Cheap enough that a program which never spawns anything pays nothing.
- Concurrent code should be deterministically testable.

## Foundational choices

| Question | Decision | Why |
| --- | --- | --- |
| Suspension mechanism | Stackful fibers | No codegen changes, no color, ordinary calls stay ordinary calls |
| Stacks | Large lazily-committed virtual reservations + Cranelift `stack_limit` | Deep recursion works; no stack copying (see below); clean overflow error |
| Scheduler | M:1 (one scheduler, one heap, one OS thread) per runtime instance | Buys a no-data-races guarantee; multicore via isolates or, later, scope affinity |
| Cancellation | Unwind, not a value | Errors-as-values can't be threaded reliably through third-party code; unwinding is needed for panics regardless |
| `Task(T)` escape prevention | Region-bound types (staged: syntactic first) | Makes the structural guarantee a fact rather than a convention |
| "must be in a scope" | Capability, discharged by `with Scope` | Same mechanism as `can fs.Write`; user-extensible, not a hardcoded rule |
| I/O implementation | A swappable value in the ambient context | Blocking / fiber / io_uring / deterministic-test implementations from one source |

### Stackful fibers

Each task gets its own stack; suspension is a register save and a stack-pointer swap, on the
order of tens of nanoseconds. Calls remain ordinary calls, so **the Cranelift backend needs no
changes to support suspension** — this is a runtime feature, not a codegen feature.

The alternative, stackless coroutines, transforms suspending functions into state machines.
That transformation *is* function color, imposed by the implementation rather than by taste:
a transformed function is structurally different from an untransformed one. Transforming
every function would erase the distinction at the cost of making every call a state-machine
step, which is the wrong trade for a language with a native backend.

Zig's stackless async needs whole-call-graph analysis to size coroutine frames, which is a
persistent source of difficulty around recursion and function pointers. Stackful fibers avoid
that problem entirely.

### Stacks

Each fiber reserves a large region of virtual address space (8 MB is the working default) that
the OS commits lazily. A typical task's resident set is one or two pages; virtual address
space is the cheap resource on 64-bit. Deep recursion works up to the same headroom an OS
thread gets.

**Growth by copying is not available to us.** `ShadowFrame` holds raw pointers into the native
stack (`gc.rs:129`, `push_frame(slots: *mut i64, len)`). Relocating a stack would dangle every
one of them; Go-style copying growth would require making shadow frames relocatable and having
every JIT prologue cooperate. Segmented stacks avoid moving but reintroduce the hot-split
problem that both Go and Rust shipped and then removed.

Overflow detection uses Cranelift's built-in `Function::stack_limit`
(`cranelift-codegen-0.113.1/src/ir/function.rs:198`), a prologue SP-versus-limit check, so
exhausting the reservation produces a clean frog panic rather than a SIGSEGV on a guard page.
If genuine growth is ever wanted, that check is exactly where a `frog_grow_stack` call would go.

Cost relative to Go: reservations are limited by the OS mapping count (~65k on Linux by
default), so live tasks cap in the tens of thousands rather than the millions. Poolable and
tunable, and far past what most programs need.

### One thread, and what that costs

One scheduler, one `GcHeap`, one OS thread per runtime instance. `FrogState` is the isolate
boundary; a host application can run several.

What this buys is the reason to do it: **interleaving happens only at suspension points**, so
shared mutable state between tasks is safe without locks, and the language can promise no data
races. What it costs is in-process shared-memory parallelism.

Going M:N later would require, in increasing order of pain: per-thread allocation buffers
(easy); safepoint polls emitted on loop back-edges so stop-the-world marking can happen (a
codegen change, typically 1–3% throughput); a per-thread shadow-stack registry (mechanical);
and **a written memory model**, with atomics, mutexes, and the entire race-detection apparatus
that follows. The first three are implementation work and are retrofittable. The fourth is a
semantic promise and is not.

So the guarantee is deliberately worded as:

> Tasks within a scope interleave only at suspension points.

rather than "the runtime is single-threaded." The first is what a programmer needs in order to
reason about shared state; the second is an implementation detail. The first also remains true
under a future M:N scheduler **if scopes are the unit of thread affinity** — a scope's tasks
always run on one thread, and parallelism comes from explicitly-marked sibling scopes elsewhere.
That keeps the door to multicore open without invalidating any library written in the meantime.

To preserve that option, three things should be true from the start even while the runtime is
single-threaded:

1. Shadow stacks are per-task behind a registry, not one global `Vec` (`gc.rs:147`).
2. Allocation is routed through an explicit heap handle rather than a thread-local
   (`ffi.rs:11`, `ACTIVE_HEAP`).
3. Documentation promises the suspension-point property, never single-threadedness.

CPU-bound parallelism in the meantime comes from `blocking { }` (below), where pool threads run
Rust FFI and never frog code — so no shared heap, and no memory model needed. For an embeddable
language, "call into parallel Rust" covers most of the real cases.

## Primitives

The irreducible set the compiler and runtime must provide. Everything else is library code.

1. **`suspend` / `resume`** — yield the current task to the scheduler; make a task runnable
   with a value. Every other concurrency feature is built on these. The representation must
   allow a task to be parked on *N* wakeup sources at once, not one, or `select` becomes
   impossible to add later.
2. **`spawn`** — a syntactic form, not a function. Arguments evaluate eagerly at the spawn
   point; the body runs later. Produces a `Task(T)` with a result cell.
3. **`unwind(task, reason)`** — force a task to unwind from its next suspension point, running
   cleanup as it goes. Cancellation is `unwind(t, Cancelled)`; panic is the same machinery
   applied locally.
4. **Dynamic (task-local) binding** — so `spawn` finds the current scope, and I/O finds the
   current `Io`, without threading either through every signature. Also the mechanism for
   request context and tracing spans.
5. **`with`** — the general context manager. `with E as x { body }` calls `E.enter()`, binds,
   runs the body, and calls `E.exit(outcome)` on *every* path out, including unwind. This is
   what allows `Scope` to be library code, and it independently provides files, locks,
   transactions, and `defer`.

A clock and timer source is needed too, but it is an ordinary I/O primitive rather than a
concurrency-specific one.

### Task boundaries are unwind barriers

A failing task unwinds its own frames, running cleanup, and stops at its own root. The outcome
lands in the task's result cell:

```
data Outcome(T) is Ok(value: T) | Err(error: Error) | Panicked(info: Panic) | Cancelled
```

The parent — blocked inside its scope's `exit` — is then resumed and decides what to do.
Nothing crosses task frames implicitly. This is the same shape as `thread::join` catching a
panic in Rust, and it is the answer to "how does a child's error reach the scope."

## Scope

`Scope` is a library type. Its entire configuration is a **fold over child outcomes**:

```
data Verdict is Continue | StopAndCancel | StopAndWait

// A supervisor is: (accumulator, one child's outcome) -> (updated accumulator, verdict)
type Supervisor(A, T) = ((A, Outcome(T)) -> (A, Verdict))

data Scope(A, T)(
    children:  List(Task(T))
    supervise: Supervisor(A, T)
    acc:       A
) provides Spawn
```

`Scope.exit` is a single loop: wait for the next child to settle, fold it, act on the verdict,
and once the body has finished *and* the verdict says stop, cancel or await the remainder and
return the accumulator. Because `exit` runs on unwind paths too, the join is guaranteed —
which is what makes the structural guarantee hold.

**The value of a `with Scope(...)` block is the supervisor's accumulator**, not the value of
the block body. The body runs for its spawning effect.

The standard library supplies the common folds as ordinary functions:

```
func all(acc: List(T), o: Outcome(T)): (List(T), Verdict) = match o {
    is Ok(v)  then (acc + [v], Continue)
    is Err(e) then (acc, StopAndCancel)      // first error cancels siblings and propagates
}

func collect(acc: (List(T), List(Error)), o: Outcome(T)) = ...   // never cancels
func race(acc: T?, o: Outcome(T)) = match o {
    is Ok(v)  then (v, StopAndCancel)        // first success wins, losers cancelled
    is Err(_) then (acc, Continue)
}
```

which makes the shape of a call site depend only on which fold was chosen:

```
let pages          = with Scope(all)     { for url in urls do spawn http.get(url) }
let results, errs  = with Scope(collect) { for url in urls do spawn http.get(url) }
let fastest        = with Scope(race)    { for m in mirrors do spawn http.get(m) }
```

`scope { ... }` is sugar for `with Scope(all) { ... }`. It is sugar, not a syntactic form —
nothing about `Scope` is hardcoded, and a user-defined supervisor works identically.

### Task handles

`spawn e` always has type `Task(T)`. Reading a handle before its scope has joined is a *type*
error, not a runtime await — which is how results come out without ever writing `await`:

```
func page(id: Int): Page = {
    let user; let posts
    with Scope(all) {
        user  = spawn fetch_user(id)
        posts = spawn fetch_posts(id)
    }
    // the scope has joined; the handles are readable here and only here
    render(user!, posts!)
}
```

Use handles when results are heterogeneous and named; use the supervisor's accumulator when
they are homogeneous and collected. The two are not redundant, and neither is magic: `spawn`
uniformly yields `Task(T)`, and the scope's value is uniformly the fold result.

### What is *not* a Scope parameter

Most abilities a scope might appear to need factor out into separate composable contexts. This
is deliberate, and the fact that they compose is evidence the factoring is right — a deadline
does not care whether it wraps a scope, a single I/O call, or another deadline.

| Ability | Where it lives |
| --- | --- |
| Completion condition (all / race / n-of-m / run-forever) | the supervisor fold |
| Cancel siblings on some event | the fold's `Verdict` |
| Error aggregation | the fold's accumulator |
| Deadline / timeout | `with Deadline(5s) { ... }` |
| Shielding cleanup from cancellation | `with Shield { ... }` |
| Concurrency limit (max N in flight) | `with Limit(8) { ... }` — a semaphore |
| Thread / isolate affinity (future) | `with Isolate(...) { ... }` |
| As-completed streaming | not a knob — `for r in scope.as_completed()` |

`Scope` therefore has exactly one configuration parameter.

`Shield` is the one people forget and then urgently need: cleanup that must run to completion
even though the surrounding scope is being cancelled. It is nearly free once cancellation is
unwind-based — it suppresses delivery within its region.

### Blocking calls

```
let rows = blocking { legacy_ffi_query(conn) }
```

Runs on a thread pool; the calling task suspends. Without this, a single blocking FFI call
stalls the entire runtime — and since froglang is meant to be embedded in Rust applications,
blocking FFI is the normal case, not the exotic one. `blocking` is core, not a nicety.

## Cancellation

Cancellation is an unwind, not a value. A cancelled task stops at its next suspension point,
runs its cleanup (`with` exits and `defer` blocks), and settles as `Cancelled` in its result
cell, where its parent's supervisor sees it.

The alternative — cancellation as a value returned by I/O calls — requires every function in
every library to thread it correctly, and one sloppy `else` turns a cancel into an infinite
loop. Go's `ctx.Done()` demonstrates the failure mode well enough that it does not need
re-deriving.

Errors-as-values remains the mechanism for *domain* errors. Unwinding covers cancellation and
panics. Both need to run cleanup on the way out, so they share one mechanism, and `?` should
be designed against it rather than beside it.

**This means unwinding and `defer` must land before the scheduler is useful.**

## Static rules

Two distinct properties, easily conflated:

- **Rule A** — `spawn` is illegal unless a scope is in the dynamic context. This is about
  *which operations are permitted here*: a capability question.
- **Rule B** — a `Task(T)` must not outlive the scope that created it. This is about *where a
  value may travel*: a region question.

Regions alone would not reject `with File(...) { spawn get(url) }`; capabilities alone would
allow spawning correctly and then returning the handle. Both are needed. Both hang off `with`.

### Rule A: capabilities

A context manager declares what it provides; an operation declares what it requires:

```
data Scope(A, T)(...) provides Spawn
data File(fd: Int) provides Read, Write
```

`with SomeOtherContextMgr(...) { spawn ... }` is an error because that type provides no
`Spawn` — not because the compiler special-cases a builtin named `Scope`. A user-written
supervisor that declares `provides Spawn` works identically.

A function that spawns without containing the scope declares the requirement, which propagates
to callers until some `with` discharges it:

```
func fan_out(urls: List(Str)): List(Task(Str)) can Spawn =
    for url in urls do spawn http.get(url)

with Scope(collect) { fan_out(urls) }      // requirement discharged here
```

This is the same `can` mechanism sketched for effects in `DESIGN.md`, with `Spawn` as one
capability among many.

**Ambient authority, attenuated downward.** Ordinary capabilities like `Fs` and `Net` are
*granted to `main`*, not acquired by user code. The interesting operation is taking authority
away:

```
func main() can Fs, Net = {
    with open("my-file.txt") as f { let data = f.read() }   // no ceremony

    without Net {
        run_untrusted_plugin(input)      // statically cannot make a request
    }
    with Fs(read_only = "/tmp") {
        parse_user_upload(path)
    }
}
```

Zero syntax in the common case, and the sandboxing case becomes expressible. This lands
particularly well for an embeddable language: the *host* decides what `main` is granted, so a
Rust application can hand a frog script read-only filesystem access and no network, enforced
statically inside the script rather than by sandboxing the process.

**`Spawn` is deliberately not ambient**, and the asymmetry is principled. `Fs` and `Net`
concern *authority* — what you may touch — where there is a sane default and the interesting
move is restriction. `Spawn` concerns *lifetime structure* — who waits for you — where there
is no sane default: a task attached to an implicit program-wide root scope outlives its caller
silently, which is precisely the unstructured concurrency this design exists to prevent.
Kotlin shipped that as `GlobalScope` and subsequently had to mark it `@DelicateCoroutinesApi`.

`main`'s body runs inside an implicit root scope, so top-level `spawn` in `main` works. Any
*function* that spawns must declare `can Spawn`, forcing its callers to know it starts
concurrent work. Dynamic binding supplies the scope value; the declaration supplies the
discipline.

### Rule B: regions

A minimal region system — "lifetimes-lite":

- **One region per `with` block. No region variables, no inference.** A region-bound value
  belongs to exactly one region, fixed at creation. Regions are ordered by lexical nesting, so
  "outlives" is just "encloses" and there is no constraint solving.
- A bound value **may not** be returned from its region, stored in a heap object bound to an
  outer region (or unbound), captured by an escaping lambda, or sent to a task that outlives
  the region.
- Boundness is **contagious upward through construction**: a struct or list containing a bound
  value is itself bound to that region. This keeps the check local rather than requiring a
  general escape analysis.
- A bound value **may** be passed as a function argument, bound to a local, and read freely.

That last point is where region systems usually become expensive, because the callee might
retain what it was passed. Rust answers with lifetime annotations, which exist largely because
of separate compilation and abstract checking of generic bodies. Froglang compiles whole
programs through one `FrogState`, so **the checker can look at the callee's body** and verify
it does not retain the value. First-order code therefore never needs a region annotation.

**Annotations are required exactly where the callee is not statically known** — function
*types* and, once traits land, trait method signatures that accept region-bound values. That
boundary is predictable and narrow.

The unification: `with Scope(all) as s` binds a *region-bound handle* in the ambient context.
`spawn` is legal exactly when such a handle is ambient (Rule A, as an object capability), and
the `Task(T)` it produces inherits its region from that handle (Rule B). One mechanism, two
guarantees.

### Why regions are worth the trouble

Beyond task escape:

- **No use-after-close** on files and sockets — the handle cannot escape into a struct.
- **No leaked locks**, and, combined with the suspension-point rule, holding a lock across a
  suspension point can be forbidden statically.
- **Non-copying slices**: `nums[1..3]` as a region-bound view rather than a fresh allocation.
- **Arena allocation**: values provably bound to a region can be bump-allocated and freed en
  masse at region exit, bypassing the GC. The roadmap notes `malloc`/`free`/`memset` plus
  marking are roughly half of what remains in `benches/orders.frog`; region-allocated
  temporaries attack exactly that, with no write barriers and no heap redesign. (This is the
  one benefit that requires the checker to be *sound*, so it comes last.)

### Risks

- **The failure mode is Rust's learning curve.** Defences: one region per block, no
  user-written region variables, no inference, whole-program retention checking, and
  diagnostics treated as a deliverable rather than an afterthought.
- **Generics.** Per-instantiation checking keeps the whole-program argument valid, and
  monomorphization is the natural strategy anyway given unboxed flattened structs. The cost is
  the C++-template model: a library author cannot know a generic function is region-correct for
  instantiations nobody has written, and errors point into library code with an instantiation
  trace. Zig sits at this end and mitigates it with `@compileError` and "called from here"
  chains; Zig's *lazy* analysis, where unreached branches are never checked at all, is the part
  to avoid.
  **The choice is per-property, not per-language**: check ordinary types abstractly at
  definition (real constraint errors, checked once) and defer only *region* checking to
  instantiation. Later, region constraints on generic signatures can be *inferred* and checked
  abstractly; because nothing is user-written, that migration touches no source.
- **The REPL is not a whole program.** `state.rs` compiles incrementally, so a function defined
  at one prompt gets a caller at a later one. Since compilation is on demand anyway, the
  retention check runs per call site as it is compiled — but that means a REPL session can
  surface a region error on a function that was accepted when defined. A deliberate decision,
  not something to discover later.
- **A hole in the region checker is not memory-unsafe.** Every value remains GC-managed;
  regions are a discipline layered over the GC, not a replacement for it. A missed escape means
  a handle read after its scope joined — a logic bug with a clean runtime error, not undefined
  behaviour. An incomplete checker can ship and be tightened over time.

### A capability Rust does not have

Rust's `spawn` requires `'static` because its concurrency is unstructured: nothing proves the
task ends before a borrow does. Structured concurrency proves exactly that — a scope cannot
exit until its children have. So a spawned frog task can safely hold region-bound values from
any enclosing region, and the two features reinforce each other rather than fighting.

## I/O

The I/O implementation is a **swappable value in the ambient context**, not a hardcoded
runtime — the same design Zig arrived at from "no hidden control flow," reached here from "no
function color." One source compiles against a blocking implementation, a fiber scheduler,
io_uring or kqueue, or a deterministic test harness.

Froglang binds it dynamically rather than threading an `io` parameter through every signature;
Zig's explicitness is right for Zig and wrong for a language whose stated goal is to say what
you mean without verbosity obscuring intent. The honest cost is that a call site does not show
which implementation it gets; `can Io` in the signature is the mitigation.

**`spawn` means *may* run concurrently, not *does*.** Under a blocking implementation, a spawn
can execute inline and settle immediately, so simple programs pay no scheduler and no fiber and
the same source still works. The necessary counterpart, borrowed from Zig, is a second spelling
— `spawn_concurrent` — for cases where concurrency is *required*, such as producer/consumer
pairs that deadlock if the producer runs to completion inline. Two spellings, because getting
this wrong is silent.

**Deterministic testing follows from injectability**, and is arguably the biggest single win
here. A test `Io` with a fixed scheduling order and a virtual clock makes concurrent code
reproducible and interleavings enumerable. `DESIGN.md` names trustworthiness of generated code
as a goal; this is a larger lever on it than pre/post-conditions, and it falls out of a
decision being made anyway. It should be treated as a design goal, not a happy accident.

## Deferred

Not foundational; all are library-shaped once the above exists, and none constrain it —
except `select`, which constrains only the representation of a parked task (see Primitives).

Channels, `select`, mutexes and condition variables, async iterators and streams, task-local
storage beyond the dynamic-binding primitive, work stealing, isolates and inter-isolate
messaging.

`Suspend` is deliberately **not** an effect in the `can` set. Tracking it in the type system is
exactly the function-colour problem re-entering through the back door.

## Implementation plan

Each stage is usable on its own and forward-compatible with the next.

### 1. Unwinding

Prerequisite for everything else, and needed for panics regardless.

- Unwind mechanism through JIT frames, shadow-stack frames popped correctly on the way out.
- `defer` and `with`, with `exit` guaranteed on all paths including unwind.
- Panic as an unwind with a payload; `Outcome` as above.

### 2. Fibers

No codegen changes except the stack-limit check.

- Stack-switch trampoline, aarch64 and x86_64.
- Fiber stacks: lazily-committed virtual reservations, pooled.
- Cranelift `Function::stack_limit` wired into the prologue; trap mapped to a frog panic.
- **Per-task shadow stacks**: `GcHeap.shadow_frames` (`gc.rs:147`) becomes a registry of
  per-task stacks; the mark phase walks all live tasks. Do this before there is much code
  depending on the current shape.
- `ACTIVE_HEAP` / `with_heap` (`ffi.rs:11`) routed through an explicit handle.

### 3. Scheduler and I/O

- Single-threaded run queue, timers, readiness (kqueue on macOS to start).
- The `Io` interface as a value; blocking and fiber implementations.
- `blocking { }` thread pool.
- Deterministic test implementation — build it here, not later; it is much harder to retrofit
  once the scheduler has grown assumptions.

### 4. Scopes, syntactically checked

- `spawn` and `scope` as forms; `spawn` must appear lexically inside a scope block in the same
  function; `Task` may not be returned or stored.
- This is a strict *subset* of what stages 6 and 7 permit, so no program written against it
  breaks later.

### 5. Supervisors

- `Scope` as a library type over the fold; `all`, `collect`, `race`, `n_of`.
- `Deadline`, `Shield`, `Limit` as separate context managers.

### 6. Regions

- Region-bound types, contagion, whole-program retention checking.
- Generalises the `Task`-escape rule and immediately pays for files and locks.
- Diagnostics are a deliverable of this stage, not a follow-up.

### 7. Capabilities

- `provides` / `can` / `without`; grants to `main` from the host.
- Merges with the effects work already on the roadmap.
- Effect polymorphism for higher-order stdlib functions: inferred, never written by users.

## Open questions

- Do `Deadline` and `Shield` need to interact with `blocking { }`? A blocked pool thread cannot
  be unwound at a suspension point, so a cancelled `blocking` call presumably completes and its
  result is discarded. Needs a decision before `blocking` ships.
- What is the escape hatch for values that legitimately must outlive their region — a cached
  file handle, say? Some explicit `detach` producing an owned form, with the copy or refcount
  cost made visible.
- Does dynamic binding of the current scope hurt debuggability enough to want a lint that
  requires `can Spawn` even where it could be inferred?
- Error messages for the "handle read before its scope joined" case have to be excellent, since
  it is the first region error most users will ever hit.

## Prior art

- **Trio** — nurseries, cancel scopes, `move_on_after`, shielding. The source of the core
  insight, and of the observation that timeouts and shields compose as their own contexts
  rather than as nursery parameters. Its explicit nursery-passing is the ergonomic wart being
  avoided via dynamic binding.
- **Kotlin** — implicit `CoroutineContext` (adopted), `GlobalScope` (rejected, see Rule A).
- **Go** — colorless goroutines and growable stacks (the model), unstructured `go` and
  `ctx.Done()` cancellation (the anti-model), M:N with permanent data races (the trade being
  declined).
- **Zig** — `Io` as a swappable implementation, "async means *may* be concurrent",
  `asyncConcurrent`, deterministic test implementations (all adopted); explicit `io` parameter
  threading and stackless frame sizing (both declined); lazy per-instantiation analysis
  (declined, though per-instantiation checking of regions is adopted).
- **Rust** — errors as values alongside panic-unwind (the model for having both), lifetime
  annotations driven by separate compilation (avoidable here), `'static` bounds on spawn (a
  restriction structured concurrency removes).
