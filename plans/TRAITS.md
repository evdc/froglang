# Traits, Generics, and Methods

Status: **in progress**. Stages 0-2 (`Implementation plan`, below) are implemented — UFCS,
`Type::Named`, and type schemes/instantiation. Nothing past that is. Syntax past this point is
still a sketch and should be expected to change; the *decisions* in "Foundational choices" are
the part meant to be stable, since they are the ones that are expensive to revisit later.

This supersedes the roadmap's "Traits/interfaces, explicit-style" and "User-definable generics"
bullets, and answers both the duality that bullet flags between `provides` and impl blocks and
the method-syntax sketches at the bottom of `roadmap.md`.

## What exists today

Verified against the tree, not remembered — several earlier drafts of this document were wrong
about these, and the errors pointed at the wrong stage being the risky one. **This section is a
snapshot from before Stages 0-2 landed** — kept as-is because it's the reasoning that justified
the staging order, not a claim about the current tree; see the `Implementation plan` section
below for what's actually true now. In particular the "No polymorphism of any kind" example
below now type-checks (Stage 2), and `Type::List(Box<Type>)`/`Type::Struct(String)` no longer
exist (Stage 1 collapsed them into `Type::Named`).

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
~~Today `type_implements` returns `true` for any struct and the per-field comparison fails later~~
— **done**, and it was worse than described: `build_struct_eq`'s synthesized per-field `Binary`
nodes are never re-checked by `builtin_op_type`, so nothing failed "later" at all. A non-`Eq`
field compiled into a raw pointer comparison and `W(xs=[1]) == W(xs=[1])` answered `false`.
`type_implements_rec` now recurses into field types (and into a generic instantiation's `args`),
with a `seen` list making it total over a type that reaches itself.

### `List` and union equality

Two more types had to answer the `Eq` question, since the recursion above now asks it of every
field. Both are decided in codegen, alongside `print_list`/`print_union` and for the same reason:
only codegen still knows the static type, and the runtime would compare a `Str` by address.

- **`List(T)` is `Eq` iff `T` is** — structural, like Python's, which is what `[1, 2] == [1, 2]`
  is expected to mean. It can't be desugared into a fixed conjunction the way a struct's is (the
  length is a runtime value), so codegen emits the element loop: `eq_list`, lengths first, then
  elements pairwise with an early exit.
- **A union is `Eq` iff every member is** — `eq_union`, the equality counterpart of `print_union`:
  find the member the left operand holds, ask whether the right one holds the same, and if so
  unpack both carriers and compare them. The two share their tag-provenance helpers
  (`union_dispatch_cases`, `emit_union_tag_test`, `unpack_union_slots`) so they can't drift on the
  one detail that is silently wrong rather than loud — a nominal union's tag is its *declaration*
  index, an anonymous one's its normalized position.
- **`None` is `Eq`**, so an optional is. Without it `Int | None` failed the member rule and no
  optional could be compared at all.

With that, **every value type is `Eq`** — there is no longer a negative witness to write a test
against, since the only non-`Eq` types (`Never`, a function type) can be neither a struct field
nor a type argument. That is the intended end state: `Eq`/`Ord`/`Show` just work for value types.

### Recursive types are a codegen limit, not a trait answer

A type that encloses itself *is* structurally `Eq`; what's missing is a way to emit the
comparison, since each step of these walks monomorphizes one statically known type. So it is
rejected at the operator by `check_comparable` — sharing its walk with `check_printable`, which
predicts the same wall for printing — rather than by `type_implements` saying "not `Eq`", which
would name the wrong cause.

A cycle can close two ways, and only the first was being caught:

- through a **union member** (boxed), e.g. `data Node is Lit(v: Int) | Add(lhs: Node, rhs: Node)`;
- through a **`List` field**, e.g. `data Tree(v: Int, kids: List<Tree>)`. This one became reachable
  when `DATA.md` stage 0 taught the printing walk to descend into a list's element type, and was a
  live bug: `print(tree)` overflowed the *compiler's* stack. Both walks now reject it.

Lifting the limit means an out-of-line comparison — one emitted function per type, recursing at
runtime rather than at codegen time — which is the same machinery a real `Show`/`repr` will need
for recursive types. Worth doing together, not before.

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

# Part 7 — `Linear`: a marker trait for resource identity

Motivated by `DATA.md`'s `Sink`, but not specific to it — see below for other customers.

## The problem

Every value in froglang today is copied, not aliased: `mut` params are copy-in/copy-out, `List`/
`Str` deep-clone on any aliasing-risk read (`codegen/mod.rs`'s `Move`/`Copy` liveness
classification). That is exactly right for data, and exactly wrong for a handle to something
external — a `Sink`, a file, a lock guard, a channel endpoint, a one-shot future. Writing to one
copy of a sink and reading from another is not "two independent values that happen to look
alike," the way two copies of a `List` are; it's the same resource observed through a broken
window. The earlier framing of this problem (an earlier draft of `DATA.md`'s Stage 4) was "such
types are exempt from value semantics" — workable, but a blanket carve-out rather than a checked
property, and it doesn't generalize to "the compiler catches me if I alias one of these."

## `Linear`, not a new trait mechanism

`Linear` is an ordinary marker trait, structurally identical to `Error`: no members, granted by
`provides`, never structurally derived. Nothing about the trait-resolution machinery in Part 1
changes.

```frog
data StrBuf(...) provides Sink, Linear
```

**Why `Linear` rather than `Handle` or `IO`**: the property being enforced is "duplicating this
value is a bug," not "this represents an external resource" (`Handle`) or "this does I/O" (`IO`).
Locks, channel endpoints, one-shot futures/promises, transaction handles, and capability tokens
are all real customers with nothing to do with I/O — `IO` would misname the mechanism and invite
someone to reach for it on the wrong axis.

## No supertraits needed

`trait Sink: Handle` was the first draft of this idea, and it doesn't fit: **TRAITS.md has no
trait-inheritance mechanism today**, and adding one raises the same "two traits declare the same
member name" question Part 1 spent effort avoiding for ordinary traits — a real feature, not
earned by this alone. Instead: a type states both markers (`provides Sink, Linear`), and impl
registration — already the place duplicate `(Trait, Type)` pairs are rejected, see "Coherence" —
gets one more targeted check: registering `Sink` without `Linear` on the same type is an error.
That buys "every `Sink` is `Linear`" as a checked conjunction, not a general inheritance feature.
Revisit only if a second, unrelated case wants real supertraits.

## The check itself

No new algorithm class, and reuses infrastructure that already exists:

- `codegen/mod.rs`'s liveness pass already classifies every read of a GC-pointer-bearing binding
  as `Move` (last use, no clone) or `Copy` (aliasing risk, clone before use). For an ordinary type
  this decides "clone or don't"; for a `Linear` type, a `Copy`-classified read becomes a **type
  error** ("`buf` is `Linear`; it can only be moved or passed `mut`, not aliased") instead of a
  silent clone. Emitted at typeck, before codegen ever sees it.
- At a branch join (`if`/`match`), a `Linear` binding must be consumed on every path or none —
  the same fixpoint the existing `mut`-liveness back-edge analysis (`transfer_loop`) already
  computes, extended to error rather than merge when the arms disagree.
- No lifetimes, no region variables, no annotation burden beyond what `mut` already asks for.
  froglang has no borrowing (`&`) — `mut` is copy-in/copy-out, never a reference — so this needs
  none of the flow-sensitive lifetime inference that makes Rust's borrow checker (as opposed to
  its ownership rules) hard to build or to use. See "Prior art" below for the specific precedent.
- A `Linear` value's GC representation can be a genuine identity handle — `clone_obj` returns the
  same pointer rather than deep-copying — since the type checker has already ruled out the
  `Copy`-classified read that would make that observable as aliasing.

## Relationship to `CONCURRENCY.md`'s regions

Orthogonal axes, meant to compose, on purpose — the same split as Rust's ownership (can this be
duplicated) versus borrowing (how long can a reference live). Region-bound types (`CONCURRENCY.md`
Rule B) answer *where a value may travel*: it can't escape its `with` block. They do **not**
answer *how many live references exist within the block* — Rule B's own text permits a bound value
to be "passed as a function argument, bound to a local, and read freely," which allows
`let g = f` inside the region. `Linear` is what closes that gap. A real `File` wants both:

```frog
data File(fd: Int) provides Read, Write, Sink, Linear
```

region-bound so it can't outlive its `with` block (no use-after-close — already a stated win of
regions), and `Linear` so it can't be silently duplicated within the block (no double-write
through an alias — a case regions alone don't cover). `Scope`/`Task(T)` are plausible second
customers once `Linear` exists — nothing stops `let s2 = scope_value` today — though that's a note
for `CONCURRENCY.md` to pick up, not a blocker here.

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

### Stage 0 — UFCS over free functions — **done** (`d3abff8`)

Resolution steps 1 and 3 only; no traits, no impls, no overloading. Confined to
`lower_field_access` and the `Call` arm of `check_and_lower`: when a `FieldAccess` target has no
such field and the node is the callable of a `Call`, rewrite to `f(x, args)`.

Also implements the receiver exemption (Part 2). `is_push` currently requires its first argument
to be an `Expression::MutArg`, so the desugar has to thread a "came from receiver position" flag
through to the `mut`-argument validation.

- Delivers `xs.push(x)`, `s.concat(t)`, `items.filter(p).map(g)` — the roadmap's actual ask.
- Cheapest item in the plan; the only one that is user-visible on its own before stage 4.
- Requires the `MUTABILITY.md` amendment to land with it.

### Stage 1 — `Type::Named` — **done** (`86da661`, `4d767dd`)

Collapse `Type::Struct` and `Type::List` into `Type::Named { name, args }`; unification,
normalization, display, and codegen layout recurse into `args`. 70 sites. No user-visible change —
a pure refactor, landed and tested on its own.

Landed in two commits (accessor migration, then the enum swap itself) rather than one, to keep
`cargo test` green at each step given the load-bearing hazards below. Fixed the `List(~t)` /
`List(Int)` unification gap as a side effect — `unify` had no structural arm for `Type::List`
at all before this.

**Hazards that turned out to matter, for the next person touching this code**: `Display`'s output
feeds `normalize`'s sort key, which feeds `nominal_member_index`'s runtime union tags — it had to
stay byte-identical (`[T]` for `List`, bare name for a zero-arg `Named`). `clone_if_owned`,
`list_elem_kind`, and `union_is_recursive` in `codegen/mod.rs` all had `List`/`Struct`-specific
logic (a deliberately-narrow copy-on-write predicate, a runtime type tag, and a cycle-breaking
rule) that had to be preserved by name (`Named{name: "List", ..}`) rather than widened to "any
`Named` with args" or "any zero-arg `Named`".

### Stage 2 — Type schemes and instantiation — **done** (`324b543`, `a6c30d4`, `8d4bb6d`)

Generalization at `func` and `let`-bound-lambda boundaries; fresh instantiation at call sites;
`TypeCheckerCheckpoint` extended to cover schemes; REPL persistence across entries. No new syntax.

Acceptance: `let f = x -> x + x; f(1); f(1.5)` type-checks — done, verbatim.

**This turned out to have two more moving parts than the plan anticipated**, both worth recording
here since Stage 3 will re-encounter them:

1. **Even a single instantiation didn't compile without extra work.** A generalized
   declaration's own `params`/`return_type` are never resolved to a concrete type by ordinary
   checking — `instantiate` mints a *fresh* `TypeVar` per call site precisely so calls don't
   contaminate the declaration or each other, so the declaration's own binder never gets a
   `substitutions` entry even after its one call site fully resolves. `make_sig` builds a
   Cranelift signature straight from the declaration's stored types with no resolution step
   (`cl_type` silently defaults an unresolved `TypeVar` to `I64`). Fixed by
   `TypeChecker::resolve_single_instantiations`: once `check_generic_monomorphism` has proven
   there's exactly one concrete instantiation, zip the declared type against it to recover
   `binder -> concrete`, write that into `substitutions`, and rewrite every node's stored type
   throughout the whole typed AST (including `Function` nodes' own `params`/`return_type`) via
   `lookup`. This is a genuine (if deliberately narrow — no duplication, no mangled symbols)
   slice of what Stage 3 does for real.
2. **The codegen gate needs to span REPL entries, not just one.** `check_generic_monomorphism`'s
   own walk only sees the current entry's typed AST; a scheme minted at entry 1 and instantiated
   differently at entry 5 would otherwise reach codegen as a raw Cranelift verifier panic instead
   of a clean error. `generic_instantiations` (checkpointed like `substitutions`) tracks each
   generalized name's confirmed instantiation across the whole session.

**Known remaining boundary, left for Stage 3 — closed by sub-stage 3b**: a generic declared in one
REPL entry and never called there, then called for the first time in a *later* entry, used to fail
(safely — a caught Cranelift verifier panic, not silent corruption — but without the gate's clean
message). Sub-stage 3b's `generic_templates` (a declaration's template survives past its own entry)
fixes this for real: on-demand compilation at first call site, however many entries later.

### Stage 3 — Generic syntax and monomorphization

`<>` binders on `data` and `func`; `TypeExpr::Apply`'s delimiter switches; `GtEq` splitting in the
type parser; `List`'s arity check folded into the same generic-declaration table `data Name<A>`
uses (not a prelude declaration — see 3c below for why that fuller version was skipped);
monomorphization at instantiation, emitting mangled symbol names into the typed AST. `List(T)` →
`List<T>` migration across README, tests, error messages, and `CONCURRENCY.md`.

#### Sub-stage 3a — Generic `data` declarations — **done**

`<A, B>` binders on `data` (`func` binders are Stage 3b, untouched); `TypeExpr::Apply`'s delimiter
switched from `(...)` to `<...>` everywhere (paren-style type application retired, migrated across
`froglang-core/tests/`); `Grammar::expect_close_angle` splits the `X<Y>=5` `GtEq` lexer hazard.

**The struct-instantiation design from the top of this file held exactly as predicted**: no mangled
symbols, no duplicated compiled bodies. What it turned out to need instead:

1. **A struct's stored field template needs *two* different kinds of instantiation**, depending on
   whether the concrete type arguments are already known or still being inferred:
   - **Construction** (`Name(...)`) doesn't know its arguments up front — they're inferred from the
     constructor call's own argument types. `TypeChecker::instantiate_struct` mints one *fresh*
     `TypeVar` per binder per call (exactly `instantiate`'s per-call-fresh-renaming move, applied to
     a struct's binders instead of a scheme's free variables) so two `Pair(...)` calls in one
     program don't fight over a shared global substitution.
   - **Reading** an already-typed value's field (`FieldAccess`, place-assignment, UFCS field lookup)
     already has concrete `args` sitting in the resolved `Type::Named` — no inference needed, just
     substitution. `TypeChecker::materialize_struct` does that directly.
2. **Codegen needs a *distinct* concrete layout per instantiation, keyed by something codegen can
   compute from the type alone.** `StructDefs` is a flat `HashMap<String, Vec<(String, Type)>>`
   threaded through ~50 call sites in `codegen/mod.rs` — threading a second `struct_type_params`
   table through all of them (the originally-planned shape) would have meant touching every one.
   Instead: `Type::struct_key()` reuses `Display`'s existing `Name(Arg1, Arg2)` rendering for a
   non-empty-`args` `Named` as the lookup key (a bare name, unchanged, for a non-generic struct), and
   `materialize_struct`/`instantiate_struct` register each instantiation's concrete field list into
   `struct_defs` under that key the moment typeck first computes it (construction or field access,
   whichever happens first) — memoized, so a repeat lookup of the same instantiation is free. By the
   time codegen runs, every instantiation appearing anywhere in the typed program is already
   present; `codegen::struct_fields`/`field_slice_range` needed exactly one line changed each
   (`ty.as_struct_name()` → `ty.struct_key()`), no signature changes, no new parameter threaded
   anywhere. `as_struct_name` itself stays a bare-name accessor (now just widened to accept non-empty
   `args`) — used wherever *identity*, not layout, is what's wanted (e.g. `print_value`'s printed
   prefix, which must say `Pair(...)`, never the mangled `Pair(Int, Str)(...)`).
3. **Generic unions are out of scope, rejected explicitly.** A struct's "instantiation" is a layout
   substitution; a nominal union's members are boxed/tagged (`FrogVariant`) — different enough
   machinery that folding them in wasn't attempted. `hoist_data_decls` rejects `data Name<A> is
   X | Y` with a clear error rather than silently mistyping it.
4. **A pre-existing gap, left alone on purpose**: `build_struct_eq` (the `==` desugaring into
   per-field comparisons) still reads a struct's *un-substituted* template — untouched by this
   substage, since fixing it would require the same materialize-on-read treatment threaded into a
   third place, and no test exercises `==` on a generic struct's concrete field values end-to-end.
   `type_implements`'s `Eq` arm (what actually gates whether the `==` operator type-checks at all) is
   fixed: it recurses into a generic instantiation's `args`, so `Pair<Int, Int>` is `Eq` and
   `Box<List<Int>>` is correctly rejected — but the desugared comparison's own per-field types, for a
   generic struct specifically, aren't verified beyond that gate. Matches the file's pre-existing
   "structural derivation must recurse" note (Part 3) — not newly introduced here, not fixed here.

#### Sub-stage 3b — Generic `func`/let-bound-lambda monomorphization — **done**

Real monomorphization: a generalized name (Stage 2's inferred generalization — see below for why no
new `<A, B>` syntax was added) may now be called at any number of distinct concrete types, in one
entry or split across several REPL entries, and each distinct instantiation gets its own compiled
body under a mangled symbol name. `TypeChecker::check_generic_monomorphism` (the single-instantiation
rejection gate) and `resolve_single_instantiations` (its "make the one instantiation compile" helper)
are both gone, replaced by `TypeChecker::monomorphize_generics`.

1. **No `<A, B>` binder syntax was added to `func` declarations.** The plan's stated fallback —
   "the existing scheme-inference path may be sufficient" — held: Stage 2's `generalize` already
   infers exactly the right binder set from every unconstrained free type variable in a syntactic
   function value (`func` or let-bound lambda), and nothing about *compiling* N instantiations
   instead of 1 needed the binder set to originate from explicit syntax instead. Revisit only if a
   future stage needs a user-written trait bound beyond what inference can discover on its own.
2. **The declaration's template must survive past its own entry.** `generic_templates: HashMap<String,
   GenericTemplate>` (new `TypeChecker` field, checkpointed) retains each generalized declaration's
   binder list, declared (abstract) `Type::Function`, and its `TypedExprKind::Function`'s `params`/
   `return_type`/`body` — populated in `lower_assign` alongside `ctx.insert_generalized`, never
   removed once inserted. Keyed by a **per-declaration template symbol** (`name#42`, minted once per
   generalization and recorded on the scope-stack `Binding`), not by the source name — see the
   follow-up notes below for why a name is not an identity here. This is what closes Stage 2's documented
   boundary: a generic declared in one entry and never called there now monomorphizes correctly the
   first time a *later* entry calls it, at as many distinct types as it's ever called at.
3. **Mangling deliberately avoids `Display`.** `TypeChecker::mangle_type` is a dedicated,
   non-parseable-back string (`Int`, `Named_Arg1_Arg2`, `Fn_Arg_Result`, ...) — reusing `Display`
   would have coupled symbol names to the string Stage 1 already flagged as load-bearing for union-tag
   sort order. A full instantiation's mangled name is `name$mangled_binder_1$mangled_binder_2...`,
   built from the same `zip_binder_types` Stage 2 already had (declared type structurally zipped
   against the call site's resolved concrete type).
4. **One session-wide set of already-compiled mangled names (`emitted_instantiations`) avoids
   redundant recompilation.** `codegen::func_ids` already persists across entries (Stage 2 relied on
   this too) — a generic called at the same concrete type from two different entries reuses the
   first entry's `FuncId` rather than cloning+compiling the body again.
5. **A per-entry rewrite, not a whole-program one.** `monomorphize_generics` runs once per `eval`
   call: collect every `(name, concrete type)` pair this entry's typed AST actually references
   (`collect_generic_var_types`, now accumulating a *set* per name instead of erroring past the
   first); for each pair not already in `emitted_instantiations`, clone the template body, substitute
   binder `TypeVar`s via a **local** mapping (`substitute_types_deep` — deliberately not the shared
   session-wide `substitutions` map, which is single-valued per name and exactly incompatible with
   two live instantiations coexisting), and insert the specialized `Assign` before the entry's own
   tail statement (so the entry's result value is never accidentally reassigned to a declaration).
   Every un-substituted generic `Assign` is stripped from what reaches codegen — only mangled
   instantiations do — and every `Var` reference (including a recursive self-call inside a freshly
   cloned body, which is what makes recursive generics monomorphize correctly too) is rewritten to
   its instantiation's mangled name in one final pass (`rewrite_call_sites`).
6. **An unrelated latent bug surfaced and was fixed in passing**: `lower_call_arg`/`lower_widen`
   stamp a `Coerce`/`Widen` node's `.ty` with the callee's parameter type *as it stood at that call
   site* — for a generalized function this is a fresh per-call `TypeVar`, only resolved later via
   `unify` writing into `self.substitutions`. Nothing re-resolves it in place afterward unless
   something walks the whole entry's typed tree through `lookup`. Stage 2's `resolve_types_deep` did
   this (unconditionally, over the whole entry, whenever any instantiation existed) but only as a
   side effect of its single-instantiation freeze; losing that when `resolve_single_instantiations`
   was deleted reintroduced the bug (surfaced immediately as a codegen panic, `"no widening from Int
   to TypeVar"`, in every generics test). Fixed by keeping exactly that whole-entry resolve step —
   `substitute_types_deep` called with an empty mapping is exactly Stage 2's `resolve_types_deep` —
   run over every statement before instantiations are inserted, not only inside a generic's own body.
7. **The single collection pass was not enough, and is now a fixed point.** Filed originally as a
   speculative "known gap" about type-changing recursion, this turned out to bite the ordinary case
   too: a generic whose body calls *another* generic (`func id(x) = x; func idpair(a) = id(a)`).
   The inner `id` reference sits at `idpair`'s own un-resolved binder `TypeVar` in the
   pre-monomorphization tree, so collecting once over that tree records a garbage instantiation and
   never emits `id$Int` at all — a `no entry found for key` codegen panic. `monomorphize_generics`
   now drains a worklist: every body it emits is re-run through `collect_generic_var_types` (its
   binders are concrete by then, so the calls inside it finally name real instantiations) and
   anything new goes back on the queue. A body's mangled name is claimed in `emitted_instantiations`
   *before* its own body is walked, which is the recursion guard.

Acceptance, run end-to-end rather than only checked: `let f = x -> x + x; f(1); f(1.5)` now compiles
and runs both instantiations correctly (`tests/test_generics_stage2.rs`'s
`a_generic_used_at_two_types_runs_both_instantiations_correctly` — previously the rejection case);
a generic declared in one REPL entry and called for the first time in a later one, at multiple
distinct types across further entries, also runs correctly
(`tests/test_state.rs`'s `test_generic_declared_without_being_called_monomorphizes_on_first_call_in_a_later_entry`).

#### Sub-stage 3c — `List`'s arity check folded into `struct_type_params` — **done**

Scoped down from the plan's original "`List` becomes a prelude declaration" framing. That version
was explicitly flagged as the risky one (`FrogState::new()`'s no-stdlib callers, used by most of
the existing test suite, would need unconditional prelude injection) and the plan itself said to
skip it if a smaller unification sufficed — it did.

What actually shipped: `resolve_type_expr`'s `TypeExpr::Apply` arm had two branches doing the same
arity check twice — a hardcoded `if name == LIST_NAME` branch, and a `struct_type_params.get(name)`
branch for user `data Name<A>` declarations. `TypeChecker::initial_struct_type_params()` (called
from both `new()` and `empty()`) now seeds `struct_type_params` with `"List" -> vec!["T"]` (arity
1, placeholder binder name never actually substituted anywhere), so both cases go through the same
lookup and the same length check. `List` keeps its own `Type::list(...)` construction — reached via
an inner `if name == LIST_NAME` *inside* the now-unified branch, after the shared arity check
passes — since `Type::list` builds `Type::Named{name: LIST_NAME, args: vec![elem]}` (per 3a's own
exploration notes, already identical to what the generic branch would build) but a plain struct
still needs the general `Type::Named{name, args}` fallthrough. The mismatch error message for
`List` specifically keeps its original wording (`"List takes exactly 1 type argument, got N"`,
pre-existing test coverage in `tests/test_type_expr.rs` locks it in) rather than the generic
`"{name} takes exactly N type argument(s), got M"` phrasing, since unifying the *check* doesn't
require unifying the *wording*.

Confirmed unaffected, by inspection and by the full suite staying green: `as_struct_name`/
`struct_key`/`is_struct` are keyed on `name != LIST_NAME` directly (not on `struct_type_params`
membership), so seeding the table doesn't make `List` start looking like a struct anywhere else —
`materialize_struct`/`instantiate_struct` (the two other `struct_type_params` readers) are only
ever reached through a `struct_defs`-gated or already-struct-confirmed path, and `struct_defs` has
no `"List"` entry (deliberately — no field template was added). Runtime representation, GC
scanning, and `codegen`'s `is_list()`/`as_list_elem()`/`list_elem_kind` special-casing are all
completely untouched — this substage only touched the type-checking arity-lookup seam.

New coverage: `tests/test_type_expr.rs`'s
`test_list_and_a_user_generic_struct_are_arity_checked_through_the_same_table` builds a user
`data Box<A>` alongside `List` in one program and checks both a `Box<Int, Str>` arity mismatch and
a `List<Int, Str>` arity mismatch are caught, plus that a correctly-aritied `Box<Int>` and
`List<Int>` coexist and type-check normally — demonstrating the shared table holds both
registrations without either clobbering the other.

#### Stage 3 follow-up: what a code review of the above found

Five defects, all in code the stages above introduced. Two were local; three shared one root cause.

- **Instantiation discovery had to become a fixed point** — see note 7 above.
- **`==` on a generic struct compared the wrong field types.** `desugar_struct_eq`/`build_struct_eq`
  looked the field layout up by the struct's *bare declared name*, which for a generic struct holds
  the placeholder-`TypeVar` template; the synthesized `FieldAccess` nodes were stamped at a `TypeVar`
  and codegen compared every field as a raw `I64`, so two identical `Pair(fst="ab", snd="cd")`
  values compared unequal. Both now take the resolved `Type` and go through `materialize_struct`,
  the way `lower_field_access`/`lower_place_assign` already did.
- **A name is not an identity.** `generalized_names` (a global, never-emptied, name-keyed set) drove
  three separate wrong answers at once: a later monomorphic `let f = ...` was stripped from codegen
  and its calls redirected to the earlier generic's instantiation; redefining a generic in a later
  REPL entry reused the previous definition's compiled body, because `emitted_instantiations` keyed
  `f$Int` off the bare name; and a plain parameter that merely shared a name with a generic
  (`func g(id: Int)` under a generic `id`) was treated as an instantiation, mangled to `id$None`,
  and rewritten into an unbound symbol. The fix is to decide *which declaration a reference belongs
  to* in scope, at lowering time, where the scope stack still exists: each generalization mints a
  unique template symbol, `Binding::generic_symbol` carries it, and every `Var` that resolves to a
  generalized declaration — plus the declaration's own `Assign` node — is emitted under that symbol
  instead of the source name. `generic_templates`' key set then *is* the "is this a generic
  reference?" test, and `generalized_names` is deleted. (`lower_ufcs_call` synthesizes its callee
  `Var` directly rather than through `lower_literal`, so it needs the same rename.)
- **An entry ending in a generic declaration** now evaluates to `None` rather than to the statement
  before it: the tail is pulled aside *before* the strip, and a stripped tail is replaced by
  `NoneLit`.

#### Stage 3 follow-up: functions in value position

The review's fifth finding — `let f = g; f(3)` panicking with `unbound variable in codegen` — was
correctly identified as pre-existing and *not* generic-specific: it reproduces for a plain `func`,
because froglang has no runtime representation of a function at all. There is no closure object, no
function pointer, and no indirect call; `codegen::compile_call` resolves a callee by name through
`func_ids` and panics on anything else. Every use of a function in value position therefore ended in
one of two panics rather than a diagnostic.

Rather than invent function values (a real feature — closures, an indirect-call ABI, GC tracing of
captured environments — and one no stage here has asked for yet), the resolution is to make the
language's actual position explicit and enforce it:

- **`let f = g` is an alias, not a copy.** The one thing a user reasonably wants from a function in
  value position is a second name for it, and that needs no runtime value: `lower_assign` binds `f`
  to `g`'s type, binders, codegen symbol (`Binding::symbol`, the same field the template symbols
  ride on) and `func_mut_params` entry, and the declaration compiles to nothing. An alias of a
  generic is therefore still generic, instantiating through the target's own template and sharing
  its already-emitted instantiations. `mut f = g` is deliberately *not* an alias — reassigning it
  would have to change what a call site resolves to at runtime, which is precisely the indirect
  call that doesn't exist.
- **Everything else is a spanned type error.** `validate_codegen_constraints` rejects a `Var` of
  function type anywhere except a call's callable (which the `Call` arm no longer recurses into),
  and a `Function` literal anywhere except as a declaration's own value. That covers `print(g)`,
  `[g]`, `mut f = g`, `map(xs, x -> x*2)`, and `(x -> x + 1)(5)` — all of which named the mangled
  or synthetic symbol in a panic message before, and now name the source identifier in a diagnostic
  (`Self::source_name` strips the `#42` off a template symbol).

This closes the last codegen panic reachable through the generics work. `tests/test_state.rs`'s
`test_codegen_panic_becomes_clean_error_and_state_survives` — which used the IIFE as its "codegen
still panics on something" specimen — now uses `print(none)` instead, the panic `plans/DATA.md`
Stage 1 is scheduled to remove.

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
- **General trait inheritance (`trait A: B`).** See Part 7 — deliberately not built for `Linear`;
  revisit only if a second, unrelated case wants it.

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
- **Does `Scope`/`Task(T)` want `Linear` too?** (Part 7.) Nothing in `CONCURRENCY.md` today stops
  `let s2 = scope_value`; not a blocker for `Linear` shipping, but worth `CONCURRENCY.md` picking
  up once it exists.

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
- **Clean** — uniqueness types, the precedent for Part 7: a value looks and is written like an
  ordinary one, but the type system guarantees exactly one live reference, licensing in-place
  mutation without aliasing risk. Rust's ownership rules (not its borrow checker — froglang has no
  borrowing) are the same idea reached independently; Part 7 is deliberately the cheap half of
  Rust's model, without the lifetime-annotation half, since froglang's `mut` gives temporary
  exclusive access via copy-in/copy-out rather than via references.
- **Zig** — comptime-as-generics (declined; a separate evaluation model is a larger novelty spend
  than `<>`), and per-instantiation checking (adopted for regions in `CONCURRENCY.md`; the same
  trade applies here).


---
