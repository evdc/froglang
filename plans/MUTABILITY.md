# Mutability and Value Semantics

Status: **implemented**, stages 1-4 and 6 of the plan below (`let`/`mut`, places, `mut`
parameters, move-on-last-use, and `push`). Stage 5 (closures) is **done** as of 2026-09-06 —
see `plans/CLOSURES.md`, and §5 below for what it cost;
stage 7 (COW via a `shared` header bit) is deliberately not attempted — see stage 6's "as built"
for why it isn't needed for correctness. The *decisions* in "Foundational choices" are what stayed
stable throughout; "Where the language actually is" below is now historical (it describes the
pre-stage-1 accidental state, kept for the reasoning that motivated the whole document).

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
| `let f = z -> z + n` | codegen panic *(fixed 2026-09-06)* | **no closures**, so no captured mutable state |
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

#### Amendment: the receiver position is exempt (`TRAITS.md` Part 2)

`TRAITS.md` introduces UFCS — `x.f(args)` resolving to a member or to `f(x, args)`. Under the rule
as stated above, the receiver form would be unusable for exactly the operations most worth
chaining: `xs.push(x)` desugars to `push(xs, x)` and is rejected for want of a marker.

**The receiver of a dot-form call is exempt from the call-site marker. Every other argument
position is not.**

```
xs.push(x)              // receiver: the mutated operand is the leftmost token
push(mut xs, x)         // prefix: buried in an argument list, marker earns its keep
swap(mut a, mut b)      // unchanged
```

The rule generalizes the paragraph above rather than contradicting it: *the marker exists to make
a mutated operand visually prominent, and the receiver position already is one.* Rust — which
cares about this more than any other language — reaches the same place via autoref: `vec.push(x)`
carries no `&mut` at the call site either.

Affordable for two reasons. First, the marker is **semantically inert**: `func_mut_params` is
consulted by `lower_call` to validate it and to compute the copy-out count, and the copy-out count
comes from the *declaration*. Nothing downstream reads the marker. Second, the property this
section protects is already half-carried by the *declaration* site — a `let`-bound variable cannot
be passed as a `mut` argument at all, so "which variables in this scope are writable" is answered
without reading any callee either way. The call-site marker adds only "…and it is written *at this
statement*".

What is given up: adding `mut` to a member's receiver parameter silently changes existing dot-form
call sites, where marking would have made it a compile error at each. Contained — it can only
affect bindings already declared `mut`, and only through the dot form.

Accepted knowingly: for a plain (non-member) function, `xs.push(x)` and `push(mut xs, x)` are the
same call under different marking rules, so a mechanical dot→prefix rewrite can change what is
legal.

Rejected alternative: exempting *the first parameter* rather than *the receiver position*. It
sounds more uniform and produces `swap(a, mut b)`, which is worse than either option.

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

**As built** (2026-09-06, `plans/CLOSURES.md`): decided exactly as written below, and it turned
out to be load-bearing in a way this section did not anticipate. By-value capture is the precise
condition under which *lambda lifting* — rewriting a captured name into an extra parameter — is
semantics-preserving with no escape analysis, and lifting is how froglang has first-class
functions at all without a closure object. So the rule that was chosen to keep aliasing out is
also what made the implementation cheap. Capturing a `mut` binding is rejected outright rather
than silently snapshotted, which is the same decision one notch stricter; a lambda reads its
captures at the point it is created, implemented as a snapshot binding rather than as a rule
anyone has to remember. The accumulator cost predicted below is exactly the cost paid.

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
4. **`mut` parameters** ✅ with the call-site marker and the single-root exclusivity check, via
   copy-in/copy-out extra Cranelift return values.
5. **Closure capture by value** — decided now, implemented when closures are.
6. **Move on last use** ✅ — split into three pieces; the analysis landed, the GC-rooting consumer
   is moot (Cranelift's stack maps own that now — RUNTIME.md Part 2), the semantic consumer
   (move-elision for a plain value) landed as `codegen::clone_if_owned`, threaded from
   `Ctx::liveness` into `compile_expr_multi`'s `Var` arm. See "Stage 6, as built" below.
   *Then* container mutation: `push(mut xs, v)` ✅, a special-cased builtin call mirroring
   `print`'s (`typeck.rs`'s `is_push`, `codegen`'s `func_name == "push"` branch), reusing the
   existing `emit_list_push`. `set` was already covered by `xs[i] = v` (stage 3). `Dict`'s mutating
   operations are unimplemented since `Dict` itself doesn't exist yet, but need nothing new here —
   they'd go through the same `clone_if_owned` mechanism.
7. **COW via the `shared` header bit** — not needed for correctness (see "Stage 6, as built"'s
   scoping argument for why); would only reduce clones further in cross-function-call patterns the
   intraprocedural liveness analysis can't see. Not attempted; revisit only if benchmarks want it.

### Stage 6, as built

The semantic consumer landed narrower than the original sketch, and the scoping turned out to be
the interesting part.

**Where cloning happens.** A GC-managed value can only ever be mutated in place through a *bare*
`mut`-rooted `List` binding — `push`/index-assignment both require `flatten_place`/`is_push` to
resolve a plain identifier root, never a struct field or a union payload. So the only type that
ever needs protecting is `Type::List(_)` at the top of a binding's own type, exactly — not "any
type with a GC-pointer leaf". A struct or union value can only ever be *rebound* wholesale, never
mutated through one alias while another alias still points at the old contents, so cloning one
would be pure waste. This was tried the broad way first (clone on any `is_heap_ty` leaf) and
reverted: an `Int | Bad`-shaped union (a `Str`-bearing member, neither of which is ever mutable)
got cloned on every `Copy`-classified read anyway, costing a real `frog_clone` call for nothing —
see `benches/pipeline.rs`'s `fallible` workload, which is exactly what caught it.

**Where in codegen.** `Ctx::liveness` (the `Liveness` `analyze_body`/`analyze_entry` already
computed, now threaded through unconditionally instead of only under `FROG_DUMP_LIVENESS`) is
consulted by `codegen::clone_if_owned`, called from exactly one place:
`compile_expr_multi`'s `TypedExprKind::Var` arm. Every non-transient consumer of a binding's value
— a bind, a call argument, a return, a struct/list/variant literal's field or element, a `Widen`,
a `Block`'s tail, a `Conditional` branch — reaches that arm automatically by ordinary recursion, so
none of them needed separate wiring.

**The one real design trap.** A first pass put the clone check directly in the `Var` funnel with no
exceptions, on the reasoning that "it's the only place a binding's value gets duplicated". That's
true for *duplication* but the funnel is also where every *transient* read goes — `Index`'s,
`FieldAccess`'s, `Slice`'s, `IsVariant`'s/`VariantField`'s, `Narrow`'s target, and a `for`-loop's
`iterable`, none of which store anything new. A scattered read inside a loop (`xs[j]` for many
`j`) is `Copy`-classified on nearly every occurrence — the name is used again by the next `j` —
which turned an O(n) read pass into an O(n²) clone storm (`benches/pipeline.rs`'s `lists` workload
went from 5.4ms to 251ms). The fix was `compile_expr_transient`/`compile_expr_multi_transient`,
which those specific call sites use instead — they bypass the clone check via `read_var_raw`
directly, since reading a pointer only to address through it can never need a clone.

**A known, pre-existing, and still-open gap**: reading a leaf back out of a container — a list
element, a struct/union field, a narrowed match arm — is not itself a `Var` node, so a value
extracted that way and then bound to a new `mut` name is never cloned, even if it's later `push`ed.
This predates this work (nothing was ever cloned on that path) and stays out of scope for the same
reason nested-index place assignment does: no surface syntax reaches it, since `push` and place
assignment both require a bare identifier root, never `xs[0].listField`.

Measured on `benches/pipeline.rs`: `fallible`, `structs`, `lists`, `orders`, and every other
existing workload are unchanged within noise against the pre-stage-6 baseline. Two new workloads,
`list_push_loop` and `list_push_aliased`, demonstrate the elision claim directly — the former costs
about what `lists`' comprehension build costs (`push`'s own receiver is always `Move`-classified,
per `liveness.rs`'s `Call`/`mut_args` handling), the latter shows one measurable, once-per-round
clone cost rather than a per-element one.

### Stage 6 in detail

Split into a liveness analysis (frontend, name-level) and two consumers, per the plan the
analysis itself sketched.

- **Node identity + the analysis.** ✅ `TypedExpr` gained a `NodeId`, stamped by a post-lowering
  renumbering pass (`liveness::number_nodes`) rather than at construction, since lowering clones
  subtrees (a guarded match arm's `tail`, a `catch` handler inlined per `Error` member) and a
  construction-time id would let clones share one. `liveness::analyze_body`/`analyze_entry` is a
  backward, name-level dataflow pass — no CFG exists on the froglang side, so it's a structured
  fold mirroring `compile_expr_multi`'s own evaluation order arm for arm, with a bounded fixpoint
  over the one back-edge in the language (`for`-loop bodies). Output is `Ownership::{Copy, Move}`
  per `Var` node id plus `dead_after(stmt)` per statement boundary, both side tables keyed by
  `NodeId` rather than new AST variants.
- **GC root minimization.** ❌ **Attempted and reverted — do not retry this shape.** Inspection
  showed the existing per-producer shadow-slot walk was already leaf-type- and allocation-precise
  (`apply` in `benches/orders.frog` needed zero slots *before* this work, contrary to this doc's
  own prediction), so the apparent waste was that `Conditional`'s two branches summed their slot
  needs instead of sharing them, even though exactly one branch ever runs. Sharing them
  (`max_heap_slots` combining by `max`, `compile_conditional` resetting `ctx.heap_cursor` before
  the false branch) cut `discount_for`'s frame from 5 slots to 1 and touched every construct, since
  `match`/`?`/`!`/`catch` all desugar to nested `Conditional`. `orders`' wall-clock didn't move —
  it's allocation/GC-bound, not frame-setup-bound.

  It was also **unsound**, and the `FROG_GC_STRESS=1` sweep that "verified" it simply had no test
  of the failing shape. Mutual exclusion holds within one execution, not across a loop back-edge: a
  value produced in one branch and assigned to a binding declared outside the loop is rooted only
  in that shared slot, and the next iteration taking the other branch overwrites it while the value
  is still live. Reverted; `tests/test_gc_roots.rs` now covers it.

  The general invariant the whole scheme rests on: **a shadow slot may be overwritten only where
  its current occupant is dead.** Without reuse that holds vacuously, which is why the pre-existing
  code was correct without ever arguing for it. Any sharing has to discharge it with real live
  ranges — share iff non-interfering — not with a structural mutual-exclusion argument.

  Chasing that also turned up a **pre-existing hole of the same shape, still open**: loop bodies
  reuse their slots across the back-edge, so a producer inside a loop overwrites its own slot on
  the next iteration. Since `let s = <producer>` gives `s` no root of its own — it borrows the
  producer's — a binding fed from inside a loop and read after it loses its root. See the ignored
  test in `tests/test_gc_roots.rs`. This is not fixable by removing a reuse (the back-edge reuse is
  inherent); it needs the "binding roots" half of the split this bullet originally sketched and
  then skipped. Which makes that split load-bearing for *correctness*, not just for frame size —
  it should be re-planned as such, together with the third bullet below, which independently
  concluded it needs the same thing.
- **Move on last use as a semantic consumer.** Not done, and not safe to bolt on to the current
  slot scheme. Root slots are owned by *producer site*, not by *binding name* — `let x = y` binds
  `x` to whatever slot already roots `y` (a plain `Var` read costs zero new slots), so multiple
  names can share one slot through aliasing. Clearing "x's slot" at x's last use, on top of that,
  would in that case clear a slot `y` might still need. Acting on `Move` safely needs slots owned
  per binding name instead — updated at every def of that name, so aliasing between names is
  moot — which is a bigger change than this pass makes, not a follow-on to it. What *is* done:
  `Ownership`/`dead_after` are computed for every function body and REPL entry, and a
  `FROG_DUMP_LIVENESS=1` env var prints every `Var` occurrence's mark to stderr — the only way to
  inspect the analysis today, and what a future semantic consumer (copy elision, `mut` container
  operations) should be checked against.

### Stage 6a: shadow-slot allocation — RETIRED, superseded by RUNTIME.md Part 2 (implemented)

**Do not implement this section.** It is kept for the reasoning, which still holds; its
conclusion does not. GC roots are now Cranelift's own user stack maps (RUNTIME.md Part 2 —
implemented, not just designed), which subsumed the entire allocation problem below before it
was ever built: there are no slots to allocate. The blocker had been that stack-map liveness is
*use-driven*, so a union's `(tag, payload)` pair could not be declared while its payload's
pointer-ness was dynamic; RUNTIME.md Part 1's tagged-pointer representation resolved that. The
open loop back-edge hole this section's reasoning identified is closed too — see
`tests/test_gc_roots.rs::loop_body_producer_does_not_clobber_an_escaping_binding`, which was
`#[ignore]`d until Part 2 landed and now passes.

The original sketch follows.

---

Three separate conclusions above all land on the same missing piece — the reverted branch sharing
needs live ranges to be sound, the open loop back-edge hole needs binding-owned roots, and move-on-
last-use needs binding-owned roots. So build that piece once, as a real allocation pass, rather
than three times as local patches.

**The shape.** Replace the two hand-synced walks (`max_heap_slots`' counting walk and
`compile_expr_multi`'s `ctx.heap_cursor` bump) with one pass that computes live ranges for heap
roots and assigns slot indices, and hands codegen a `NodeId -> slot` map plus the frame size.
Sharing then falls out of non-interference rather than a per-construct argument.

1. **Roots are owned per binding, not per producer.** A heap-typed binding gets a slot written at
   every def of that name and held for its live range. That closes the loop back-edge hole (`best`
   has a root that does not depend on a producer inside the loop not re-running) and makes
   aliasing between names moot, which is exactly what the third bullet above says `Move` needs.
2. **Temporaries get producer→last-use ranges.** A use ends at consumption into a binding, into an
   ABI call, or into an already-rooted heap object — past that the parent's root covers it
   transitively. Slots for temporaries whose ranges don't overlap can coincide, which recovers the
   `Conditional` win *and* beats it: the cursor could never share across sequential statements or
   sibling calls, and this can.
3. **Back-edges need no special case.** `analyze_loop`'s bounded fixpoint already extends live
   ranges across the one back-edge in the language, so a value that escapes a loop interferes with
   everything in the body and gets its own slot by construction.
4. **Frame size is max simultaneous live roots**, not producer count — and it comes out of the same
   pass that assigns the indices, so the "must stay in exact lockstep" hazard that `Ctx.heap_max`
   and `max_heap_slots` both warn about stops being a hazard. That coupling is what let the
   reverted change look verified while being wrong; it is worth removing on its own merits.

**Prerequisite.** `liveness.rs` is name-level today. Slot allocation needs node-level ranges for
temporaries too. `NodeId`s and the fixpoint are already there, so this extends the pass rather than
replacing it.

**How to know it's right.** The invariant is checkable: at every overwrite of a slot, the previous
occupant is dead. That is worth asserting in a debug build directly, rather than inferring it from
a passing stress sweep — the sweep is only as good as the shapes the suite happens to contain, as
the revert above demonstrated. Grow `tests/test_gc_roots.rs` with the escape shapes first
(including un-ignoring the loop one), then measure `discount_for`'s frame again.

### Stage 7: copy-on-write — tier 3, and the elision that makes tier 1 affordable

Stage 6 shipped tiers 1 and 2: an eager deep copy at every `Copy`-classified `List`-typed `Var`
read, skipped where liveness says `Move`. Tier 3 is the missing piece, and its absence turns out
not to be only a performance problem — eager copying is expensive enough that Stage 6 had to leave
a hole in tier 1 to stay affordable, and that hole is a live value-semantics bug.

**What is actually broken.** Reading a leaf back out of a container is not a `Var` node, so it
never cloned. Stage 6 recorded this and deferred it on the grounds that no surface syntax reached
it. That was true of place assignment, which requires a bare identifier root, but `mut x = <a
container read>` reaches it fine. Three shapes, all reproducible before this stage:

```
mut rows = [[1,2],[3,4]]; mut inner = rows[0]; push(mut inner, 9)
  → rows becomes [[1,2,9],[3,4]]

mut rows = [[1,2],[3,4]]; for row in rows { mut r = row; push(mut r, 7) }
  → rows becomes [[1,2,7],[3,4,7]]

data Box(items: List<Int>); let b = Box(items=[1,2]); mut got = b.items; push(mut got, 3)
  → b.items becomes [1,2,3]
```

Closing these under eager copying means cloning on every element read, which is exactly the O(n²)
storm `compile_expr_multi_transient` exists to prevent (Stage 6: `benches/pipeline.rs`'s `lists`
workload, 5.4ms → 251ms). Under copy-on-write it costs a byte store. **That is the argument for
this stage** — not only that it is faster, but that it makes the correct thing cheap enough to do.

**What it costs today.** `benches/life.frog` passes a `List<List<Int>>` to a function about nine
times per cell and never mutates it. Measured 2026-08-29: 78% of the run inside
`GcHeap::clone_obj`, and removing the clone entirely (unsound, for measurement only) takes it from
138s to 167ms — an 800x difference on the same checksum. `benches/words.frog` pays about 8% of the
same cost passing a `List<Str>` vocabulary down; `benches/orders.frog` pays none, which is why this
stayed invisible until `life` existed.

#### The rule

One bit in `GcHeader`, which has room for it — the struct is 16 bytes today (`*mut GcHeader`,
`bool`, `ObjKind`) with 6 of padding.

```
shared = false   on every fresh allocation, and on everything `clone_obj` produces
shared = true    wherever a second live path to the object is created
write through a mut root:  if shared { root = clone(root) }  then write
```

The invariant, stated once:

> **If a heap object is reachable by more than one path that can still be read, its `shared` bit is
> set.**

The slack is one-directional and that is what makes the scheme tractable: a spuriously set bit
costs one extra copy, a missed bit is an aliasing bug. Every rule below is therefore allowed to be
conservative and never has to be precise.

**The trigger is the analysis Stage 6 already computed.** `Ownership::Copy` means "the source name
is still live after this read", which is precisely "a second path now exists". So the change at
`codegen::clone_if_owned` is not new analysis — it is the same call site under the same predicate,
with an O(1) header store in place of an O(n) `frog_clone`. `Move` reads set nothing, so tier 2
keeps working unchanged, and `push`'s own receiver (always `Move`, via `liveness.rs`'s `Call` arm
removing a `mut` argument's name from `live_out`) stays allocation-free in a loop.

| program | read | bit | write | result |
|---|---|---|---|---|
| `mut a=[..]; let b=a; push(mut a,4)` | `a` Copy | set | copies | `b` intact |
| `mut a=[..]; mut b=a; push(mut b,4)` (`a` dead after) | `a` Move | none | in place | nothing observes |
| `let a=[..]; mut b=a; push(mut b,4); print(a)` | `a` Copy | set | copies | `a` intact |
| `addOne(mut b)` | mut arg → Move | none | in place | copy-out is a no-op rebind |
| `cell_at(rows,…)` ×9 per cell | `rows` Copy | set (idempotent) | never written | **no copies at all** |

The `mut`-argument row is worth spelling out: `Move` classification does not *clear* an
already-set bit, which is what keeps `let s = a; addOne(mut a)` correct — `s`'s aliasing set the
bit earlier, so the callee's `push` copies and the caller receives the copy through copy-out.

#### The write barrier is a two-site surface

This is what keeps the stage small, and it holds for a reason worth recording: **every mutation in
the language requires a bare identifier root.** `is_push` and `flatten_place` both enforce it, and
typeck rejects `rows[0][1] = 99` outright ("assignment through more than one list index isn't
supported yet"). So there are exactly two mutation sites, and at both of them codegen holds the
root's Cranelift `Variable` and can rebind it:

```
p = use_var(root)
if shared(p) { p = frog_clone(p); def_var(root, p) }
<store>
```

One well-predicted branch per write, and self-extinguishing: after the first copy the object is
unique, so the rest of the loop runs barrier-free.

#### Why a shallow bit suffices for nested lists

The bit lives on the outer object and `clone_obj` is deep. Those two facts have to be paired, and
together they cover every case:

- Write through the outer (`push(mut rows, r)`, `rows[0] = r`) — outer is shared, so the deep copy
  makes the whole subtree fresh and unique before the write lands.
- Write through an extracted inner (`mut inner = rows[0]; push(mut inner, 9)`) — the extraction
  set the bit on the *inner*, so the inner copies and `rows` is untouched.
- There is no third case, because nested place assignment does not exist.

That second bullet is the new rule: **reading a `List`-typed leaf out of a container sets that
leaf's bit** — list element read, struct/variant field read, and the `for`-loop element bind. All
O(1), which is the whole reason this is affordable now and was not before. This is what closes the
three bugs above.

#### The cliff, accepted

A monotone bit cannot observe that an alias has *died*:

```
for i in 0..n { let snap = xs; push(mut xs, i) }     // O(n²)
```

Each iteration re-shares and re-copies even though `snap` is dead at the end of it. Refcounting
would get this; one bit cannot. Swift has the same cliff. Accepted and documented rather than
reached for — froglang has a tracing collector, and adding refcounts to recover uniqueness is a far
larger bargain than this shape justifies. Revisit only if a real program hits it.

#### How to know it's right

The failure mode that matters is a *missed* bit, which is invisible to ordinary tests until some
program happens to observe a mutation through a stale alias. So, in the same spirit as
`FROG_GC_STRESS`:

**`FROG_COW_VERIFY`** — at every write barrier, when the object is *not* marked shared, mark from
every root (the collector's own precise machinery) and count the live references that reach it.
More than one means a bit was missed; abort naming the site. Far too slow for anything but a test
run, and it turns "did we cover every aliasing site" from a code-review argument into a suite
property. Stage 6's reverted GC-root sharing is the cautionary precedent: a stress sweep is only as
good as the shapes the suite happens to contain.

Two things it took to make this usable, both worth recording because both are traps:

- **Count JIT stack-map slots, not roots.** One top-level binding is rooted three times over — as
  a rebuilt `env` root, as a slot in `call_jit`'s conservatively scanned out-buffer, and as the JIT
  value the entry is using — so counting roots made every top-level `xs[i] = v` look aliased.
  `tests/programs/mut_list_index_assign.frog` is the case that caught it. Reachability still traces
  from every root; only the count is narrowed.
- **It is a strong check, not a complete one.** Two bindings holding the *identical* SSA value
  share one spill slot and count once, so `mut got = b.items` — where the field read yields the
  same `Value` the struct's leaf holds — is not caught. Deliberately sabotaging
  `mark_shared_extracted` fires for a list element and a `for`-loop binding but not for that struct
  field. The behavioural assertions in `tests/test_value_semantics.rs` remain the specification;
  this catches the class of mistake that would otherwise go unnoticed between them.

The three bugs above and the five table rows go into `tests/test_value_semantics.rs`.

#### Sequencing

1. `shared` in `GcHeader`; `clone_obj` clears it on everything it produces. Inert.
2. Write barrier at `push` and `PlaceAssign`. Still inert — nothing sets the bit yet.
3. Flip `clone_if_owned` to mark instead of clone. **COW goes live here.** The existing suite must
   stay green and the five table rows are added.
4. Set the bit on container extraction. The three bugs close here — this is the only step that
   changes the observable behaviour of programs that already run.
5. `FROG_COW_VERIFY`.

Steps 1 and 2 are provably no-ops, which keeps the semantically risky commit small.

#### Sharp corners found while auditing Stage 7

**Non-aliasing readers must be transient, or ordinary loops go quadratic.** A read that stores
nothing creates no alias, so marking it shared is pure loss — and worse than loss, because the
*next* write then copies, re-marks and re-copies. Three were routed through `compile_expr` rather
than `compile_expr_transient`, each turning an O(n) loop into an O(n²) one:

```
for i in 0..n { s = s + xs.len();  push(mut xs, i) }     147ms -> 11.8ms at n=40000
for i in 0..n { print(xs);         push(mut xs, i) }
for i in 0..n { if xs == small ..; push(mut xs, i) }      51ms -> 12.8ms at n=20000
```

All three fixed. The general rule, worth applying to any future builtin: **if it reads a list and
stores nothing, it is transient.** `Index`, `Slice`, `FieldAccess` and a loop's `iterable` already
were; `len`, `print` and structural `==` were not. This class of mistake is invisible to the test
suite (it changes no output) and invisible to `FROG_COW_VERIFY` (over-marking is always sound), so
it needs a benchmark or an audit to catch — `benches/pipeline.rs` is the right home for a
regression workload.

**The remaining cliff: passing a list to a non-`mut` parameter.** Still quadratic in a mutation
loop, measured 51.6ms against an 11.5ms baseline at n=20000:

```
func peek(ys: List<Int>): Int = ys.len()
for i in 0..n { s = s + peek(xs);  push(mut xs, i) }
```

Unlike the three above, this mark is *correct*: the callee's parameter is a second binding. Two
things would be needed to elide it, and they only work together:

1. Mark at `mut` binding sites, so a callee rebinding an immutable parameter into a `mut` local
   (`mut ys = xs; push(mut ys, ..)`) marks rather than aliasing the caller's list. This is the
   `Assign.mutable` plumbing the "rejected" section below turned down — but its cost/benefit is
   different now that a mark is a byte store rather than a deep copy.
2. A per-function *escape* summary: does any parameter's value reach a return position (directly,
   or inside a returned container)? If not, the caller needn't mark. Whole-program compilation
   through one `FrogState` makes this tractable, and it is the same shape of analysis the inliner
   would want. Without it the elision is unsound — `func f(xs) = xs` hands the caller an alias the
   call site cannot see.

**Expressiveness gaps that are implementation limits, not semantic ones.** Every one of these is
ordinary in Swift/Nim/V, and each was forbidden by where the mutation surface was drawn (a bare
identifier root) rather than by mutable value semantics. **All three are fixed in Stage 8 below.**

- `rows[y][x] = v` — rejected outright ("assignment through more than one list index isn't
  supported yet"). This is the Game-of-Life shape; `benches/life.frog` rebuilds whole grids from
  comprehensions partly because it cannot write one cell.
- `push(mut b.items, v)` — rejected, **while `b.items[0] = v` works**. The place path already
  supports a field prefix; `push` does not, so the two mutations disagree about what a root is.
  Worth aligning: `is_push` should accept the same paths `flatten_place` does.
- `push(mut rows[0], v)` — rejected for the same reason.

Under copy-on-write these become tractable in a way they were not before: the write barrier needs
the *root* to rebind, and for a path it would need to unshare each level along the way
(`emit_unshare` per segment), which is O(depth) rather than O(size).

**Unrelated bug found in passing.** `mut xs = []` followed by `print(xs)` prints `<?>` placeholders
— the element type is still an unresolved type variable on that `Var` node when `print`'s
type-directed codegen reads it. Annotating (`mut xs: List<Int> = []`) fixes it. Reproduces
identically before Stage 7; it is a missing final resolution pass over the typed AST, not an
aliasing issue.

#### Rejected: static elision of the copy instead

The alternative is to keep eager copying but skip it where the destination provably cannot be
mutated — pass to a non-`mut` parameter without copying, and compensate by copying at `mut` binding
sites. It is sound, and it fixes `life`. It was rejected because it needs `mutable` plumbed into
`TypedExprKind::Assign` (9 construction sites, ~23 match sites), it still pays O(n) on every
`mut x = <existing list>`, and it does nothing for the three extraction bugs — closing those still
costs a clone per element read. Copy-on-write subsumes it at lower runtime cost.

Worth revisiting later as a *specialization* on top of this: a list type that no reachable code
ever mutates needs neither the bit nor the barrier. As an escape analysis that is fragile under
generics and monomorphization, so it belongs on top of a correct dynamic scheme rather than in
place of one.

Stages 1–4 are frontend work with no runtime component and can land before generics. Stage 6 is
the one with a dependency in both directions: it needs the liveness analysis, and container
mutation needs it.

### Stage 8: paths as mutable places (implemented)

Stage 7's audit left three expressiveness gaps, all with one cause: the two mutations disagreed
about what a mutation target was. Assignment took a *place* — a root binding plus a `.field` /
`[index]` path — while `push` took a bare identifier. So `b.items[0] = v` worked and
`push(mut b.items, v)` did not, and neither `rows[y][x] = v` nor `push(mut rows[0], v)` worked at
all. This is Hylo's *projection* idea in the narrow form the language can afford: not a general
`subscript` that can `yield` access to a computed part, but the concrete "root plus static path"
case, which is the shape the writable-keypath sketch in Racordon et al. §4 proposes for exactly the
managed-runtime setting we are in.

#### One representation for every mutation

`typed_ast::Place { root, path }` is what a mutation names. In typeck, `lower_place` walks the path
once — resolving each segment's type, checking the root is mutable — and every mutation calls it, so
they cannot drift apart again.

Three things carry a `Place`: `PlaceAssign { place, value }`, and — the part that makes this
extensible — **a `mut` call argument**, via `Arg::Mut(Place)`:

    pub enum Arg { Value(Spanned<TypedExpr>), Mut(Place) }

    Call { callable: TypedExprRef, args: Vec<Arg> }      // `mut_args: Vec<bool>` is gone

The first attempt gave `push` a `TypedExprKind` variant of its own so its receiver could be a path.
That worked, but it was the wrong axis. A node costs a handler in **every** walker — 14 sites across
typeck, liveness, linear and codegen — and that repeats for `pop`, `insert`, `remove` and every
`Map` operation. Worse, it put the generality in the wrong place: the *builtin* could take a path
while a user's own `func f(mut xs: List<Int>)` still could not, so `f(mut b.items)` was rejected.

Carrying the place in the argument fixes both. A new mutating builtin needs one arm in
`compile_call` and nothing else — the per-operation cost is codegen, which is inherent and cheap,
not AST surgery. And `f(mut b.items)` / `f(mut rows[1])` now work, because a `mut` parameter is
resolved, loaded and copied back out through the same `PlaceRef` as everything else. `push` went
back to being a call dispatched on its callee name, exactly as `print` and `len` already are.

Parsing changed too: `Grammar::mut_prefix` parses a full place at `Precedence::Call` (via a new
`Parser::continue_expression`, the infix loop factored out of `Parser::expression`) instead of
accepting only a bare identifier.

**Still restricted:** a user-defined mutating *method*'s receiver (`b.bump()` where `bump` takes
`mut self`) must be a bare binding — those two call paths build a `Place` with an empty path from
the receiver's root name. `b.items.push(3)` works, since `push` resolves its receiver with
`lower_place`, but the general case is unfinished. See roadmap.md.

#### A liveness rule that changed with it

A `mut` argument's root is **read-modify-write**: live going into the call, rebound by the copy-out.
Anything that aliased it earlier must therefore see a `Copy` and mark.

The old code did the opposite — it *removed* the root from `live_out` — and was still correct only
because the argument was a `Var` node whose own transfer added it straight back; the removal existed
solely to make that read a last use so it would not clone. Reading a place creates no `Var`
occurrence at all, so translating the removal literally was a soundness bug:
`let c = b; add(mut b.items)` made `let c = b` a last use, nothing marked the list, and the callee's
push landed where `c` could see it. The clone-elision the removal used to buy is structural now —
`emit_place_ref` reads the place directly and never marks — so the rule is simply
`live_out ∪ {root}`, the same one `PlaceAssign` already used.

#### Walking a path: unshare down, write back

`codegen::emit_place_ref` walks a place, unsharing every list *above* its leaf, and returns where
that leaf lives — flattened `Variable`s for a pure field path, or a slot in a list. `place_load` and
`place_store` then read and write it, which is all a `mut` argument's copy-in/copy-out needs:

    rows = unshare(rows)              // rebind the root's Variable
    inner = list_get(rows, y)
    inner = unshare(inner)            // may replace it with a private copy
    list_set(rows, y, inner)          // ...so write the new pointer back
    // ...repeat per level; the final write lands in `inner`

Two things make this correct. **Every list on the path is unshared, not just the innermost**: writing
into `rows[y]`'s buffer is observable through any other binding that can still reach `rows`. And
**each unshared pointer is stored back into its parent slot** before the walk descends — that
write-back is what keeps the chain connected without reference counts, since unsharing may replace a
list with a copy.

It stays O(depth), not O(size). `emit_unshare` copies only when the `shared` bit is set, and
`GcHeap::clone_obj` is a *deep* clone, so once an outer list has been copied every list below it is
already private and the remaining steps are pure tests.

#### A soundness hole this exposed

`mark_shared_if_aliased` marked a copied binding only when the binding's *own* type was `List`. So
copying a struct that holds a list marked nothing, and

    mut b = Box(items=[1, 2])
    let c = b
    b.items[0] = 99          // c.items now reads [99, 2]

was wrong. This predates Stage 8 — `flatten_place` has always accepted a field prefix — and it
survived the Stage 7 audit because that function's own doc comment asserted the opposite, that a
mutation "requires a bare `mut`-rooted `List` binding ... never a struct field". The fix is to mark
every `List` leaf of a copied value, which is what `mark_shared_extracted` already did for values
read out of containers; the two now share it. No benchmark moved.

Worth recording how it was *not* found: `FROG_COW_VERIFY` does not catch it. Two bindings holding
the identical SSA value share one spill slot, so `count_refs` sees one reference — the limitation
already documented under "How to know it's right". Deliberately disabling all marking still leaves
this program silently wrong under the verifier, while the extraction shapes abort as they should.
It took writing the test.

#### The verifier learned about depth

A nested write legitimately has *two* live references to the list it mutates in place: the JIT slot
holding the pointer, and the parent list's own slot. Rather than relaxing the check to `> 2`
everywhere, `frog_cow_verify` now takes how many references are expected at that position — 1 at a
place rooted directly in a binding, 2 for a list reached through another list. The outer case stays
as strict as it was, which is what a sabotage run confirms.

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
