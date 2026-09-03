# Errors, Sum Types, Option

Status (updated 2026-09-01): **mostly shipped**. `data X is A | B(...)` sugar, `match`/`is`/
destructuring, `error X(...)`, `provides Error`, `?` propagation, `catch` (value and handler
form), `!`, the tagged-pointer union representation, the `Truthy` trait, and flow narrowing after
a bindless `is`/`match` arm are all implemented and tested — see `README.md`'s "Union types" and
"Error handling" sections, and `test_truthy_and_narrowing.rs`/`test_unions.rs`/`test_result_marshalling.rs`.
Still open: error return traces. (The `Truthy`-on-union crash formerly noted here is fixed —
`coerce_truthy` now desugars a union condition into a per-member tag dispatch, see `roadmap.md`.)
This file's syntax sketches below are otherwise a reasonably accurate description of what
shipped, not a stale proposal — treat divergences from current README syntax as the doc being
behind, not the code.

This supersedes the "Error handling, Pattern Matching" section of `DESIGN.md`, and revises the
enum parts of "Data Structures" — the two turned out to be one design.

## Goals

- Errors are values. No exceptions for domain failures. `?` for propagation, no `if err != nil`.
- **Widening an error type into a broader one is free** — no `From` impls, no `map_err`, no
  derive. This is the single largest ergonomic win available and most other choices here are
  made in service of it.
- One mechanism for "this is one of these", not two. `Result` should not be a bolted-on special
  case of a general facility, nor a general facility that only works for errors.
- Enum variants are proper types. `Shape.Circle` is a type you can name, pass, and reuse.
- Convenience where it is safe (empty collections are falsey) and precision where it is not
  (an error is never silently a boolean).
- A failure should explain itself without anyone having written the explanation.

## Foundational choices

| Question | Decision | Why |
| --- | --- | --- |
| `Result(T, E)` | Not a type. `T \| E` where `E: Error` | Makes error widening a no-op instead of a conversion |
| Sum types | **One mechanism**: the union. `data X is A \| B` is sugar | Two sum mechanisms is duplication users must arbitrate |
| Variant identity | Nominal member types, canonical `Shape.Circle` | Variant types are first-class; nominality prevents collapse |
| Union representation | One slot when all members are self-describing; two (`{tag, payload}`) otherwise | No allocation on the happy path; predictable rule |
| `Error` | A marker **trait**, not a union | Must be open; a predicate is cheaper than a growing union |
| Error hierarchies | Closed union aliases only | Open subtyping duplicates what traits already do |
| `None` | A built-in singleton member type, **not** an `Error` | "No row" and "the query failed" are different facts |
| Propagation | Postfix `?`, removes `Error` members | Chains; highest-frequency operation gets the shortest spelling |
| Coalescing | `catch` (errors) and `or` (falsiness) as *separate* operators | An empty list is a successful value; an error is not |
| Truthiness | A `Truthy` trait, coerced only in condition position | Python's convenience, confined to where a Bool was already demanded |
| Panics | Unwinding, per `CONCURRENCY.md` | Domain failures are values; bugs and cancellation unwind |

## The unification

There is one sum mechanism. `data Shape(color: Color) is Circle(r: Int) | Rectangle(w: Int, h: Int)`
is **sugar** for:

```
data Shape.Circle(color: Color, r: Int)                 // ordinary nominal structs
data Shape.Rectangle(color: Color, w: Int, h: Int)      // common fields prepended
type Shape = Shape.Circle | Shape.Rectangle             // a closed union alias
```

This works here specifically because froglang structs are **already nominal** — `Type::Struct(String)`,
compared by name only (`typeck.rs:78`). Nominality is what makes a union safe to use as the only
sum: the collapse hazard (`Float | Float`) exists only for structurally identical types, and
`data Temp is Celsius(Float) | Fahrenheit(Float)` produces two *distinct* nominal types.

`Shape.Circle` is the canonical name; bare `Circle` resolves when unambiguous. This is not new
machinery — `typeck.rs:690,721` already resolves bare variant references and reports ambiguity;
that code is reused rather than deleted. Without it the unification would be a namespacing
regression (two enums could not both have a `Pending`), so it is load-bearing.

### The two axes that remain

With one mechanism, the surviving distinction is between anonymous and declared unions, which
is a much smaller thing to explain than union-versus-enum:

| | anonymous (`Int \| Str`) | declared (`data Shape is ...`) |
| --- | --- | --- |
| members | pre-existing types | brought into being by the declaration |
| naming | none | canonical `Shape.Circle` |
| closure | open — anything can widen in | closed — fixed member list |
| exhaustive match | no, catch-all required | yes, checked |

Same type, same runtime shape, same `is` test. The declaration form buys names, common fields,
and exhaustiveness — nothing semantic.

### You never construct a union

Union values arise **only from widening**, which is subtyping and therefore implicit at every
site where a type is expected:

```
let x: Int | Str = 5                       // annotation
func f(): Shape = Circle(r=1)              // return position
draw(Circle(r=1))                          // argument position
let xs = [1, "a"]                          // List(Int | Str) — literal element join
let m = {"a"=1, "b"="x"}                   // heterogeneous map values (DESIGN.md)
if c then 1 else "a"                       // branch join — the only site that works today
```

Which gives the symmetry the whole design rests on:

| | construct | destruct |
| --- | --- | --- |
| union member | implicit widening, free | `is` test / `match` |
| struct | explicit `Circle(r=1)` | field access |

`Circle(r=1)` is a *struct* construction that happens to produce a value usable as a `Shape`.
There is no `Shape::Circle(...)` step, and that is exactly why `?`-widening `ParseError` to
`CompileError` costs nothing while Rust needs `From`. **Free error conversion is not a special
case for errors — it is the general behaviour of the one sum mechanism.**

### Field access on a union

`u.f` is legal iff *every* member of the union has an `.f` of compatible type. This is what
makes common fields work after desugaring, and it generalizes past enums, so it is a rule about
unions rather than a rule about `data ... is ...`.

### Why Rust doesn't do this

Worth recording, because the answer is a cost we are choosing to pay rather than an oversight.

A Rust `enum` is tag-plus-max-payload, inline, `Sized`, on the stack. If `E::A` were a real
type, `E` would be its supertype, and widening `A → E` would change the value's size and layout.
Rust has essentially no subtyping precisely because layout-changing coercions fight
monomorphization and niche optimization. **We can afford variant types because we have a GC and
a uniform representation — we are buying them with the runtime cost Rust exists to refuse.**

Secondarily: set-theoretic unions need subsumption, which does not fit unification-based
inference. Froglang already committed to *local* inference with annotations at function
boundaries, which is the regime where subtyping is tractable — so this follows from a decision
already made rather than being a new bet.

The languages that landed here — Scala 3 (`enum` desugars to sealed class plus case classes),
Kotlin (sealed interfaces), TypeScript (discriminated unions) — are the ones designed on a
runtime that was already uniformly boxed with subtyping present. Rust is the outlier because it
is the one trying to be zero-cost, and it does not consider the matter settled: an RFC for enum
variant types has been open since 2018, stuck largely on the layout/subtyping interaction.

The honest cost: Rust gets `match` as a jump table on a register-resident tag with the payload
inline. We get a load before the dispatch for boxed members.

## Representation

The discrimination question a union has to answer is never "which of N types is this" in the
general case — it is **"is this member self-describing?"**, and the answer is usually yes.

- **Boxed members** (`Str`, `List`, structs, multi-field variants) are pointers, and the GC
  header carries a **global type id**. `GcHeader` today is `{ next: *mut, marked: bool, kind:
  ObjKind }` (`gc.rs:20`); a `u32 type_id` fits in the existing padding at no size cost. Whole-
  program compilation through one `FrogState` makes id assignment trivial.
- **Nullary members** (`None`, `Red`) are already unboxed immediates, `(tag << 1) | 1`, low bit
  distinguishing them from 8-aligned pointers (`gc.rs:82`). They carry their own id.
- **Unboxed primitives** (`Int`, `Float`, `Bool`) are raw `i64`s and are *not* self-describing.

So:

> **A union occupies two slots — `{tag, payload}` — if and only if it contains `Int`, `Float`,
> or `Bool`. Otherwise it occupies one.**

| type | slots | note |
| --- | --- | --- |
| `Shape` | 1 | pointer, id in header — same cost as today's enums |
| `User?` | 1 | pointer or the `None` immediate; the null-pointer optimization, for free |
| `Str \| ParseError` | 1 | both boxed |
| `Int \| DbError` | 2 | `Int` is not self-describing |
| `Int?` | 2 | one extra register, no allocation |

**Widening boxes a member iff it does not fit in one slot.** A flattened multi-field struct is
boxed at the widening site; `Int`/`Float`/`Bool` are not boxed — they ride in the payload slot,
because a second register beats an allocation on the happy path. The cost appears at the
widening site, where it is visible.

Multi-slot returns are already in codegen's vocabulary (structs are flattened into slots) and
Cranelift returns multiple values natively, so the two-slot form needs no new mechanism.

`FrogVariant` (`gc.rs:74`) largely survives: `nslots` and `ptr_mask` are what the GC actually
needs, and its comment already notes the GC never needs to know which enum a value came from.
What changes is that `tag` becomes a global type id rather than an index local to one enum.

**Known cost**: `List(Int | DbError)` has stride 2. Lists of unions containing primitives double
in size. Acceptable, and `FrogList` already supports arbitrary stride with a `ptr_mask`
(`gc.rs:52`), so no new list work is required.

## Error-ness

`Error` is a **marker trait**, not a union and not a magic type.

```
error ParseError(line: Int, col: Int, reason: Str)
// ≡ data ParseError(line: Int, col: Int, reason: Str) provides Error
```

`provides` is the spelling because `CONCURRENCY.md` already spends it on exactly this relation
(`data File(fd: Int) provides Read, Write`). `is` is unavailable — it means "has these union
members". The `error` shorthand exists because error declarations are frequent and almost always
this exact shape, on the same reasoning that gives `data ... is ...` sugar over a hand-written
union.

Error hierarchies are **closed union aliases**:

```
type CompileError = ParseError | TypeError | CodegenError
```

Exhaustively matchable, free widening from any member.

### Why a trait and not an open union

An open union subsumes the *discrimination* half of traits and none of the *operations* half.
The axis that matters is not trait-versus-union:

| | open (extensible) | closed (fixed) |
| --- | --- | --- |
| carries operations | **trait** — dispatch, retroactive impls, no exhaustiveness | — |
| carries none | (a marker trait) | **union** — exhaustive match, no dispatch |

The empty upper-right cell is the point. An open union *is* a marker trait, so having both is
duplication, and traits are the side that generalizes. Concretely:

1. `Error` must be open — third parties define errors. `type Error = A | B | C` would need
   editing at one declaration site every time anyone adds one.
2. `?`'s type rule asks a **predicate** of each member ("is this an `Error`?"), not a subtype
   relation against an ever-growing union. A predicate is cheaper and never materializes the set.
3. `typeck.rs:39` already implements `type_implements` with the rule we need — *a union
   satisfies a trait iff every member does* (`typeck.rs:42`). `E: Error` on
   `ParseError | TypeError` works for free. `Trait` (`typeck.rs:20`) gains one variant.
4. When `Error` grows operations — `.message()`, `.code()`, a source location — a trait has
   somewhere to put them.

**Therefore `type ParseError : CompileError` (open subtyping) is dropped.** It buys retroactive
extension at the price of never being exhaustively matchable, which is what traits already do,
better. This is the same move as the enum/union unification: delete the mechanism that is a
special case of another, rather than publish a guideline for choosing between them.

`is Error` as a match arm is a **trait test on a type id** — whole-program compilation makes it
a static table (type id → trait bitset), so it is a load and a bit test. Exhaustiveness stays
checkable: "every remaining member implements `Error`" is decidable over a closed member list.

## Syntax

### Signatures

`|` is the only type-level sum operator. `?` never appears in type position except in the one
abbreviation `T?` = `T | None`.

```
func find(id: Int): User?                          // User | None
func parse(s: Str): AST | ParseError
func compile(s: Str): Code | CompileError
func read(p: Str): Str | IoError | ParseError
```

An earlier draft used `T ?E`. It is dropped: under the unification `T ?E` and `T | E` would be
two spellings of one type, which is the duplication this document exists to remove. Spotting
fallibility is better bought at the call site — see "must handle", below — which is where it
matters and which needs no signature syntax.

### Operators

| spelling | meaning |
| --- | --- |
| `f()?` | propagate `Error` members to the caller |
| `f()!` | unwrap, or panic |
| `x catch y` | error-coalesce: supply a value in place of a failure |
| `x or y` | truthiness-coalesce: supply a value in place of an *empty* one |
| `x.is_ok()` / `x.is_err()` | error union → `Bool`, explicitly |

`catch` replaces the `else` of the `DESIGN.md` draft. `else` was overloaded past usefulness —
`if c then a else b else d` settles it. `catch` is Zig's spelling; the exception baggage does
not bite in practice. Note that Zig needs *both* `orelse` (optionals) and `catch` (error unions)
because those are different types in Zig; the unification means one keyword suffices, which is
some evidence it is paying.

```
let port = config.get("port") catch 8080
let user = find(id) catch return NotFound
let rows = query(sql) catch [e] -> { log(e); [] }
```

`catch` takes either a value or a one-argument lambda, reusing the existing `[x] -> body` lambda
syntax rather than inventing a binder. `catch return X` and `catch panic(...)` require `return`,
`break`, and `panic` to have a **bottom type** — which the unwinding work in `CONCURRENCY.md`
needs regardless, so `Never` should land with that rather than being invented here.

`try` as a prefix operator (floated in `DESIGN.md`) is dropped: postfix `?` chains
(`parse(s)?.validate()?`) and prefix does not. `try { ... }` as a block where every call
propagates is also dropped — hidden control flow is what `?` exists to make visible.

### Truthiness

Empty-is-falsey is convenient and is kept, but it is separated from the error path completely.
The distinction that makes it safe: **`[]` is a successful value that happens to be empty; a
`DbError` is not a value at all.**

```
let xs = query(sql) catch []       // DbError → [];  a successful [] stays []
let xs = query(sql) or [1, 2]      // an empty result → [1,2]
```

Two operators because those are two different questions. `catch` and `?` look only at the union
tag; `or`, `and`, `not`, and condition position look at the value.

Mechanism: a **`Truthy` trait**, with implicit coercion **only in condition position** — `if x`,
`while x`, `x or y`, `not x`, and nowhere else. That is Python's model restricted to exactly the
positions where a `Bool` is already syntactically demanded; it needs no new token and `if xs { }`
works with no ceremony.

**Collections, strings, and numbers are `Truthy`. Error unions are not.** `if user_result` does
not compile. Not because it is ambiguous, but because that is the one case where silent
conflation — "failed" versus "found nothing" versus "found zero" — is a correctness bug rather
than a convenience. `.is_ok()` / `.is_err()` are the explicit escape hatch: greppable, and
obviously written on purpose.

**None** is falsey; this is ok per the previous point, as None is not an Error.

An earlier draft gave postfix `?` to truthiness. Dropped: it would make one token mean "coerce to
Bool" in condition position and "early-return" in expression position, and propagation is far
more frequent, chains, and carries Rust/Swift precedent worth a great deal for a language aimed
partly at generated code.

## `None`, and what `?` propagates

`None` is a built-in singleton member type, immediate-encoded, and **not** an `Error`.

Given `func get_user(id: UserId): User | None | DbError`:

```
let u = get_user(id)?      // propagates DbError only;  u : User | None
```

If `None` were an `Error`, `?` would yield `User` but force every caller to list `| None` in its
own return type — so a handler returning `Response | HttpError` would end up claiming it can
return "nothing", which is a different and false fact. A third option, where `?` propagates
whichever members the enclosing signature happens to list, is rejected: adding `| None` to your
own return type would silently change what `?` does in your body.

The two operators compose, so no third spelling is needed:

```
func handler(id: UserId): Response | HttpError = {
    let u = get_user(id)?              // DbError propagates; u : User | None
    let u = u or return NotFound       // None handled explicitly, right here
    render(u)
}

let u = get_user(id)? or return NotFound     // or in one line
```

`?`'s type rule is a set operation: remove the `Error`-bounded members from the union, and
require the enclosing function's return type to include the removed ones.

To avoid ambiguity: `catch` coalesces Errors, `or` coalesces None

## Matching, and narrowing

```
match let u = get_user(id) {
    is User    then do_thing(u)
    is None    then log("user not found")
    is DbError then log("DB error: ${u}")
}
```

Two things make this work with no new binder syntax:

- **Flow narrowing of the subject binding.** Inside `is DbError`, `u` *has type* `DbError`. The
  tag test is already there, so this is free, and it means `is Type` arms never need a binder.
  `is Shape.Circle(r)` destructuring remains available when the fields are what you want.
- **`is Error` matches a trait bound**, as a catch-all over every error member — which is what
  makes the common two-arm case terse.

Lowering is unchanged in shape: `lower_match` (`typeck.rs:1684`) already desugars to
`Conditional` + `IsVariant` + `VariantField` with no new control flow in codegen. `IsVariant`
generalizes from "enum variant index" to "type id or trait bitset test".

## Static rules

1. **A union containing an `Error` member cannot be silently discarded.** It must be `?`'d,
   `catch`'d, `!`'d, or matched. This is the real safety property, and it is why signature-level
   fallibility syntax is unnecessary.
2. **`catch` requires an unambiguous failure side** (above).
3. **Widening is implicit and unrestricted**; narrowing always requires a test.
4. **Exhaustiveness is checked for closed unions** (declared aliases and `data ... is ...`), and
   a catch-all is required for anonymous open ones.
5. **`?` is an ordinary early return**, so `with`-exits and `defer` blocks run on the way out.
   `CONCURRENCY.md` stage 1 must count `?`-return among "all paths out".

## The escape hatch

Flattening means `T | E | E` normalizes and `Result(Result(T, E), E)` cannot be spelled — mostly
a feature, but it kills the legitimate case of an error *as data*: a function that classifies,
maps, or parses errors. The explicit wrapper:

```
func classify(s: Str): Ok(ParseError)      // the success value happens to be an error value
```

`Ok(T)` is a one-field struct, deliberately verbose, deliberately rare.

## What this deletes

Being explicit, since the point of doing this now is that there are no users or libraries yet:

- **`Type::Enum`** (`typeck.rs:84`) folds into `Type::Union` plus closure/order metadata in
  `enum_defs`. Nominal enum types become closed union aliases.
- **`FrogVariant::tag` as a per-enum index** becomes a global type id; `GcHeader` gains one.
- **`Result` as a type** — never introduced. No `From`, no `map_err`, no `#[from]`, no error
  conversion machinery of any kind.
- **Open subtyping (`type X : Y`)** — dropped in favour of traits.
- **`T ?E` return syntax** — dropped in favour of `T | E`.
- **`else` as a coalescing operator** — dropped in favour of `catch`.
- **"Error types are falsey"** (`DESIGN.md`) — dropped. Errors are never implicitly boolean;
  emptiness is, and only in condition position.

## Implementation plan

Stages 1 and 2 are the rework and are worth doing before more code depends on the current shape;
the roadmap's queued "unbox variants *with* payloads" item touches the same code, so this is a
scheduling question, not extra work.

### 1. Type-system unification

- `Type::Enum` folded into `Type::Union`; `enum_defs` keeps closure, member order, and common
  fields. `normalize()` (`typeck.rs:98`) already flattens, dedups, and canonicalizes.
- `data X is A | B` desugars to member structs plus a closed alias. Canonical `X.A` names, bare
  resolution reusing `typeck.rs:690`.
- Member types become first-class: nameable, passable, reusable in other unions.
- `is_subtype` (`typeck.rs:949`) already has both union directions; widening is subsumption.

### 2. Union representation in codegen

Currently there is none — grepping `codegen/` for `Union` returns nothing, so unions survive
today only because match-arm joins get narrowed away before lowering.

- Global type ids; `u32 type_id` into `GcHeader`'s existing padding.
- One-slot and two-slot union forms, per the rule above; boxing at widening sites.
- `IsVariant` generalized to a type-id / trait-bitset test.
- `List` stride for two-slot unions (`FrogList::ptr_mask` already supports it).

### 3. `Error` and the must-handle rule

- `Trait::Error`; `provides` on `data`; the `error X(...)` shorthand.
- Closed union aliases (`type CompileError = ...`).
- Discarding an unhandled error union becomes a type error.

### 4. Operators

- `?` (member subtraction plus early return), `!` (unwrap or panic), `catch` with the ambiguity
  rule, both value and lambda operand forms.
- `Never` for `catch return` / `catch panic` — ideally shared with `CONCURRENCY.md` stage 1.

### 5. Truthiness and narrowing

- `Trait::Truthy`; coercion in condition position only; `or` / `and` / `not`.
- `None` is falsey.
- Flow narrowing of match subject bindings; `is Error` trait arms; exhaustiveness over closed
  unions including trait-bound arms.

### 6. Error return traces

Every `?` is a compiler-known site and errors are already boxed, so appending `(fn, line)` to a
per-task trace buffer at each `?` yields Zig's error return traces: not "where was this thrown"
but "the exact chain of propagation that carried it here". Debug-only, near-zero cost on the
happy path.

For a language whose stated goal is trustworthiness of generated code, this is a larger lever
than the pre/post-condition work — the failure report becomes self-explanatory without anyone
writing context strings. It is listed last because it is additive, but the header space it needs
should be reserved in stage 2.

## Open questions

- **Inferred error sets.** Free widening means a function's error set is exactly the union of
  what it propagates, so it could be inferred — but that collides with "annotations required at
  function boundaries". Plausible resolution: scope *boundary* to the **module** boundary, so
  exported functions declare and module-internal ones infer. Zig's `!T` is the same bet. Needs
  a decision on spelling (`Str | Error` reads as "some error" but erases which).
- **`?` inside a comprehension.** `[for x in xs { parse(x)? }]` propagating out of the loop is
  defensible (it is an early return), but it arguably wants collect-semantics —
  `List(T) | ParseError` rather than aborting. No confident answer yet.
- **`?` inside a lambda** propagates to the lambda's return, not the enclosing function. Rust's
  most common `?` confusion. A diagnostics problem, but worth pre-committing to a good message.
- **Are `provides`/`can` and traits one mechanism?** They look like the same declaration-site
  relation differing only in how they are *required* — `can Spawn` (ambient, discharged by a
  `with`) versus `E: Error` (a bound on a value's type). If so they should be built as one, and
  that should be checked before either lands.
- **Structural `==` on unions.** `desugar_enum_eq` currently handles enums; it needs to become a
  type-id comparison plus a per-member field comparison, and the roadmap already lists
  structural `==` on enums as a follow-up.
- **Do heterogeneous anonymous unions (`Int | Str`, heterogeneous maps) earn their keep?** They
  fall out for free and `DESIGN.md` promises them for maps, but they are also the feature most
  likely to encourage stringly-typed generated code. Allowed, or merely representable?

## Prior art

- **Zig** — error sets are exactly these error unions: flat, implicitly widening to supersets,
  with inferred sets via `!T` (adopted); `catch` (adopted); error return traces (adopted);
  needing both `orelse` and `catch` because optional and error union are distinct types (avoided
  by the unification); no variant types (declined).
- **Rust** — `?` and errors-as-values alongside panic-unwind (the model); `From`/`map_err`
  conversion tax and variants-are-not-types (both the thing being fixed); unboxed enum layout as
  the reason it cannot be (the cost we are choosing to pay).
- **Scala 3 / Kotlin** — `enum`/sealed hierarchies desugaring to a union of case classes whose
  members are real types. This is the model, arrived at independently from the same premise: a
  uniform representation with subtyping already present.
- **TypeScript** — discriminated unions and flow narrowing of a matched binding (both adopted);
  structural typing making union collapse a real hazard (avoided by nominality); conflating the
  open and closed mechanisms (declined).
- **Python** — empty-is-falsey (adopted, restricted to condition position); truthiness applying
  to everything including failures (declined).
- **Swift** — `try` as a prefix marker and force-unwrap `!` (the second adopted, the first
  declined in favour of postfix `?`).
- **Go** — `if err != nil` (the anti-model, and the reason `?` is not optional).
