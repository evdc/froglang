# Traits, Generics, and Methods

Status: **design**. Nothing here is implemented. Syntax is a sketch and should be expected to
change; the *decisions* in "Foundational choices" are the part meant to be stable, since they
are the ones that are expensive to revisit later.

This supersedes the roadmap's "Traits/interfaces, explicit-style" and "User-definable generics"
bullets, and answers both the duality that bullet flags between `provides` and impl blocks and
the method-syntax sketches at the bottom of `roadmap.md`.

## What exists today

Verified against the tree, not remembered — several earlier drafts of this document were wrong
about these, and the errors pointed at the wrong stage being the risky one.

- **Five hardcoded traits**, not four: `Num`, `Eq`, `Ord`, `Error`, `Truthy` (`typeck.rs`,
  `enum Trait`). `Truthy` is a coercion relation consulted by `check_condition`, not a callable
  interface — see "The `Truthy` exception".
- **`provides` is a name-keyed marker list** with no body. Any name other than the five is
  rejected with "Unknown trait '...' in provides clause".
- **`Eq` and `Ord` are already structural for structs; `Error` is already granted.**
  `type_implements` has a literal `Type::Struct(_) if *tr == Trait::Eq => true` arm, while
  `Error` is looked up in the `provides` table. The hybrid this document formalizes is already
  what the language does.
- **No polymorphism of any kind.** There is no `generalize` and no `instantiate`; there is one
  global `substitutions: HashMap<String, Type>` and type variables are unification holes, not
  bound variables. Lambdas are *not* generalized:

  ```
  $ frog check 'x -> x + x'
  :: [~t2:Num] -> ~t2:Num

  $ frog check '{ let f = x -> x + x; let a = f(1); let b = f(1.5); a }'
  Type error: Can't unify Float and Int
  ```

  There is no `(Num t) => t -> t` display form anywhere in `Type`'s `Display`. **This is the
  single most important fact for staging**: "generics" is not "add binders and monomorphize",
  it is "introduce type schemes into a checker that has never had them".
- **`x.f(y)` already parses.** `p.foo(2)` reaches the checker as `Call(FieldAccess(p, foo), [2])`
  and fails only in `lower_field_access` with "Struct P has no field 'foo'". UFCS is a change to
  one function.
- **`Type::List(Box<Type>)` / `Type::Struct(String)`**: 70 match sites total — 42 in `typeck.rs`,
  25 in `codegen/mod.rs`, 2 in `state.rs`, 1 in `typed_ast.rs`. Mechanical, self-contained.
- **There is no standard library.** The builtins are `print`, `push`, `panic`, `gc_dump`. There
  is no user-callable `len`. Every generics stage exists to make a stdlib writable, and the
  stdlib is what proves the generics stage worked.
- **Codegen keys functions by string.** `func_ids: HashMap<String, FuncId>`, looked up from
  `TypedExprKind::Var(name)` in `compile_call`. See "Where the seam is".
- **Modules are a flat pre-typeck mangling pass** (`modules.rs`): every reachable file is parsed,
  its top-level names prefixed, references rewritten, and the result handed to an unmodified
  checker as one statement list. The whole-program assumption this design leans on holds.
- **The `mut` call-site marker is semantically inert.** `func_mut_params` (`typeck.rs:441-452`)
  is consulted by `lower_call` to *validate* the marker against the declaration and to compute
  the copy-out count — and the copy-out count comes from the declaration, not the marker. Nothing
  downstream reads it. It is a reader-facing assertion the compiler verifies, which is what makes
  the receiver exemption below affordable.

## Goals

- **Spend no novelty budget.** Generics and interfaces are the most thoroughly explored corner of
  language design; both experienced humans and LLMs arrive already trained on `<T>` and on
  `trait`/`interface`. Deviations must earn their place against that.
- **Remove special cases, don't add a subsystem.** The measure of success is that `Num`, `Eq`,
  `Ord`, and `Error` stop being a closed Rust enum and become ordinary declarations, and that
  `List` stops being a hack.
- **Method syntax is a first-class goal, not a consequence.** `xs.push(x)` and
  `items.filter(p).map(g)` are the ergonomics the roadmap asks for by name. `List.push(xs, x)` is
  explicitly rejected.
- **No global function overloading.** This is a constraint, not an outcome — see below.
- **Orthogonal to what exists.** Traits must compose with unions, with `?`/`catch`, and with the
  `with`/`can` capability sketch in `CONCURRENCY.md` rather than sitting beside them.
- **Whole-program leverage.** Froglang compiles everything through one `FrogState`. Coherence,
  orphan rules, and abstract checking of generic bodies are problems created by separate
  compilation, and should not be paid for here.
- **Monomorphization stays viable.** Structs are unboxed flattened fields; generics must not force
  a uniform boxed representation.

## Foundational choices

| Question | Decision | Why |
| --- | --- | --- |
| Trait members | Plain functions with a `Self` placeholder; no receiver | Two-`Self` members (`Ord`) and zero-`Self` members (`zero(): Self`) fall out naturally |
| Where members live | The impl's namespace, **not** the global one | Kills overload resolution entirely — the largest risk in the design |
| Method syntax | UFCS: `x.f(y)`, resolved in a fixed three-step order | Chaining ergonomics; step 3 keeps free functions chainable too |
| Prefix form for members | Trait-qualified: `Ord.compare(a, b)` | Makes zero-`Self` members spellable; `Circle.area(c)` would reintroduce `List.push(xs, x)` |
| `mut` on a method receiver | Not marked: `xs.push(x)` | The marker exists to make a hidden mutated operand visible; the receiver is the least hidden position in the language |
| Type parameters | `<A, B>`, explicit binders, uppercase by convention | Precedent. Unambiguous here for a specific reason (below) |
| Explicit type args in expression position | Never | Removes the `<>` ambiguity at the source; return-type inference covers the cases |
| Impl location | `provides` clause with a body: inline on `data`, standalone for foreign types | One keyword, existing syntax stays valid, no orphan rules needed |
| Structural traits | `Eq`/`Ord`/`Show` derived for every `data` by default; explicit `provides` overrides | Already how `Eq` works; `DESIGN.md` asks for it in as many words |
| Dispatch | Monomorphization | Unboxed flattened structs; already the assumption in `CONCURRENCY.md` |
| Runtime-chosen implementations | Closed unions + `match`, not vtables | Unions already *are* the existential mechanism, and they're faster |
| Builtin traits | Move to the prelude, `Truthy` excepted | Otherwise two trait systems coexist indefinitely |
| Variance | Invariant | Lists are mutable; covariance is unsound |

---

# Part 1 — Resolution

The core of this design, and the part that differs most from earlier drafts.

## The problem with plain-function members

Members-are-functions is right for the reasons below. But "members are functions" does not have to
mean "members live in the global namespace", and earlier drafts assumed it did. That assumption
forces the chain:

> two types implement `area` → the global name `area` is multi-valued → `ctx: HashMap<String, Type>`
> becomes multi-valued → calls resolve by argument type → **whole-program overload resolution**

which needs stated rules for ambiguity, for arguments whose types are still unresolved variables
(given that there is no generalization today, that is *most* arguments mid-inference), and for
union arguments. It is irreversible once written and it is where accidental complexity would
accumulate.

## The rule: namespaced declaration, dot dispatch

Members are still plain functions with explicit `Self`-typed parameters. They live in the impl's
namespace. Nothing is overloaded, because nothing collides.

```frog
trait Shape {
  func area(s: Self): Float
  func name(s: Self): Str = "shape"        // default body
}

data Circle(r: Float) provides Shape {
  func area(c: Circle) = 3.14159 * c.r * c.r
}

my_circle.area()            // dot form — resolved by the receiver's type
Shape.area(my_circle)       // prefix form — trait-qualified
area(my_circle)             // error: no global `area`
```

**Resolution order for `x.f(args)`** — fixed, total, and stated once:

1. A **field** named `f` on `typeof(x)` → field access. Today's behaviour, unchanged; fields
   always win, so `p.x` stays unambiguous and existing programs cannot change meaning.
2. A **member** `f` in an impl for `typeof(x)` → that member. A hash lookup keyed on
   `(typeof(x), f)`, not a search. Ambiguity is impossible by construction: a duplicate
   `(Trait, Type)` pair is already rejected globally by the coherence check, and two *different*
   traits both implementing `f` for the same type is the one collision case — rejected at impl
   registration, not at the call site, so the error names the two impls rather than the call.
3. A **global `func f`** whose first parameter accepts `typeof(x)` → rewrite to `f(x, args)`.
   Global names stay unique, so this is also a lookup, not a search.

Step 3 is what keeps ordinary free functions chainable without them being trait members:

```frog
func filter<T>(xs: List<T>, p: (T) -> Bool): List<T> = ...
func map<A, B>(xs: List<A>, f: (A) -> B): List<B> = ...

items.filter(is_active).map(price).sum()
```

Steps 1 and 3 alone — no traits, no impls — deliver the whole aesthetic the roadmap asks for, and
cost one function in `typeck.rs`. That is why they ship first, as stage 0.

## The prefix form

`Trait.member(args)`. The type-expression parser already collapses dotted names into a single
string (`grammar.rs`, "Dotted names are kept as one string"), so this costs nothing to parse.

Trait-qualification rather than type-qualification, because it is the form that makes zero-`Self`
members spellable at all:

```frog
trait Zero { func zero(): Self }

let m: Money = zero()       // inferred from the expected type
let m = Zero.zero(): Money  // explicit, `expr : Type` is already grammatical
```

`Zero.zero()` names *which trait's* `zero`; it still needs the expected type to pick the impl,
because that is the whole point of return-type dispatch. `Money.zero()` would name the impl
directly but is exactly the `List.push(xs, x)` shape the roadmap rejects, and it does not
generalize to two-`Self` members.

**Resolving a zero-`Self` call** is a search, but a narrow and diagnosable one: only over impls
of traits declaring a member with that name — typically one or two, never the whole program. If
the expected type is known, it is a lookup. If it is not, the error is "cannot infer which `zero`
is meant; annotate the expected type", which is a good error, unlike an overload-ambiguity error
listing nine candidates.

## Two-`Self` members

```frog
trait Ord { func compare(a: Self, b: Self): Ordering }

a.compare(b)          // dot form, step 2
Ord.compare(a, b)     // prefix form
a < b                 // operator sugar, see "Prelude traits"
```

Both parameters being `Self` is why members are functions rather than methods with a receiver;
a receiver model has to nominate one of the two as special and cannot express `zero()` at all.

## The cost, stated plainly

There are two spellings for two different things: `f(x)` for a free function, `x.f()` or
`Trait.f(x)` for a member. A reader cannot tell from `x.f()` alone which of steps 2 and 3 fired.

I think that is correct rather than a wart — it is the distinction between "a function about a
type" and "a function in a type's interface", and the language already draws exactly this line
with `provides Error` versus an ordinary `func`. But it is the one place this design spends
novelty that whole-program overloading would not, and it is the decision to revisit first if the
model turns out to chafe.

## Where the seam is

`compile_call` looks up `ctx.func_ids[&func_name]` from a `TypedExprKind::Var(name)` callable.
So **member resolution and monomorphization both terminate in the type checker writing a mangled
symbol name into the typed AST** — `Circle$area`, `max$Int` — and codegen keeps doing string
lookups, unchanged. The typed AST is already the right seam and it is already there. This is why
stages 0, 3, and 5 are all frontend-only work.

## Traits and unions

A union satisfies a trait iff every member does — already the rule for `Error`
(`type_implements`'s `Type::Union` arm) and it generalizes for free. Calling a member on a union
generates the dispatch `match`, exactly parallel to `u.f` being legal iff every member has a
compatible `.f` (`lower_field_access`'s union arm). Step 2 of the resolution order handles unions
without a special case: the lookup key is the union's type, and it hits iff every member has an
impl.

---

# Part 2 — Mutation and the receiver

`MUTABILITY.md` §3 requires every `mut` argument to be marked at the call site. Under UFCS that
would make the receiver form unusable for exactly the operations most worth chaining:

```frog
push(mut xs, x)         // required today
xs.push(x)              // desugars to push(xs, x) → rejected
```

**Decision: the receiver position is exempt. Every other argument position is not.**

```frog
xs.push(x)              // receiver: mutated operand is the leftmost token
push(mut xs, x)         // prefix: buried in an argument list, marker earns its keep
swap(mut a, mut b)      // unchanged
```

The rule states as: *the marker exists to make a mutated operand visually prominent; the receiver
position already is one.* That is a principled line rather than an ad-hoc carve-out, and it is
precisely what Rust — the language most careful about this — does via autoref: `vec.push(x)` has
no `&mut` at the call site either.

### Why this is affordable

The marker is semantically inert (see "What exists today"), so nothing downstream changes. And
the property `MUTABILITY.md` is protecting is already half-carried by the *declaration* site: a
`let`-bound variable cannot be passed as a `mut` argument at all — `push` rejects it with
"declare it with 'mut xs = ...'". So "which variables in this scope are writable" is answered
without reading any callee, marker or no marker. The call-site marker adds only "…and it is
written *at this statement*", which is a real increment but a smaller one, and its value is
proportional to how hidden the mutation is.

### What is given up

Adding `mut` to a member's receiver parameter silently changes the behaviour of existing dot-form
call sites, where marking would have made it a compile error at each one. This is contained: it
can only affect bindings already declared `mut`, and only through the dot form.

### The wrinkle, accepted knowingly

For a step-3 free function, `xs.push(x)` and `push(mut xs, x)` are the same call under different
marking rules, so a mechanical dot→prefix rewrite can change what is legal. Confined to step 3 —
step-2 members have no unqualified prefix form anyway.

Rejected alternative: "the first parameter is exempt" rather than "the receiver position is
exempt". It sounds more uniform and produces `swap(a, mut b)`, which is worse than either option.

**`MUTABILITY.md` needs the corresponding amendment** — this changes a decision stated there, and
the two documents must not disagree.

---

# Part 3 — Structural traits by default

`DESIGN.md` is explicit about the aesthetic:

> nb. I don't want to do a bunch of `#[derive(...)]` annotations like Rust, that's too heavyweight
> for our intended ergonomics. `data` records should have auto derived implementations for
> Display, Eq, Ord, Serialize, DBTable, etc

And `type_implements` already does this for `Eq`. Formalizing it:

| Trait | Structural for every `data`? | Derived how |
| --- | --- | --- |
| `Eq` | yes | field-wise conjunction — already implemented, `desugar_struct_eq` |
| `Ord` | yes | lexicographic by declaration order (`DESIGN.md`) |
| `Show` | yes | field-wise; needs writing |
| `Truthy` | **no** | — see "The `Truthy` exception" |
| `Error` | no — granted by `provides` | — |
| user traits | no | — |

This is what makes derives mostly disappear as a feature: `alice < bob` works with no annotation,
per `DESIGN.md`, and `provides Error` stays a deliberate grant, per `ERRORS.md`.

### Explicit override replaces the whole impl

`data Vec2(x: Float, y: Float) provides Ord { ... }` overrides the structural default. The rule
for partial overrides — supplying one member of a three-member trait — is: **an explicit
`provides` block replaces the entire impl.** Unsupplied members fall back to the *trait's* default
body, never to the structural derive. An impl that is half-derived and half-written is
unauditable, which is the opposite of what this language is for.

### Two consequences worth naming

**Field order becomes semantically load-bearing.** Lexicographic `Ord` by declaration order means
reordering fields in a `data` declaration is a behaviour change. `DESIGN.md` already assumes this;
once it ships it is something a reader can trip over, and it belongs in the language reference.

**Structural derivation must recurse.** A struct is `Eq` only if every field's type is `Eq`.
Today `type_implements` returns `true` for any struct and the per-field comparison fails later —
the comments call this deliberate, and the *place* is right, but with a `List` field the user
currently gets an error about `==` on lists rather than about `Eq` on their struct. Fix when this
is formalized: check the fields, report at the struct.

### Deferred: opt-out

`data Password(hash: Str) without Ord` has a real use case (don't let secrets be sorted, don't
offer `==` where it is meaningless), but it is not needed to ship any stage below, and `without`
is already spoken for by the capability sketch. Revisit when something actually wants it.

---

# Part 4 — Generics

## `Type::Named`

`Type::Struct(String)` and `Type::List(Box<Type>)` collapse into one constructor:

```rust
Type::Named { name: String, args: Vec<Type> }
```

`List` stops being a special case and becomes a prelude declaration with a builtin
representation; `Dict<K, V>` becomes writable; unification recurses into `args`. 70 match sites,
no user-visible change, lands and is tested on its own.

## Type schemes — the piece earlier drafts missed

There is no generalization or instantiation today (see "What exists today"). Before any `<T>`
syntax means anything, the checker needs:

- **Schemes.** A `func`'s entry in `ctx` becomes `forall <binders>. Type` rather than a bare
  `Type`. Lambdas keep inferring everything — `func` declares, lambdas infer, the line the
  language already draws — but a `let`-bound lambda should generalize too, or
  `let f = x -> x + x; f(1); f(1.5)` stays an error, which is indefensible once the language
  claims to have generics.
- **Instantiation at call sites.** Fresh variables per use, replacing today's accidental sharing
  through the one global substitution map.
- **REPL persistence.** `TypeCheckerCheckpoint` snapshots `substitutions` per entry and
  `state.rs` restores it on error. Schemes and the instantiation cache have to join that
  protocol, and a generic function defined at one prompt must accept new instantiations at later
  ones.

This is the actual risk in the plan. It is invisible in the syntax and it touches `unify`,
`lookup`, and every `Type::TypeVar` site.

## Syntax

```frog
data Pair<A, B>(fst: A, snd: B)
data Outcome<T> is Ok(value: T) | Err(error: Error)
data Dict<K, V>(...)

trait Container<T> { func get(c: Self, i: Int): T }

func first<A, B>(p: Pair<A, B>): A = p.fst
func max<T: Ord>(x: T, y: T): T = if x > y then x else y

let d: Dict<Str, List<Int>> = ...
```

### Why `<>` and not the parens already implemented

`TypeExpr::Apply` is parsed today with parens — `List(Int)` — and that is *already* unambiguous,
for the same reason `<>` would be: type position contains no comparison operators. So the
familiarity argument has to carry the change on its own, and the honest cost is the `>=` hack
plus a migration across README, tests, error messages, and `CONCURRENCY.md` (written throughout
in `List(Str)`, `Task(T)`).

The argument that actually decides it is the **declaration header**, not the use site:

```frog
data Pair(A, B)(fst: A, snd: B)      // two paren groups, unreadable
data Pair<A, B>(fst: A, snd: B)      // clearly better
```

One spelling everywhere, so use sites move too. The mixed option — `<>` in headers, parens at use
sites — is rejected deliberately rather than by omission: it would keep `CONCURRENCY.md`'s prose
valid, and it is still not worth two spellings for one concept.

### Why the ambiguity does not bite

`<>` is painful in C++, Java, and Rust because `f(a<b, c>(d))` parses both ways. That ambiguity
lives entirely in *expression* position, and this design closes it by construction: **explicit
type arguments never appear in expression position.** `zero<Int>()` is not writable; the expected
type resolves it. Patterns take no type arguments either: `is Ok(v)`, never `is Ok<Int>(v)`.

Two lexer facts make it cheaper here than elsewhere:

- **No `>>`/`<<` tokens** (`tokens.rs` has only `Lt`, `Gt`, `LtEq`, `GtEq`), so `List<List<Int>>`
  lexes as `> >` with no token splitting — the most notorious `<>` implementation wart, absent.
- **`>=` is the one hazard**: `let x: List<Int>= 5` lexes `GtEq`. Fixed by splitting `GtEq` into
  `Gt`, `Eq` when the type parser expects a closing angle. A few lines, one place.

### Explicit binders, not inference from free variables

Inferring type parameters from free names (`func first(p: Pair(A, B)): A`) reads well one level
deep and has no principled answer one level down:

```frog
func outer<A, B>(a: A, b: B) = {
  func inner<B, C>(bb: B, c: C) = ...    // fresh B, shadows outer's — like any shadowed binding
  func other(bb: B, c: C) = ...          // error: C not bound. B is outer's, captured
}
```

With explicit binders this is ordinary scoping. With implicit binders, whether `inner`'s `B` is
fresh or captured is a convention, and either choice silently mis-types real programs.

A casing rule (lowercase = variable) was considered and rejected: a load-bearing distinction on a
single character's case, invisible at a glance, inverting the convention every reader arrives
with.

## Bounds

Inferred from the body wherever the body is visible; written at abstraction boundaries — trait
signatures and function types — mirroring the line `CONCURRENCY.md` draws for regions.

```frog
func max<T: Ord>(x: T, y: T): T = ...
func merge<K, V>(a: Dict<K, V>, b: Dict<K, V>): Dict<K, V> where K: Ord + Eq = ...
```

Inline `<T: Ord>` for one or two bounds, `where` as the overflow valve. Function *types* have no
binder list; a display form for their bounds has to be *invented* — `Type`'s `Display` currently
prints `[~t2:Num] -> ~t2:Num` and no `(Num t) =>` form exists.

## Variance

**Invariant.** `List<Int>` is not a `List<Int | Str>`. Lists are mutable, so covariance is
unsound. Worth stating because union subtyping (`T ≤ T | U`) makes people expect the opposite.

---

# Part 5 — Prelude traits

`Trait` becomes an open interned name rather than a closed enum, and the builtins become ordinary
declarations with operator sugar:

```frog
trait Num   { func add(a: Self, b: Self): Self  ... }    // a + b  ⇒  Num.add(a, b)
trait Eq    { func eq(a: Self, b: Self): Bool }          // a == b ⇒  Eq.eq(a, b)
trait Ord   { func compare(a: Self, b: Self): Ordering } // a < b  ⇒  Ord.compare(a, b) == Less
trait Error { }                                          // marker; ?/!/catch key off it
```

`type_implements`'s hardcoded arms become lookups: the `provides` table for granted traits, the
structural rule of Part 3 for `Eq`/`Ord`/`Show`.

**The performance caveat is real and gates the stage.** Naively desugaring `+` into a call would
wreck the `fib` benchmark. Prelude impls on primitives stay intrinsics that codegen recognizes and
emits inline; the desugaring is a type-checker-level rewrite that codegen pattern-matches back
out, in the same spirit as `?`/`catch` desugaring to `match` with no dedicated codegen node. The
criterion suite is the gate, not a judgement call.

## The `Truthy` exception

`Truthy` stays a compiler-internal marker and does **not** move to the prelude. It is not a
callable interface — it is a coercion relation consulted by `check_condition`/`coerce_truthy` to
decide what `if x` means. Making it user-implementable would let any impl redefine control flow,
which is a poor trade for uniformity.

So "the builtin traits all become ordinary declarations" is true of four of the five, and this is
the exception. Revisit only if a concrete case wants a user type to be conditional-position
legal.

## Error, once traits have bodies

`Error` is a marker today because there is no way to give it a member. Once there is, it almost
certainly wants `func message(e: Self): Str` with a structural default — every error path
currently formats through `print`, and `catch` has nothing to show a user. Not a blocker for any
stage below, but it is the first real customer for "a trait with a default body", so it is worth
designing at stage 5 rather than after.

---

# Part 6 — Capabilities are traits with implicit instances

`CONCURRENCY.md` writes `data Scope(A, T)(...) provides Spawn` and `func fan_out(...) can Spawn`.
Those are two different relations wearing one word:

| Relation | Meaning | Resolved |
| --- | --- | --- |
| `T provides C` | this type can serve as a C | statically, per type |
| `can C` in a signature | an instance of C must be ambient here | dynamically bound, statically tracked |

The unification: **a capability is a trait whose instance is passed implicitly rather than
explicitly.**

- `with e { ... }` binds `e` as the ambient instance of every trait `typeof(e)` provides, for the
  dynamic extent of the block.
- `can C` on a signature is an implicit parameter of type "some C", propagating to callers until a
  `with` discharges it.
- `without C` removes the binding.
- `spawn` and `blocking` need not be syntactic forms at all — `spawn` is a member of `trait Spawn`
  resolved against the ambient instance. `Io` likewise: "a swappable value in the ambient context"
  is exactly "an implicit instance of `trait Io`".

Scala's implicits and OCaml's modular implicits, arrived at from froglang's own `provides`. It
makes the concurrency design *less* special-cased, and it means the trait work is a prerequisite
that pays for itself twice. It also isolates where dynamic dispatch would actually be needed: an
ambient instance chosen at runtime is the one case monomorphization cannot erase.

---

# Dispatch and representation

Monomorphize. Trait calls resolve statically at each instantiation; no vtables. This keeps
`data Pair<A, B>(fst: A, snd: B)` instantiated at `Int` as two flat slots rather than two boxes,
and it is already the assumption `CONCURRENCY.md` makes for region checking.

**The existential mechanism already exists: closed unions.** "A value that is one of several
implementations" is `data Io is Blocking(...) | Fiber(...) | Test(...)` plus a `match` — faster,
closed-world, already implemented. `any Trait` / `dyn` is deliberately deferred until a case
proves unions insufficient; it slots in later without invalidating anything written meanwhile.
The known candidate is the swappable `Io` of `CONCURRENCY.md`, where a *host* might register an
implementation the frog program never named.

# Coherence

Froglang sees every declaration through one `FrogState`, so the checker rejects a duplicate
`(Trait, Type)` pair globally. No orphan rules. A real advantage over Rust's separate-compilation
constraints, and it should be spent deliberately.

Two consequences of the module system being a flat mangling pre-pass:

- **Impls are global; names are module-scoped.** An impl in `shapes.frog` affects `main.frog`
  whether or not `main` imported it. That is the right semantics — coherence means nothing
  otherwise — but it is an explicit exception to module scoping, and `modules.rs` must know not
  to mangle impl registrations the way it mangles top-level names.
- **The REPL caveat from `CONCURRENCY.md` applies unchanged.** An impl introduced at one prompt
  changes resolution for a function defined at an earlier one.

---

# Implementation plan

Reordered from earlier drafts by cost and by what unblocks the stdlib. Each stage is usable on its
own and forward-compatible with the next.

### Stage 0 — UFCS over free functions

Resolution steps 1 and 3 only; no traits, no impls, no overloading. Confined to
`lower_field_access` and the `Call` arm of `check_and_lower`: when a `FieldAccess` target has no
such field and the node is the callable of a `Call`, rewrite to `f(x, args)`.

Also implements the receiver exemption (Part 2). `is_push` currently requires its first argument
to be an `Expression::MutArg`, so the desugar has to thread a "came from receiver position" flag
through to the `mut`-argument validation.

- Delivers `xs.push(x)`, `s.concat(t)`, `items.filter(p).map(g)` — the roadmap's actual ask.
- Cheapest item in the plan; the only one that is user-visible on its own before stage 4.
- Requires the `MUTABILITY.md` amendment to land with it.

### Stage 1 — `Type::Named`

Collapse `Type::Struct` and `Type::List` into `Type::Named { name, args }`; unification,
normalization, display, and codegen layout recurse into `args`. 70 sites. No user-visible change —
a pure refactor, landed and tested on its own.

### Stage 2 — Type schemes and instantiation

Generalization at `func` and `let`-bound-lambda boundaries; fresh instantiation at call sites;
`TypeCheckerCheckpoint` extended to cover schemes; REPL persistence across entries. No new syntax.

Acceptance: `let f = x -> x + x; f(1); f(1.5)` type-checks. That one line is the whole stage.

**This is the risk in the plan**, not the trait stage. It should be scheduled as such.

### Stage 3 — Generic syntax and monomorphization

`<>` binders on `data` and `func`; `TypeExpr::Apply`'s delimiter switches; `GtEq` splitting in the
type parser; `List` becomes a prelude declaration; monomorphization at instantiation, emitting
mangled symbol names into the typed AST. `List(T)` → `List<T>` migration across README, tests,
error messages, and `CONCURRENCY.md`.

### Stage 4 — A real standard library

`len`, `map`, `filter`, `fold`, string functions, `Dict<K, V>`. Stages 1–3 exist to make this
writable; this stage is what proves they worked. Note there is *no* user-callable `len` today, so
this is also the first stage that makes the language usable for ordinary programs.

### Stage 5 — Traits

`trait` declarations with function members and default bodies; `Self` resolution; `provides` blocks
in both positions; namespaced member registration; resolution step 2; trait-qualified prefix form;
zero-`Self` resolution by expected type; the global coherence check; `Error` gains `message`.

Deliberately after stage 4: traits are only strictly required for *bounded* generics, and the five
builtin bounds cover the stdlib's first hundred functions. If stage 4 completes without hitting a
bound it cannot express, that is information worth having before writing this stage.

### Stage 6 — Prelude traits and structural derivation

`Trait` becomes open; `Num`/`Eq`/`Ord`/`Error` become prelude declarations; `Truthy` stays
internal; operators desugar to member calls; primitive impls stay codegen intrinsics; structural
`Eq`/`Ord`/`Show` for `data` with explicit-override-replaces-the-impl; field-recursive checking.

Benchmarks gate this stage — `fib` especially, via the criterion suite.

### Stage 7 — Capabilities

`provides`/`can`/`without` as implicit instances. Merges with `CONCURRENCY.md` stage 7.

---

# Deferred

Not foundational; none of them constrain the above.

- **Associated types.** `trait Iter { type Item }` versus generic `trait Iter<Item>` — matters for
  `for x in xs` over user types; decide when iteration is designed.
- **Blanket impls.** Whole-program coherence makes them tractable later; they complicate
  resolution and buy little initially.
- **Higher-kinded parameters.** Not needed for anything on the roadmap.
- **`any Trait` / boxed existentials.** See "Dispatch and representation".
- **Opt-out from structural traits.** See Part 3.
- **Annotation-driven derives** (`#db:model` and friends). Structural defaults remove the need for
  `#derive(Eq)` specifically; annotations are still wanted for serde/DB work, on their own track.
- **Trait members in `data` fields, and variance thereof.**

# Open questions

- **Does `[Int]` survive as sugar for `List<Int>`?** Leaning yes: it matches the `[1, 2, 3]`
  literal syntax, and dropping it makes every list type in every error message longer. But
  `Type`'s `Display` prints `[Int]` while `TypeExpr`'s prints `List(Int)`, and that inconsistency
  has to be resolved one way or the other at stage 3.
- **How much bound inference?** Inferring bounds from a `func` body is what lambdas already do,
  but it means a public signature is not fully written in its header. Alternative: infer, then
  *require* the inferred bounds to be written, rustc-suggestion style.
- **Do prelude operator traits admit user impls on user types?** `data Vec2(...) provides Num`
  giving `+` is the obvious want and the obvious route to operator abuse.
- **A display form for bounds on function types.** None exists; `[~t2:Num] -> ~t2:Num` is what
  prints today. Needed before stage 3 ships anything user-facing.
- **Error messages for instantiation failures.** `CONCURRENCY.md` already flags the C++-template
  failure mode; per-instantiation trait errors have the same shape and need "instantiated from
  here" chains.
- **Which of steps 2 and 3 fired?** A reader cannot tell from `x.f()` alone. Probably a tooling
  answer (hover, `frog explain`) rather than a syntax one, but it is the ergonomic cost of Part 1
  and should be watched.

# Prior art

- **Rust** — `trait`/`impl` separation, inline and `where` bounds, monomorphization, receiver
  exemption from call-site mutation marking via autoref (all adopted); orphan rules and coherence
  (unnecessary here); receiver methods, turbofish, and `#[derive]` ceremony (all declined).
- **Haskell** — type classes as functions rather than methods, dispatch including on return type
  (`zero(): Self`), superclass-free simplicity (adopted); higher-kinded classes (deferred).
- **TypeScript / Java / C# / Kotlin** — `<T>` syntax and uppercase convention, the precedent this
  design defers to.
- **Go** — implicit structural interface satisfaction (declined for *semantic* traits: `Error` is
  deliberately granted, and `ERRORS.md` argues why — but adopted wholesale for *structural* ones,
  which is Part 3); annotations (the model for the deferred serde/DB work).
- **Scala / OCaml modular implicits** — implicit instance resolution, the model for capabilities.
- **Nim / D** — UFCS as a rewrite rule rather than a method system; step 3 is theirs exactly.
- **Swift** — `mutating func` and the dynamic exclusivity enforcement froglang avoids by having no
  aliases.
- **Zig** — comptime-as-generics (declined; a separate evaluation model is a larger novelty spend
  than `<>`), and per-instantiation checking (adopted for regions in `CONCURRENCY.md`; the same
  trade applies here).
