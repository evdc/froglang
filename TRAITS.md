# Traits and Generics

Status: **design**. Nothing here is implemented. Syntax is a sketch and should be expected to
change; the *decisions* in "Foundational choices" are the part meant to be stable, since they
are the ones that are expensive to revisit later.

This supersedes the roadmap's "Traits/interfaces, explicit-style" and "User-definable generics"
bullets, and answers the duality that bullet flags between `provides` and impl blocks.

Today the language has four hardcoded traits (`typeck.rs`, `enum Trait`), a `provides` clause
that grants exactly one of them (`Error`) as a name-keyed marker, one generic type (`List(T)`,
a builtin special case in `Type::List(Box<Type>)`), and no way to write any of the three.

## Goals

- **Spend no novelty budget.** Generics and interfaces are the most thoroughly explored corner
  of language design; both experienced humans and LLMs arrive already trained on `<T>` and on
  `trait`/`interface`. Deviations must earn their place against that.
- **Remove special cases, don't add a subsystem.** The measure of success is that `Num`, `Eq`,
  `Ord`, and `Error` stop being a closed Rust enum and become ordinary declarations, and that
  `List` stops being a hack.
- **Orthogonal to what exists.** Traits must compose with unions, with `?`/`catch`, and with
  the `with`/`can` capability sketch in `CONCURRENCY.md` rather than sitting beside them.
- **Whole-program leverage.** Froglang compiles everything through one `FrogState`. Coherence,
  orphan rules, and abstract checking of generic bodies are problems created by separate
  compilation, and should not be paid for here.
- **Monomorphization stays viable.** Structs are unboxed flattened fields; generics must not
  force a uniform boxed representation.

## Foundational choices

| Question | Decision | Why |
| --- | --- | --- |
| Trait members | Plain functions with a `Self` placeholder; no receiver | Two-`Self` members (`Ord`) and zero-`Self` members (`zero(): Self`) fall out naturally; no second namespace |
| Method syntax | UFCS: `x.f(y)` desugars to `f(x, y)` when `x` has no field `f` | Chaining ergonomics without a method system existing |
| Type parameters | `<A, B>`, explicit binders, uppercase by convention | Precedent. Unambiguous here for a specific reason (below) |
| Explicit type args in expression position | Never | Removes the `<>` ambiguity at the source; return-type inference covers the cases |
| Impl location | `provides` clause with a body: inline on `data`, standalone for foreign types | One keyword, existing syntax stays valid, no orphan rules needed |
| Dispatch | Monomorphization | Unboxed flattened structs; already the assumption in `CONCURRENCY.md` |
| Runtime-chosen implementations | Closed unions + `match`, not vtables | Unions already *are* the existential mechanism, and they're faster |
| Builtin traits | Move to the prelude in the same round | Otherwise two trait systems coexist indefinitely |
| Variance | Invariant | Lists are mutable; covariance is unsound |

## Generics

### `Type::Named`

`Type::Struct(String)` and `Type::List(Box<Type>)` collapse into one constructor:

```rust
Type::Named { name: String, args: Vec<Type> }
```

`List` stops being a special case and becomes a prelude declaration with a builtin
representation; `Dict<K, V>` becomes writable; unification recurses into `args`. This is the
load-bearing refactor — it touches `unify`, `normalize`, `Display`, `resolve_union`,
`resolve_type_expr`, and codegen's layout logic. Nothing user-visible changes at this step,
which makes it a good first stage.

`TypeExpr::Apply(String, Vec<Spanned<TypeExpr>>)` already exists and is already parsed
generally — only its delimiter changes.

### Syntax

```
data Pair<A, B>(fst: A, snd: B)
data Outcome<T> is Ok(value: T) | Err(error: Error)
data Dict<K, V>(...)

trait Container<T> { func get(c: Self, i: Int): T }

func first<A, B>(p: Pair<A, B>): A = p.fst
func max<T>(x: T, y: T): T = if x > y then x else y

let d: Dict<Str, List<Int>> = ...
```

`List(T)` becomes `List<T>` throughout, matching TypeScript, Java, C#, Kotlin, and Swift. The
existing `[Int]` rendering in `Type`'s `Display` becomes either sugar for `List<Int>` or is
dropped in favour of it — see Open questions.

### Why `<>` is unambiguous here

`<>` is painful in C++, Java, and Rust because `f(a<b, c>(d))` parses both as two comparisons
and as a generic call. That ambiguity lives entirely in *expression* position, and this design
closes it by construction: **explicit type arguments never appear in expression position.**
Type arguments occur only inside the `TypeExpr` grammar and in declaration headers, neither of
which contains comparison operators.

So `zero<Int>()` is not a thing you can write. Where a call's type parameters aren't determined
by its arguments, the expected type determines them — the checker is already bidirectional:

```
let x: Money = zero()
let y = zero() : Money        // `expr : Type` is already grammatical
```

Patterns likewise take no type arguments: `is Ok(v)`, never `is Ok<Int>(v)`.

Two lexer facts make this cheaper here than elsewhere:

- There are **no `>>`/`<<` tokens** (`tokens.rs` has only `Lt`, `Gt`, `LtEq`, `GtEq`), so
  `List<List<Int>>` lexes as `> >` with no token-splitting. This is the most notorious `<>`
  implementation wart and we don't have it.
- `>=` is the one real hazard: `let x: List<Int>= 5` lexes `GtEq`. Fixed by splitting `GtEq`
  into `Gt`, `Eq` when the type-expression parser is expecting a closing angle — a few lines,
  in one place.

### Why explicit binders rather than inference from free variables

Inferring type parameters from free names in a signature (`func first(p: Pair(A, B)): A`) reads
well in the one-level case and has no principled answer one level down:

```
func outer<A, B>(a: A, b: B) = {
  func inner<B, C>(bb: B, c: C) = ...    // fresh B, shadows outer's — like any shadowed binding
  func other(bb: B, c: C) = ...          // error: C not bound. B is outer's, captured
}
```

With explicit binders this is ordinary scoping and needs no new rule. With implicit binders,
whether `inner`'s `B` is fresh or captured is a convention, and either choice silently
mis-types real programs.

A casing rule (lowercase = variable) was considered and rejected: it carries a load-bearing
distinction on a single character's case, is invisible at a glance, and inverts the convention
every reader arrives with.

**Lambdas remain implicitly generalized.** `x -> x + x` has no name and no signature to hang a
binder list on, and already prints as `(Num t) => t -> t`. This is the same line the language
already draws — `func` requires full parameter and return annotations, lambdas infer
everything — so "`func` declares, lambdas infer" extends to type parameters unchanged. The
visible cost: an inferred bound variable prints lowercase in the REPL while source uses
uppercase.

### Bounds

Bounds are **inferred from the body** wherever the body is visible, which is what `x -> x + x`
already does. They are written only at abstraction boundaries — trait signatures and function
types — mirroring the line `CONCURRENCY.md` draws for region annotations.

```
func max<T: Ord>(x: T, y: T): T = ...           // inline, the common case
func merge<K, V>(a: Dict<K, V>, b: Dict<K, V>): Dict<K, V> where K: Ord + Eq = ...
let cmp: (Ord T) => (T, T) -> Bool = ...        // function types keep the display form
```

Inline `<T: Ord>` for one or two bounds, `where` as the overflow valve. Function *types* have
no binder list, so they keep the `(Ord T) =>` prefix the checker already prints.

### Variance

**Invariant.** `List<Int>` is not a `List<Int | Str>`. Lists are mutable (`push`), so
covariance is unsound. Worth stating explicitly because union subtyping (`T ≤ T | U`) makes
people expect the opposite.

## Traits

### Members are functions, not methods

```
trait Shape {
  func area(s: Self): Float
  func name(s: Self): Str = "shape"        // default body
}

data Circle(r: Float) provides Shape {
  func area(c: Circle) = 3.14159 * c.r * c.r
}

let a = area(my_circle)
```

There is no receiver and no `self` keyword. `Self` is a type placeholder that stands for the
implementing type, and it may appear in any position — including none:

```
trait Ord  { func compare(a: Self, b: Self): Ordering }   // two Self parameters
trait Zero { func zero(): Self }                          // no Self parameter at all
```

Both are awkward under a receiver model and natural here. `zero()` needs no `T::zero()`
turbofish because the expected type resolves it — `let m: Money = zero()`.

The cost, and it is the main new machinery: **function names become overloaded.** `area` names
one function per implementing type, so `ctx: HashMap<String, Type>` can no longer be
one-name-one-type, and calls resolve on argument types. This is unavoidable in any design where
two types implement one trait.

### UFCS

> `x.f(a, b)` where `x` has no field named `f` is exactly `f(x, a, b)`.

One rewrite rule, applied to every function rather than to trait members specially, gives
method syntax and chaining without a method system existing:

```
items.filter(is_active).map(price).sum()
```

Fields win over functions on a name collision, so `p.x` stays unambiguous and
`infer_field_access` needs no change.

### `provides`, inline and standalone

One keyword, two positions. The inline form is today's syntax with a body added:

```
data Circle(r: Float) provides Shape { func area(c: Circle) = ... }   // your type

Int provides Shape { func area(n: Int) = n.to_float() }               // foreign type

error ParseError(msg: Str)                                            // unchanged: marker trait,
                                                                      // empty body omitted
```

`provides Error` as it exists today is the degenerate case — a trait with no members, so the
body is omissible. No current syntax breaks.

**No orphan rules, no coherence problem.** Froglang sees every declaration through one
`FrogState`, so the checker can simply reject a duplicate `(Trait, Type)` pair globally. This is
a real advantage over Rust's separate-compilation constraints and should be spent deliberately.

The REPL caveat from `CONCURRENCY.md` applies unchanged: an impl introduced at one prompt
changes overload resolution for a function defined at an earlier one.

### Traits and unions

A union satisfies a trait iff every member does — this is already the rule for `Error`
(`type_implements`'s `Type::Union` arm) and generalizes for free. Calling a trait function on a
union generates the dispatch `match`, exactly parallel to `u.f` being legal iff every member has
a compatible `.f`.

## The builtin traits move to the prelude

`Trait` becomes an open name (interned) rather than a four-variant enum, and the four become
ordinary declarations with operator sugar:

```
trait Num   { func add(a: Self, b: Self): Self  ... }    // a + b  ⇒  add(a, b)
trait Eq    { func eq(a: Self, b: Self): Bool }          // a == b ⇒  eq(a, b)
trait Ord   { func compare(a: Self, b: Self): Ordering } // a < b  ⇒  compare(a, b) == Less
trait Error { }                                          // marker; ?/!/catch key off it
```

`type_implements`'s hardcoded arms become lookups in the generalized `provides` table; the
structural cases (`Int: Num`, every struct is `Eq`) become prelude impls plus a derive.

This is what makes the feature a *removal* of special cases, and it is the argument for doing it
in the same round rather than growing a user trait system alongside the builtin four.

**The performance caveat is real and must be respected:** naively desugaring `+` into a call
would wreck the `fib` benchmark. Prelude impls on primitives stay intrinsics that codegen
recognizes and emits inline; the desugaring is a type-checker-level rewrite that codegen pattern
matches back out, in the same spirit as `?`/`catch` desugaring to `match` with no dedicated
codegen node.

## Dispatch and representation

Monomorphize. Trait calls resolve statically at each instantiation; no vtables. This is what
keeps `data Pair<A, B>(fst: A, snd: B)` instantiated at `Int` as two flat slots rather than two
boxes, and it is already the assumption `CONCURRENCY.md` makes for region checking
("per-instantiation checking keeps the whole-program argument valid").

**The existential mechanism already exists: closed unions.** "A value that is one of several
implementations" is `data Io is Blocking(...) | Fiber(...) | Test(...)` plus a `match` — faster,
closed-world, and already implemented. So `any Trait` / `dyn` is deliberately deferred until a
case proves unions insufficient; it slots in later without invalidating anything written
meanwhile.

The known candidate for that case is the swappable `Io` of `CONCURRENCY.md`, where a *host*
might want to register an implementation the frog program never named. If that requirement
firms up, boxed existentials become the answer; until then it is speculative.

## Capabilities are traits with implicit instances

`CONCURRENCY.md` writes `data Scope(A, T)(...) provides Spawn` and
`func fan_out(...) can Spawn`. Those are two different relations wearing one word:

| Relation | Meaning | Resolved |
| --- | --- | --- |
| `T provides C` | this type can serve as a C | statically, per type |
| `can C` in a signature | an instance of C must be ambient here | dynamically bound, statically tracked |

The unification: **a capability is a trait whose instance is passed implicitly rather than
explicitly.**

- `with e { ... }` binds `e` as the ambient instance of every trait `typeof(e)` provides, for
  the dynamic extent of the block.
- `can C` on a signature is an implicit parameter of type "some C", propagating to callers until
  a `with` discharges it.
- `without C` removes the binding.
- `spawn` and `blocking` need not be syntactic forms at all — `spawn` is a member of
  `trait Spawn`, resolved against the ambient instance. `Io` likewise: "a swappable value in the
  ambient context" is exactly "an implicit instance of `trait Io`".

This is Scala's implicits and OCaml's modular implicits, arrived at from froglang's own
`provides` rather than imported. It makes the concurrency design *less* special-cased rather
than more, and it means the trait work is a prerequisite that pays for itself twice.

It also isolates where dynamic dispatch would actually be needed: an ambient instance chosen at
runtime is the one case monomorphization can't erase.

## Deferred

Not foundational, and none of them constrain the above:

- **Associated types.** `trait Iter { type Item }` versus generic `trait Iter<Item>` — the
  choice matters for `for x in xs` over user types and should be made when iteration is
  designed, not now.
- **Blanket impls** (`impl<T: Display> Show for T`). Whole-program coherence makes them
  tractable later; they complicate resolution and buy little initially.
- **Higher-kinded parameters.** Not needed for anything on the roadmap.
- **`any Trait` / boxed existentials.** See above.
- **Derives.** `#derive(Eq)` waits on the annotation work already on the roadmap.
- **Trait objects in `data` fields, and variance thereof.**

## Implementation plan

Each stage is usable on its own and forward-compatible with the next.

### 1. `Type::Named`

Collapse `Type::Struct` and `Type::List` into `Type::Named { name, args }`; unification,
normalization, display, and codegen layout recurse into `args`. No user-visible change — this
is a pure refactor and should land and be tested on its own.

### 2. Generics

`<>` binders on `data` and `func`; `TypeExpr::Apply` switches delimiter; `GtEq` splitting in the
type parser; monomorphization at instantiation. `List` becomes a prelude declaration. `List(T)`
→ `List<T>` migration across README, tests, and error messages.

### 3. Traits

`trait` declarations with function members and default bodies; `Self` resolution; `provides`
blocks in both positions; global coherence check; overload resolution by argument type. This is
the largest stage and the overload resolution is its risk.

### 4. UFCS

The `x.f(y)` → `f(x, y)` rewrite, with fields taking precedence.

### 5. Prelude traits

`Trait` becomes open; `Num`/`Eq`/`Ord`/`Error` move to prelude declarations; operators desugar;
primitive impls stay codegen intrinsics. Benchmarks (`fib` especially) gate this stage.

### 6. Derives

Depends on the annotation work on the roadmap. `Eq` for structs is the first customer, since it
is currently a hardcoded arm of `type_implements`.

### 7. Capabilities

`provides`/`can`/`without` as implicit instances. Merges with `CONCURRENCY.md` stage 7.

## Open questions

- **Does `[Int]` survive as sugar for `List<Int>`?** `Type`'s `Display` prints `[Int]` today
  while `TypeExpr`'s prints `List(Int)` — an existing inconsistency that this work has to
  resolve one way or the other. Keeping `[Int]` as sugar is cheap and reads well; dropping it
  means one spelling.
- **Overload resolution rules.** Resolution by argument type is easy for the first-order case
  and needs a stated rule for ambiguity, for arguments that are themselves unresolved type
  variables, and for interaction with union arguments. This is the part most likely to grow
  accidental complexity.
- **How much bound inference?** Inferring bounds from a `func` body is what lambdas already do,
  but it means a function's public signature is not fully written in its header. An alternative
  is to infer and then *require* the inferred bounds to be written, rustc-suggestion style.
- **Do prelude operator traits admit user impls on user types?** `data Vec2(x: Float, y: Float)
  provides Num` giving `+` is the obvious want, and also the obvious route to operator abuse.
- **Monomorphization and the REPL.** A generic function defined at one prompt gets new
  instantiations at later ones; `state.rs` compiles incrementally, so instantiation caching has
  to survive across entries.
- **Error messages for instantiation failures.** `CONCURRENCY.md` already flags the C++-template
  failure mode; per-instantiation trait errors have the same shape and need "instantiated from
  here" chains.

## Prior art

- **Rust** — `trait`/`impl` separation, inline and `where` bounds, monomorphization, derives
  (all adopted in substance); orphan rules and coherence (unnecessary here); receiver methods and
  turbofish (both declined).
- **Haskell** — type classes as functions rather than methods, dispatch including on return type
  (`zero(): Self`), superclass-free simplicity (adopted); higher-kinded classes (deferred).
- **TypeScript / Java / C# / Kotlin** — `<T>` syntax and uppercase convention, the precedent this
  design defers to.
- **Go** — implicit structural interface satisfaction (declined: `Error` is deliberately granted
  rather than structural, and `ERRORS.md` argues why).
- **Scala / OCaml modular implicits** — implicit instance resolution, the model for capabilities.
- **Nim / D** — UFCS as a rewrite rule rather than a method system.
- **Zig** — comptime-as-generics (declined; a separate evaluation model is a larger novelty spend
  than `<>`), and per-instantiation checking (adopted for regions in `CONCURRENCY.md`, and the
  same trade applies here).
