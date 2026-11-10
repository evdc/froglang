# First-class functions

Status: **tier 1 implemented** (2026-09-06) — `TypeChecker::lower_function_values`
(`frontend/typeck.rs`), `tests/test_closures.rs`. Tier 1.5 (function pointers) and tier 2
(escaping closures) are design only; see "Beyond tier 1". The "Foundational choices" table
is the part meant to be stable.

This supersedes the roadmap's "Top-level `let` bindings aren't capturable from functions"
bullet, and answers the question `plans/MUTABILITY.md` Stage 5 parks on ("closures wait on
closures existing at all").

## What existed before

Verified against the tree, not remembered — and the finding reframed the whole task.

**The type checker was already done.** `frog check` accepted every higher-order form:

```
func apply(f: (Int -> Int), x: Int): Int = f(x)   ;   apply(y -> y + 1, 2)
func mapped<A, B>(xs: List<A>, f: (A -> B)): List<B> = [for x in xs do f(x)]
```

`Type::Function` was spellable in every annotation site, lambdas inferred their parameter
types from the expected type, and generic higher-order functions unified fine. The only
thing in the way was `validate_codegen_constraints`, which rejected a function in value
position because codegen had no representation for one. So this was a **lowering problem,
not a type-system problem**, and nothing in inference, unification, or `Type` moved.

Codegen's two limits, both narrow and both exactly what the pass now removes:

| limit | symptom |
| --- | --- |
| `compile_entry` Pass 1 declares only *top-level* `Assign { value: Function }` statements | a nested `func` panicked with `no entry found for key` |
| Pass 2 seeds a body's variables only from its parameters | a function reading a top-level `let` panicked with `unbound variable in codegen` |

Both were crashes on code the type checker had already accepted, which made them the worst
failure mode in the tree.

## Foundational choices

| Question | Decision | Why |
| --- | --- | --- |
| Closure representation | **None.** Function values are compiled away | No GC-traced environment, no indirect call, no calling-convention change |
| How | Lambda lifting + per-callee specialization | Both are transforms froglang already performs for types (`monomorphize_generics`); this is the same shape keyed on a name |
| Capture | **By value, immutable** | Keeps mutable value semantics intact, and is the exact condition under which lifting a capture into a parameter is sound without escape analysis |
| Capturing a `mut` | Rejected, with the fix in the message | Java's effectively-final rule. The reversible choice: allowing it later accepts strictly more programs |
| When capture is read | Where the function value is **created** | The definition of by-value capture; implemented as a snapshot binding, not as a rule to remember |
| Escaping function values | Rejected, naming why | Tier 2. A restriction that makes the elimination *total* rather than best-effort |

### Why by-value capture is load-bearing rather than tidy

It is what makes lambda lifting — turning a captured name into an extra parameter — a
semantics-preserving transform. Under by-reference capture, lifting silently changes
behaviour: mutations through the captured name stop propagating, and the compiler needs
escape analysis to know when it may lift at all. Decide by-value and the optimization is a
mechanical rewrite; decide by-reference and it becomes a research problem.

The cost is real and worth stating: no accumulator or memoizing closures, ever. `mut`
parameters (copy-in/copy-out) and folds are the replacement, and both already exist.

### Prior art

The design space splits on **closed world vs. separate compilation**, and froglang is
firmly closed-world: it compiles whole programs at eval time and already monomorphizes.

- **Futhark** — higher-order functions in the source, *zero* function values at runtime,
  guaranteed by defunctionalization, bought with language restrictions on where a function
  value may flow. The closest match to what tier 1 does and why.
- **Roc** — the general version (lambda sets: a singleton set compiles to a direct call, a
  larger one to a tag dispatch). No restrictions, considerably more machinery.
- **Rust** — the same result without a dedicated pass, since each closure has a unique
  anonymous type and generics monomorphize per closure. The bill arrives as compile time.
- **Zig** — declines: no closures, `comptime` function parameters are the idiom.
- **Hylo/Val** — the other mutable-value-semantics language, and it requires explicit
  capture lists for exactly the reason above.
- **Go** — captures variables rather than values, and got a decade of the loop-variable bug
  for it. A cautionary tale, not a model.
- **Swift** — flipped its default to non-escaping, because a non-escaping closure needs no
  heap allocation and no ARC traffic. The same payoff.

## As built

One pass, `lower_function_values`, run after `monomorphize_generics` and before
`desugar_notation`, from `FrogState::eval_with_base`, `codegen::compile_and_run`, and
`frog check`.

The order against monomorphization only works one way: monomorphization keys on the whole
function *type*, so two different lambdas of type `(Int -> Int)` both land in
`mapped$Int$Int`, and specialization then separates them. Substituting a name never creates
a new type instantiation, so no iteration back is needed.

Four steps:

1. **Resolve stored types.** A lambda in argument position takes its parameter types from
   the expected type, so its `Function` node carries the fresh `TypeVar`s unification later
   pins. Hoisting turns those into a *signature* codegen reads directly, so they have to be
   real first. (`monomorphize_generics` resolves the same way for the same reason, but
   returns early when nothing is generic — this cannot rely on it having run.)
2. **Hoist.** Every `Function` node that is not already a top-level declaration's value is
   moved to the top level under a fresh unspellable name. A nested `func`/`let f = ...`
   leaves a rewrite behind for the rest of its block; a lambda in expression position is
   replaced by a `Var` naming its own hoisted declaration — which is exactly the
   "statically known callee" shape step 4 consumes. Each hoisted declaration's captures are
   **snapshotted** where the declaration stood.
3. **Specialize.** A call whose callee is a retained higher-order template and whose
   function-typed arguments are all plain names is rewritten to a clone of that callee with
   each one substituted in, so `f(x)` in the body becomes a direct call. Run to a fixed
   point, since a clone's body can hold the first specializable call to something else. The
   un-substituted template is stripped (`make_sig` has no Cranelift type for a
   function-typed parameter) and retained for later entries — exactly what
   `monomorphize_generics` does with a generic declaration.
4. **Captures.** Each function's free value names become trailing parameters and every call
   site passes them, to a fixed point: if `f` calls `g` and `g` gained captures `f` doesn't
   bind, `f` gains them too.

Specialization runs *before* captures deliberately. A clone that calls the lambda it was
specialized on is an ordinary caller of a capturing function, so step 4's fixed point
threads that lambda's captures through it with no forwarding logic of its own. The other
order needs one.

### What the pass had to get right that a first sketch does not

- **Scope-awareness.** `walk_vars_mut` is scope-blind, which is fine for its own job
  (template symbols are unspellable, so nothing can shadow them) and wrong here, where the
  names being rewritten are ordinary user identifiers. Both the capture analysis and every
  rename track binders — parameters, `let`, loop variables, match-arm and `catch` binds.
- **Snapshots, not call-site reads.** Passing a capture as a plain `Var(n)` at each call
  site is wrong, because a call site may shadow `n`:

  ```
  let n = 1
  let f = x -> x + n
  let g = { let n = 1000; f(0) }   // f must still see 1
  ```

  Binding each capture to a fresh unspellable name where the declaration stood fixes that
  *and* is the definition of by-value capture, rather than two separate mechanisms.
- **A name is not an identity.** `fn_templates` and `lifted_captures` are name-keyed and
  persist across REPL entries, so a redeclaration has to clear them — otherwise a later,
  perfectly ordinary `func apply(x: Int)` is stripped as if it were still the higher-order
  one. `monomorphize_generics` sidesteps this by keying on a unique symbol; this pass
  cannot, because the name is what a call site carries.
- **Rewrite unconditionally, emit conditionally.** A specialization already compiled — in
  an earlier round or an earlier *entry* — builds nothing, but its call sites here still
  have to be pointed at it. A fixed point that only rewrote when it had just emitted
  something left a cross-entry call naming a stripped template.
- **Mutability is not recoverable from the typed AST.** A `let` declaration and a later
  reassignment are the same `Assign` node, so "assigned twice" cannot tell a re-`let` in an
  inner scope from a mutation, and rejected legal shadowing. `mut_names` records the answer
  during lowering instead.

### Cost

Codegen was not touched. Neither were the GC, `liveness.rs` or `linear.rs` — the last two
already *assume* functions appear only as top-level declarations, so the pass strengthens
what they rely on. A captured `List` or `Str` becomes an ordinary parameter, and parameters
are already roots, so `FROG_GC_STRESS=1` needed nothing new.

Compile time, measured A/B on a 13,000-statement program: **0.07s → 0.08s**. Noise on
anything real; not worth a guard.

## Beyond tier 1 — the design space (2026-11-10)

Tier 1 covers passing a function *down*: to a callee the compiler can see, captures
included. What it rejects is a function value that **escapes**: stored in a list, dict, or
struct/union field, returned, or bound to a `mut`. That is the ordinary-programming case:
dispatch tables, a handler held in a struct, a returned configured function.

Why this is tractable here at all: capture is by value and immutable, and froglang has no
references, so an environment is an *owned, immutable record*. A closure is a tree like any
other value: copy copies the record, drop drops it, and no collector or refcount is needed
(`plans/NOGC.md` §1.4 item 2). The costs that make closures hard elsewhere (Rust's
lifetimes, Swift's ARC and `@escaping`, Go's loop-variable capture) all come from
capturing references.

| Option | Representation | Covers | Cost | Fit |
| --- | --- | --- | --- | --- |
| A. Status quo | none; a union + `match` by hand | passing down | none | what `SELFHOST.md` plans for `froggc` anyway |
| **B. Tier 1.5: function pointers** | one word: a capture-free function's code | dict/list dispatch, a named handler in a struct, returning a named function | small | works across REPL entries and for the host |
| **C. Tier 2: closures** | `{code, env, vtable}`, indirect call | everything, incl. returned capturing closures and partial application | medium–large | the shape `NOGC.md` §1.4 already names |
| D. Lambda sets (Roc) | per-type set of lambdas → tagged union, call = `match` | everything, no indirect calls | large | **poor** — needs a closed world |

**Lambda sets are rejected.** They look attractive because boxed tagged unions already
exist to build them on, but they need every lambda that can reach a value to be known when
that value's code is compiled. `FrogState` compiles one REPL entry at a time: a later
entry that stores a new lambda into a dict built by an earlier one adds a variant the
earlier code cannot dispatch on. Host embedding has the same problem. Roc's experience is
also that lambda-set inference is a long-running source of compiler bugs.

**Plan: build B, and lay it out as the degenerate case of C.** If a function-pointer value
is `{code, env = null}`, tier 2 later only adds a non-null environment and a vtable. Nothing
already stored or compiled changes shape. Tier 1 stays the fast path whenever the callee is
statically known, so the indirect call is only ever paid for a value that really escapes.

### Tier 1.5 — function pointers

- **What qualifies:** a top-level `func`, or a lambda with no captures (after hoisting, the
  same thing). A capturing function in an escaping position stays rejected, with a message
  pointing at a union + `match` until tier 2 exists.
- **Codegen:** a scalar leaf holding the function's address, and Cranelift `call_indirect`
  with the signature derived from the `Function` type. No drop, and copy is a word copy.
- **Conventions:** an indirect call has no callee name for `conventions::infer` to resolve,
  so either the function type fixes the convention (below), or every indirect call is
  treated as consuming. Starting with "consuming" is safe and only costs copies, which
  `check --copies` reports.
- **Traits:** no `Eq`, no `Hash` (so never a dict *key*), no `repr`/JSON/`read`. Structural
  derivation already makes a struct holding one lose `Eq`.
- **Also fixes:** `{ "a": f }` today reports "Unknown type 'f'" instead of the "function used
  as a value" message.

### Tier 2 — escaping closures

A capturing function value that escapes needs `(code, owned env)` plus a per-closure vtable
(copy, drop, `abort`), because the type `(Int -> Int)` erases what the environment holds. Not
built. Decide these before building it:

1. **The convention of an indirect call.** Either the function type carries borrow/consume
   per parameter (precise, but visible in types), or one rule applies to every function
   value, for example "arguments are borrowed", with a wrapper that copies first when the
   real body consumes (Swift's reabstraction thunks). Either way the copies show up in
   `check --copies`.
2. **Linear and second-class captures.** A closure capturing a `Linear` value would itself be
   `Linear`, a call-once function, which means linearity in function types. Start by
   rejecting `Linear` captures in escaping closures. Tier 1 can keep allowing them, because
   lifting turns them into ordinary arguments. `NOGC.md` §3.3 already rejects capturing a
   second-class `with` handle in an escaping closure.
3. **Capture stays by value.** An escaping closure with by-reference capture is precisely the
   aliasing `MUTABILITY.md` exists to prevent.

The bulk of the work is spread rather than deep: every subsystem that matches on `Type` needs
a function case. That means heap leaves, copy/drop, union element layouts, `repr`/JSON/`read`
(likely "unsupported"), `FrogValue` and host marshalling, and the copy lint. Tier 2 is also
what tier 1's specialization would fall back to, so the code size it adds can be capped.

**What it buys beyond expressiveness:** host callbacks (an opaque function handle in
`FrogValue`), a `spawn` that takes a function, and nothing awkward for the emit-C backend,
where it is a function pointer and an env struct.

## Deferred, by design

- **Explicit capture lists** (`[n] -> ...`, as in Hylo and C++). Worth revisiting only if
  implicit capture proves confusing in practice; froglang's rule is simple enough
  (immutable, by value, at creation) that spelling it per lambda is ceremony.
- **Higher-order stdlib** — `map`, `filter`, `any`, `all`, `find`, `sort_by`. Now
  expressible: `FrogStateBuilder::prelude` takes frog source, and a generic higher-order
  declaration in it is exactly the shape this pass compiles. Deliberately left out of tier
  1 so the compiler change landed reviewable on its own.
- **Partial application / currying.** No syntax, and it would produce an escaping closure
  in every interesting case.
