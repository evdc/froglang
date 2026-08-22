# Mutability and Value Semantics

Status: **design**. Nothing here is implemented beyond what the language already does by
accident (see "Where the language actually is", below). Syntax is a sketch; the *decisions* in
"Foundational choices" are the part meant to be stable.

This supersedes the "Objects: mutable structs?" sketch in `DESIGN.md`, and settles a
prerequisite that `TRAITS.md` (derived `Eq`, `Dict` keys, variance) and `CONCURRENCY.md`
(what "interleaving only at suspension points" is worth) both assume without stating.

## Where the language actually is

This is the load-bearing observation, and it was not designed — it fell out of the flattened
struct representation and the absence of a standard library. Probed against the current build:

| Program | Result | Meaning |
| --- | --- | --- |
| `let b = a; b.x = 9` on a struct | `a.x` unchanged | structs are values; binding copies |
| `func f(p: Point) { p.x = 100 }` | caller's `p` unchanged | arguments are copies |
| `let p = ps[0]; p.x = 77` | `ps[0]` unchanged | reading out of a list copies |
| `ys[0] = 99` | parse error | **no user-facing list mutation exists** |
| `o.i.v = 5` | parse error | no nested-path assignment |
| `let f = z -> z + n` | codegen panic | **no closures**, so no captured mutable state |
| `x = 5` with no prior `let` | binds a new `x` | `let` is optional decoration |
| `let x = 1` then `x = "hi"` | rebinds at `Str` | assignment can change a binding's type |

So froglang today is **already a mutable-value-semantics language**, by omission. There is no
user-reachable aliasing anywhere in it. Strings are immutable, lists cannot be mutated, structs
are flattened into SSA slots and copied on every bind, and there are no closures to capture
anything. The only mutation in the language is whole-variable rebinding, and `p.x = v` is sugar
for rebinding the flattened leaf slots — a functional update wearing imperative clothes.

The last three rows are the holes: `let` means nothing, assignment can retype a binding, and the
distinction between declaring and assigning does not exist. Those are bugs against any static
language's expectations and are worth fixing regardless of which option below is chosen.

**The decision is urgent because it is about to be made by default.** The next four items on the
roadmap — `push`/`set` on lists, `Dict`, a standard library, closures — each have an obvious
cheap implementation that introduces aliasing, because the runtime already represents lists and
dicts as GC pointers internally. Writing `frog_list_set` and exposing it is a two-hour job that
silently converts froglang into a reference-semantics language and cannot be walked back once
libraries depend on it.

## Goals

- Imperative ergonomics. `p.x = 1` and `xs.push(v)` in a loop, not `with_x(p, 1)`. `DESIGN.md`
  says "imperative and pragmatic, not pure-functional and theoretical" and that stands.
- One rule, stated in one sentence, that a reader can apply at every use site without knowing
  which type they are looking at.
- No action at a distance: whether a call can modify my data is visible at the call, not
  discoverable by reading the callee.
- Explicit where it matters, silent where it doesn't. Mutation markers cost tokens; each one
  must buy a guarantee.
- Good-enough performance: no semantics that force an allocation or a copy on the hot path of an
  ordinary pipeline.
- Preserve the option of the concurrency design in `CONCURRENCY.md`, including the M:N door it
  deliberately leaves open.

## Foundational choices

| Question | Decision | Why |
| --- | --- | --- |
| Value or reference semantics | **Values, uniformly** | Already true by accident; the alternative is a per-type rule invisible at the use site |
| What carries mutability | The **binding**, never the type | No `mut T` vs `T`, so no viral qualifier and no variance interaction |
| Default | Immutable (`let`), opt in with `mut` | One keyword; catches the accidental-rebind class outright |
| Callee mutation | `inout`-style `mut` parameters, marked at **both** sites | Makes "this call writes my data" locally visible |
| Aliasing | Not expressible | It is the thing being bought; every guarantee below follows from it |
| Copy cost | Semantics fixed, elision is an optimization | Move-on-last-use, then COW; neither changes program meaning |
| Closures | Capture **by value** | The one place aliasing could re-enter; free to decide now, expensive later |
| Graph-shaped data | Arena plus index handles | The froglang compiler already does this (`ExprRef`) |
| Host resources | Opaque `Handle` types, reference-like, never copied | Files and sockets are genuinely not values; that seam belongs at the FFI boundary |

## Why the concurrency guarantee depends on this

`CONCURRENCY.md` promises:

> Tasks within a scope interleave only at suspension points.

and reads that as "shared mutable state between tasks is safe without locks, and the language can
promise no data races." The first half is true and the second half is a different claim.

Cooperative scheduling eliminates **data races** — no task is preempted mid-instruction, so there
are no torn reads and no memory-model undefined behaviour. It does not eliminate **race
conditions**, which are violations of an atomicity the programmer intended across *several*
operations:

```
let bal = account.balance      // task A
...                            // any suspension point in here — a log write, a metric, a retry
account.balance = bal - amount // task B ran in between and A now writes a stale balance
```

If `account` is shared mutable state, this is broken exactly as it would be under threads. The
interleavings are fewer and deterministic, which makes such bugs reproducible but also **rare and
latent**: they appear only when a suspension point happens to fall inside the critical section.
Worse, the set of suspension points is not local information. A callee that gains a log write in
version 1.2 gains a suspension point, and every caller that was accidentally atomic stops being
so, with no diff at the call site. This is precisely the failure mode of Python `asyncio` and of
single-threaded JavaScript with shared objects, and the standard advice in both — "don't share
mutable state between tasks" — is a convention. `CONCURRENCY.md`'s first goal is "structured
concurrency as a language feature, not a library convention."

So the guarantee's worth is a function of what can be shared:

| If sharing is... | "Interleaving only at suspension points" means |
| --- | --- |
| arbitrary (reference semantics) | no data races; atomicity still your problem; locks return; suspension points become part of every function's contract |
| impossible (value semantics) | interleaving is **unobservable**. Each task reasons locally. The guarantee is a theorem, not a caveat |

Three further consequences follow, all of them load-bearing for designs already written down:

**1. It is also the multicore decision.** `CONCURRENCY.md` correctly separates the retrofittable
work (per-thread allocation buffers, safepoint polls, a shadow-stack registry) from the part that
is not: "a written memory model, with atomics, mutexes, and the entire race-detection apparatus
that follows." A memory model is only needed for state observable from two threads. Under value
semantics there is none, so the M:N door stays open at the cost of the first three items alone.
Under reference semantics, going M:N means writing the fourth. The mutability decision made now
determines whether multicore later is engineering or semantics.

**2. It shrinks Rule B (regions).** The escape rule exists so a `Task(T)` cannot outlive its
scope. Under value semantics the analysis is about the handle: one type, one syntactic check.
Under reference semantics it must cover everything *reachable from* what a task captured, because
a spawned task holding a reference into its parent's data has the same lifetime problem, and now
the check is an ownership system. `CONCURRENCY.md` stages regions as "syntactic first" — that
staging is only viable under value semantics.

**3. Cancellation-by-unwind stops being observable.** A task cancelled halfway through updating
shared structure leaves other tasks looking at a broken invariant. If its state is its own, the
half-updated value dies with the task. Unwind-as-cancellation is a foundational choice already
made; value semantics is what makes it safe.

And `spawn`'s eager-argument rule (`CONCURRENCY.md`, Primitives §2) needs no special case: if
closures capture by value, a spawned body is isolated by the ordinary capture rule rather than by
a concurrency-specific one.

## The options

### A. Reference semantics for heap types (Go, Java, Python, Julia)

Structs stay values because they are flattened; `List`, `Dict`, and anything else heap-backed
become mutable references. `xs.push(v)` mutates through every binding that names that list.

- **For:** zero implementation cost — this is what the runtime already does internally, so it is
  the path of least resistance and therefore the outcome of *not deciding*. Matches what most
  programmers and most training data expect from a Go-shaped language. No copies, no elision
  analysis, no new keywords.
- **Against:** the rule is per-type and invisible at the use site. `f(xs)` may or may not modify
  `xs` depending on whether `List` is one of the reference types — the same wart as Go's slices
  versus arrays, Java's boxed types, and JavaScript's objects versus primitives, and the one that
  reliably confuses both humans and generated code. It forces the identity-versus-equality
  question that `TRAITS.md` currently gets to skip. It degrades the concurrency guarantee as
  above, and it commits the language to writing a memory model before multicore.
- **Reversibility:** none. Once a standard library exists whose functions mutate their arguments,
  aliasing is in every signature. This asymmetry is the strongest argument against A: option C
  can be relaxed toward A later by adding an explicit reference type, but A cannot be tightened.

### B. Full immutability with persistent structures (Clojure, Elm)

Every update returns a new value; structural sharing keeps it affordable.

- **For:** the simplest possible story; concurrency-safe by construction; free structural equality
  and free time-travel debugging.
- **Against:** it contradicts a stated design goal, and it contradicts the performance goal in the
  same breath. Persistent structures allocate on every update, and this language already has an
  allocation problem serious enough that `orders` runs 14× slower than Rust. Loop accumulators,
  the single most common imperative shape, become the expensive path. Rejected.

### C. Mutable value semantics (Swift, Hylo/Val, Mojo, V, Nim's `sink`/ARC)

Every binding holds an independent value. Mutation is in-place update of a variable you own, so
it is O(1) and imperative. Copies are semantic but elided when the compiler can prove nobody
observes the difference. There are no references in the language; where a callee must write to a
caller's variable, it takes a non-escaping `mut` parameter marked at both sites.

- **For:** one rule for every type. Mutation is visible where it happens. Structural equality is
  the only equality, which settles `Eq`, `Dict` keys, and hashing. It is what the language already
  does, so nothing existing breaks. It preserves everything in the previous section.
- **Against:** copies cost real time until elision lands, and the elision analysis is the actual
  work. Shared mutable graphs — observers, caches, doubly-linked structures — must be re-expressed
  as arena plus index. It is a novelty spend relative to the Go-shaped baseline, against
  `TRAITS.md`'s "spend no novelty budget" principle, though Swift has made the model
  mainstream-adjacent and the user-facing rule is *simpler* to state than Go's.

### D. Ownership and borrowing (Rust)

Full aliasing-XOR-mutation, enforced statically with lifetimes.

- **For:** the strongest guarantees, zero-cost, and it subsumes C.
- **Against:** it is the wrong end of every trade this language has chosen — "practicality over
  purity", a GC precisely so that Rust's costs need not be paid, and an LLM-authoring thesis that
  a borrow checker actively fights. Rejected as a whole. **One piece is worth taking:** non-escaping
  `mut` parameters with an exclusivity rule. That is Swift's `inout`, not Rust's `&mut`, and it
  costs a syntactic check rather than a lifetime system, because there is no aliasing for it to
  reason about in the first place.

## Recommendation: C, staged, with the semantics fixed before the optimizations

### 1. `let`, `mut`, and the end of implicit declaration

```
let x = 1              // immutable binding
mut total = 0          // mutable binding
total = total + i      // fine
x = 2                  // error: x is not mutable
y = 5                  // error: y is not declared. Did you mean `let y = 5` or `mut y = 5`?
mut s = "a"
s = 3                  // error: s is Str, cannot assign Int
```

Three rules, each closing a hole in the table at the top: `let` declares and is required;
assignment targets an existing binding; a binding's type is fixed at declaration. `let` and `mut`
are sibling declaration keywords rather than `let mut` — `mut` always immediately precedes the
name it makes writable, in both a declaration and a parameter (§3), which is one token cheaper on
a line that is frequent and keeps the two positions spelled the same way.

Mutability attaches to the *binding*. There is no `mut Point` type, so it never appears in a type
annotation, never propagates through a signature, and has no interaction with variance or with
union members. This is the property that keeps the feature from becoming viral, and it is the
main structural difference from Rust.

### 2. Assignment targets are places, not just names

```
mut o = Outer(i=Inner(v=1), n=2)
o.n = 3                // works today
o.i.v = 5              // parse error today — must work
xs[0] = 9              // parse error today — must work
xs[0].x = 9            // same
o.n += 1               // compound assignment, per DESIGN.md
```

A *place* is a path from a mutable binding root through fields and indices. The root must be
`mut`; nothing else about the path matters, because no other binding can observe the write. For a
flattened local the implementation stays what it is today — rebind the leaf slots. For a path that
passes through a heap object, it is a store, guarded by the uniqueness rule in §4.

### 3. `mut` parameters for callee mutation

```
func bump(mut p: Point): None = { p.x = p.x + 1 }

mut a = Point(x=1, y=2)
bump(mut a)            // marked at the call site: this call writes a
```

`mut` binds the *parameter*, exactly as it binds a `let`/`mut` declaration — never the type, so
`p`'s type is still plain `Point`, `mut` never appears in a `TypeExpr`, and it has no interaction
with variance or with union members. This is the same reasoning §1 already gives for why
mutability attaches to the binding rather than the type, applied one position over. Marking the
call site is the point of the feature: whether a call can modify your data is answered by reading
the call, never by reading the callee. That property is what makes generated code auditable, and
it is worth the four characters.

**Hazard, worth naming because it collides with a reader's priors:** in Rust, `fn f(mut x: T)`
means *a local mutable copy* — `x` is still passed by value, and the caller never sees a write.
Here `func f(mut x: T)` means the opposite: write-back to the caller. The collision is tolerable
for two reasons rather than one accidental one: the call site is always marked (`bump(mut a)`), so
the caller can never be surprised by a write it didn't ask for; and there is no way to spell Rust's
meaning at the parameter at all — "a local mutable copy, no write-back" is `mut q = p` in the body
(below), not a parameter modifier. A function migrating from mutating its own by-value parameter
(illegal since §1 landed) to a `mut` parameter is a *behavior* change, from a real to an
occasionally-surprising feature, and should be reviewed as one rather than applied reflexively
everywhere `mut q = p` appears today.

The exclusivity rule is one syntactic check, and it is cheap here only because nothing aliases:
**within a single call, no variable may appear as the root of more than one argument if any of
them is `mut`.** So `swap(mut a, mut a)` and `merge(mut xs, xs)` are rejected; everything else is
allowed. Swift needs dynamic enforcement for this because classes alias; froglang does not.

A `mut` parameter does not escape. It cannot be stored in a struct, returned, or captured — there
is no type to store it *as*, since `mut` is not part of the type. That falls out rather than
needing a rule.

For a callee that wants a local mutable copy rather than write-back — Rust's `mut` — `mut q = p`
in the body says so in the place where it happens:

```
func record(stats: Stats, score: Int): Stats = {   // stats stays a plain, immutable parameter
  mut r = stats                                     // an explicit local copy — no write-back
  r.count = r.count + 1
  r
}
```

No second parameter form for this — the ordinary declaration syntax already says it.

### 4. Copies are semantic; elision is where the performance lives

The semantics: binding, argument passing, returning, and reading an element out of a container all
produce independent values. The implementation is free to skip any copy no observer can detect.

Three tiers, and **the first two must ship together** — eager copying alone would be a serious
regression on exactly the pipeline shape `orders` benchmarks:

1. **Copy on write-through-a-non-unique root.** Immutable types (`Str`, and any value never
   written) never copy at all.
2. **Move on last use.** If the source binding is dead after a bind, call, or return, transfer the
   buffer instead of copying. Whole-program compilation through one `FrogState` makes the
   liveness question tractable, and the dominant real shape — build a list, pass it down a
   pipeline, consume it — is linear, so this covers most of it with an intraprocedural analysis.
   This is also the same analysis the GC shadow-frame liveness work already wants.
3. **Copy-on-write for the residue.** A `shared` bit in `GcHeader` — the padding is there, and
   `ERRORS.md` already plans to spend some of it on a `type_id`. A write to a shared object copies
   first and clears the bit. This is Swift's model, and Nim's for `seq` and `string` under ARC.

Because tier 1 is a *semantic* statement and tiers 2 and 3 are optimizations, they can land later
without changing the meaning of any program written meanwhile. That property is the reason to fix
the semantics before writing `push`.

**Sequencing consequence, and it is the most actionable line in this document: do not expose
list mutation until tier 2 exists.** Lists are immutable today by accident; keeping them that way
costs nothing, since comprehensions already cover construction. Shipping `xs.push(v)` under
reference semantics "temporarily" is the decision that cannot be reversed.

### 5. Closures capture by value

There are no closures today, which makes this free to decide and expensive to decide later. A
lambda copies what it captures at the point it is created; a captured binding's later mutation is
invisible to it, and a lambda cannot write to its enclosing scope.

This is the one place aliasing could re-enter through the back door — a closure capturing a
mutable local by reference *is* a shared mutable cell — and closing it here is what makes the
`spawn` isolation in `CONCURRENCY.md` a consequence of an ordinary rule rather than a
concurrency-specific special case. The cost is the accumulator idiom (`xs.each(x -> total += x)`),
which is served by the ordinary `for` loop that already exists.

### 6. Graphs get arenas, resources get handles

The honest cost of C: cyclic and shared-observer structures are not expressible as values. The
answer is the one every Rust and ECS codebase converges on anyway — a `List(Node)` plus `Int`
indices. Worth noting for the self-hosting goal: froglang's own compiler is written this way
already (`ExprRef` is an index into an arena), so the bootstrap target is a program the model
suits rather than fights.

Genuinely reference-like things — an open file, a socket, a database connection, a host callback
— are not values and should not pretend to be. They enter as opaque `Handle` types provided by the
host through `FrogState::builder()`: copyable as handles, never deep-copied, with no fields to
mutate. That puts the one unavoidable piece of reference semantics at the FFI boundary, where it
is visible in the type name, rather than diffused through the collection types.

## What this settles elsewhere

- **`TRAITS.md`, derived `Eq`:** structural equality is the only equality. The open question about
  identity comparison never arises.
- **`TRAITS.md`, `Dict` keys:** a key's hash cannot change under you, because nothing else holds a
  writable name for it. This is the bug Python has with mutable keys and Java has with mutable
  `hashCode`.
- **`TRAITS.md`, variance:** the invariance decision was justified by list mutability. Under value
  semantics no aliased list exists to observe an unsound write, so covariance for reads becomes
  *arguable* — worth re-examining before generics land, since `List(Int)` failing to pass as
  `List(Int | Str)` is a real ergonomic cliff in a language that otherwise widens for free.
- **`CONCURRENCY.md`:** the three consequences above — the guarantee becomes total, the M:N door
  stays open without a memory model, and Rule B shrinks to a syntactic check.
- **`ERRORS.md`:** unchanged. Widening a struct into a union boxes a copy, and narrowing copies
  out, which is already the observed behaviour.

## Risks

1. **Copy cost arriving before elision.** The largest one. Mitigated by shipping tiers 1 and 2
   together and by not exposing container mutation until then, but the analysis is genuinely the
   hard part of this proposal and it should be benchmarked against `orders` before `Dict` lands.
2. **Expectation mismatch.** Programmers and models arriving from Go, Python, or Java expect
   collections to alias, and will write code that assumes it. Mitigated because the failure is a
   *type or mutability error at the call site*, not silent wrong behaviour — the opposite of
   option A's failure mode, where the code compiles and corrupts data.
3. **V-lang is the cautionary tale**, not the model. It advertises value semantics with `mut`
   markers at both sites — very close to this proposal — and its reputation problems come from
   shipping the marketing before the implementation. The lesson is sequencing, not design.
4. **Programs that genuinely want sharing** — an in-memory cache, a mutable index, an observer —
   become arena-shaped. This is a real ergonomic cost and should be paid deliberately, not
   discovered.
5. **The `mut`-at-call-site tax.** Four characters per mutating call, on every call. If it turns
   out that most standard library mutation is method-shaped (`xs.push(v)` on a `mut` receiver),
   the marker may be mostly redundant with the receiver's own mutability and worth revisiting.

## Deferred

- `Ref(T)` / explicit shared mutable cells. Backwards-compatible to add later; speculative now.
- Whole-value literals with update (`let b = Point(...a, x=9)`), from `DESIGN.md`. Ergonomically
  attractive and independent of this decision.
- Immutable-by-default *fields* (a `data` field that cannot be assigned even through a `mut`
  binding). Second-order.
- Region-based bulk deallocation, which value semantics makes tractable but which needs the
  concurrency work first.

## Implementation plan

1. **Close the holes.** ✅ `let` required for declaration; assignment to an undeclared name is an
   error; a binding's type is fixed. Frontend-only, no codegen change, and independently correct
   under every option above.
2. **`mut` and the immutability check.** ✅ One flag on the binding (`ScopeStack`'s `Binding`), one
   check in `lower_assign`.
3. **Places.** ✅ Nested-path and index assignment. Grammar plus a generalized `PlaceAssign`
   lowering; the flattened-local path stays as it is; a write through exactly one list index goes
   through `frog_list_set`. Compound assignment (`+=` etc.) deferred — new lexer tokens, no
   existing precedent to extend, and orthogonal to the places mechanism itself.
4. **`mut` parameters** with the call-site marker and the single-root exclusivity check.
5. **Closure capture by value** — decided now, implemented when closures are.
6. **Move on last use**, benchmarked against `orders`, *then* container mutation (`push`, `set`,
   and `Dict`'s mutating operations) on top of it.
7. **COW via the `shared` header bit**, if and only if the benchmarks still want it.

Stages 1–4 are frontend work with no runtime component and can land before generics. Stage 6 is
the one with a dependency in both directions: it needs the liveness analysis, and container
mutation needs it.

## Open questions

- **Call-site marker spelling.** `f(mut a)` reads as what it is; `f(&a)` is shorter but imports a
  pointer connotation into a language that has no pointers.
- **Does covariance for `List` become sound?** See above — worth settling before `Type::Named`
  lands, since it changes what unification does with `args`.
- **How large is a value before flattening stops paying?** A twenty-field struct is twenty slots
  at every call boundary. Passing large values behind a hidden pointer to immutable storage is
  invisible to these semantics, but the threshold needs measuring.
- **Do `mut` parameters compose with UFCS?** `xs.push(v)` desugars to `push(xs, v)`, whose first
  parameter is `mut`. Does the receiver position need the marker, and if not, is the marker
  earning its place elsewhere?
- **Does the REPL preserve these rules across entries?** `let x = 1` at one prompt and `x = 2` at
  the next is the ergonomically expected thing and the rule-violating thing.

## Prior art

- **Swift** — value semantics with COW collections, `inout` with a law of exclusivity, `let`/`var`.
  The flagship implementation and the closest match to what is proposed, minus classes.
- **Hylo (formerly Val)** — mutable value semantics in its pure form, with no reference type at all
  and projections instead. The source of the framing; further than this proposal goes.
- **Mojo** — value semantics with explicit `owned`/`borrowed`/`inout` argument conventions on a
  Python-shaped surface; evidence the model survives contact with an ergonomic syntax.
- **V** — values by default with `mut` at declaration, parameter, and call site. The direct
  precedent for the marking scheme here, and a cautionary tale about sequencing.
- **Nim** — `var` parameters (the same `inout`), and ARC/ORC with move semantics and COW `seq`s.
  Demonstrates that move-on-last-use plus COW is enough to make copying semantics performant on a
  managed runtime.
- **Clojure** — persistent structures and transients ("mutate locally, publish immutably"), the
  model rejected as a default but worth revisiting for specific library types.
- **Go, Python, Java, Julia** — option A. Julia's `struct` versus `mutable struct` plus
  reference-semantics arrays is the hybrid froglang would drift into by inaction.
- **Rust** — aliasing-XOR-mutation, from which only non-escaping exclusive parameters are taken.
- **Jamie Brandon, "Ruminating about mutable value semantics"** — the source of the framing that
  the hard parts are large-value copies, projections, and graph-shaped data rather than the core
  rule.
