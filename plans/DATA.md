# Data, Notation, and Annotations

Status: **design**, with Stage 0 done and Stage 1's `print` half done. Everything
from Stage 3 on is a sketch; the *decisions* table is the part meant to be stable.

This supersedes the roadmap's "Annotations, and auto-deriving trait implementations" bullet and
the "sketching JSON/etc serde" section, and answers the reflection question posed there.

Related: `plans/TRAITS.md` (structural derivation, return-type dispatch, `Sink` consumers),
`plans/DESIGN.md` §Annotations (the original `#tag` sketch), `plans/ERRORS.md` (the span work
Stage 3 shares).

---

## What exists today

Verified against the tree, not remembered.

**froglang is much closer to round-tripping than it looks**, because struct construction syntax
*is* struct notation — `Person(name="Alice", age=42)` is simultaneously the code, the printed
form, and a valid literal. That fell out of using call syntax for construction rather than
inventing a separate struct-literal form, and it is the single most valuable property in this
document. Rust's `{:?}` deliberately does not have it.

Measured behaviour of `print` today:

| expression | prints | reads back? |
|---|---|---|
| `Person(name="Alice", age=42)` | `Person(name="Alice", age=42)` | **yes** |
| `Lit(42)` (positional fields) | `Lit(42)` | yes |
| `42` | `42` | yes |
| `0.1+0.2` | `0.30000000000000004` | yes — shortest-roundtrip |
| `1.0` | `1.0` | yes — the `.0` keeps Float ≠ Int in notation |
| `1e100` | `1e100` | **no** — the lexer has no exponent literal |
| `1.0/0.0` | `inf` | **no** — no literal |
| `none` | ~~*codegen panic*~~ → `none` | yes — *fixed, stage 1* |
| `[[1,2],[3]]` | ~~`[<list>, <list>]`~~ → `[[1, 2], [3]]` | yes — *fixed, stage 0* |
| `[Person(...)]` | ~~`[<struct>]`~~ → `[Person(name="Alice", age=42)]` | yes — *fixed, stage 0* |

Structural facts behind that table:

- **`print_value` (`codegen/mod.rs:1110`) is a type-directed structural walk** over a struct's
  flattened field layout, emitted inline per call site. It is a monomorphized `Show` derive
  written in Rust. `desugar_struct_eq` (`typeck.rs`) is a second such derive, at the AST layer.
  So froglang already has compile-time reflection, hardcoded, for two consumers, at two layers.
- ~~**List printing is the exception, and it is type-erased.**~~ *(Fixed in stage 0.)*
  `print_value`'s list arm used to collapse the element type to a one-byte `list_elem_kind`
  tag and call `frog_list_print`, which had no type information left and emitted
  `<struct>` / `<list>` placeholders. That one decision caused every `no` in the table
  except the float and `none` rows. The walk reaches through a list now.
- **`frog_float_print` uses Rust's `{:?}`** (`ffi.rs:166`). Shortest-roundtrip formatting, which
  is the hard half already correct — but it emits `1e100`, `inf`, and `NaN`, none of which the
  lexer accepts.
- **`frog_str_repr_print` uses Rust's `{:?}`** (`ffi.rs:119`), i.e. `escape_debug`. The lexer's
  escape set is `\n \t \r \\ \" \0` (`lexer.rs:202-210`) — no `\u{...}`, no `\x..` — and an
  **unknown escape silently degrades to the literal character**. So a control character prints as
  `\u{7}` and reads back as the four-character string `u{7}`: wrong, silently, with no error.
- **Blocks are a prefix parselet on `LeftBrace`** (`tokens.rs:185`), so `{` can begin an
  expression anywhere. But across all 38 `.frog` files, all 54 `{` are preceded by a keyword or
  `=` (`for..do`, `func..=`, `then`/`else`, `while`, `match`, `import`). Zero occurrences in
  argument, list-element, or expression-head position.
- **`{}` is already a parse error** and **`{x}` already collapses to `x`** (`grammar.rs:160-169`).
- **There is no interpolating string form.** The roadmap's `'${...}'` is aspirational, so the
  context-sensitivity hazard it would create for `repr` is future-only.
- **`Str` is immutable**, so any string-building derive is O(n²) without a new buffer type.

---

## The central decision: two tiers

The failure mode this design exists to avoid is letting JSON's limits back-propagate into
froglang's data model (cf. *The Shape of Data*). So the two are separated explicitly and never
share a mechanism:

| | **Tier 1 — frog notation** | **Tier 2 — interop** |
|---|---|---|
| entry points | `repr(x): Str`, `read(s): T` | `json.to_str(x)`, `json.parse(s): T` |
| output | froglang literal syntax | JSON, later others |
| lossless | yes, by law | no, best-effort |
| annotations | **none — canonical, non-configurable** | yes, `#json(...)` configures it |
| round-trip promised | yes | no |

Consequences worth stating:

- A frog program's own persistence format is frog notation, not JSON.
- `#json(...)` annotations can never change what `repr` emits. That is a stronger and simpler
  line than "annotations tune but never switch on" — in Tier 1 they do not exist at all.
- Tier 2 is allowed to be ugly and to have gaps. Tier 1 is not.

### The law

```
read(repr(x)) == x        for every x whose type has Show and Eq,  modulo NaN
```

Tested as a property test over generated values, in CI. This is the cheap, high-leverage move:
it converts "we intend to round-trip" into something that stays true. The NaN carve-out is via
`nan != nan`, a failure of `Eq` rather than of notation; state it beside the law so nobody
rediscovers it.

Two prerequisites, both of which are stages below:

- **`Show` and `Eq` must be derived together with the same recursion rule.** A type printable but
  not comparable has an untestable law; a type comparable but not printable is a hole in the
  notation. TRAITS.md Part 3 already wants field-recursive checking for `Eq`; do `Show` in the
  same pass.
- **`read` is return-type directed** — the same mechanism TRAITS.md specifies for `zero(): Self`.
  This resolves the notation's one real elision: `repr([])` is `[]` with no element type, and
  `read("[]"): List(Int)` supplies it from context.

### Why froglang needs no tagged literals

Clojure needs `#inst` because its reader has no expected type. froglang reads into a typed
context, which is strictly more information than a tag, so the notation carries no type
annotations of its own. The honest cost: **`repr` output alone is not self-describing.** A
`.frogdata` file on disk needs its type named out of band. That is the right trade for a
statically typed language, and it is another reason Tier 2 continues to exist.

### Why the notation is a tree

Value semantics mean no reference types, so no cycles, so `repr` is total and needs no seen-set,
no depth limit, and no `&ref` notation. This is *The Shape of Data*'s "trees over graphs"
arrived at from froglang's own side, and it is worth protecting: **do not add reference types.**

One active threat: `project_list_aliasing_gap` — if `List(T)` can alias through `mut` bindings,
then `mut xs = []; xs.push(xs)` is a cycle and `repr` does not terminate. That reframes the gap
as more than a semantic wart; it is the one thing that can make the notation non-total, and it
should be closed or disproved before the law is asserted.

---

## Sequence

Ordered by leverage, with the shared unblockers first. Stages 0–4 unblock *both* the notation
tier and the interop tier; the syntax changes are deliberately last.

| # | Stage | Unblocks | Depends on |
|---|---|---|---|
| 0 | **done** — List printing into the codegen walk | everything | — |
| 1 | **`print` half done** — `print` / `repr` split; `Show`+`Eq` totality | the law | 0 |
| 2 | Float and string notation fixes | the law | — |
| 3 | Source map: fn-ptr → span | diagnostics, function printing | — |
| 4 | `StrBuf` / `Sink` | every serializer | — |
| 5 | `repr` / `read` at the typed-AST layer; the property test | Tier 1 | 1, 2, 4 |
| 6 | Annotations: syntax, typed declarations, validation | Tier 2, host-side libs | — |
| 7 | Host exposure of the declaration table | ORM/DB use case | 6 |
| 8 | JSON interop tier | Tier 2 | 5, 6 |
| — | Deferred: collection literals, user reflection | — | generics |

Stages 2, 3, 4, and 6 are independent of each other and of 0/1; they can land in any order or in
parallel.

---

## Stage 0 — list printing into the codegen walk — **done**

**The single highest-leverage item in this document, and the cheapest.**

Replace the `list_elem_kind` hand-off with a loop emitted in `print_value` that calls
`print_value` recursively on each element with the *static* element type. Delete `list_elem_kind`
and the `kind` parameter of `frog_list_print`/`frog_list_println`.

Fixes nested lists, struct elements, and union elements at once. It is a prerequisite for any
serializer, `repr`, or derive worth having, because without it the type-directed walk cannot
reach through the most common container in the language.

Do this first and independently of every design decision below.

### What it took

`print_list` in `codegen/mod.rs` emits the element loop, modelled on
`compile_for_loop`'s element read (`frog_list_len` for the count, slot
`i * stride + leaf` per leaf, no bounds check since the header bounds `i`), and
recurses into `print_value` with the static element type. `frog_list_print`/
`frog_list_println`/`list_elem_kind` are gone, and the top-level `print` path calls
`print_list` directly instead of a runtime function.

Two things fell out that the plan didn't call:

- **An empty list literal's element type is still a `TypeVar`.** The loop body is dead
  (nothing fixed the variable, so there is no element), but it is still emitted, so
  `print_value` needs an arm. It prints `<?>` — Stage 1's rule that a placeholder may
  exist but must not look like notation, arriving one stage early.
- **`check_printable` had to learn to descend through a list.** It predicts
  `print_union`'s recursion guard at typeck time so the user gets a spanned error rather
  than a codegen panic; now that the walk reaches through a list, `print([Add(..)])`
  would otherwise have slipped past it into the panic.

`Str` elements now print as quoted literals (`["a"]`, not `[a]`), because the list walk
shares `print_value`'s leaf policy instead of having its own — which is the point, and is
what the notation needs.

---

## Stage 1 — `print` diagnostic, `repr` canonical

Two functions over one walk, differing only in the leaf policy:

| | `print(x)` | `repr(x): Str` |
|---|---|---|
| tier | diagnostic | canonical notation |
| totality | total — accepts anything | requires `Show` |
| law | none | `read(repr(x)) == x` |
| function value | `<func x -> x + n @ repl:3:12>` | compile error |
| layer | codegen (direct FFI, no allocation) | typed AST (allocating, Stage 5) |

**The `<...>` bracketing does real work.** `<` can never begin a frog literal, so a
non-notation form that leaks into something being `read` fails loudly instead of silently. That
is the discipline: a placeholder may exist, it may not lie about being notation.

This also replaces today's `print codegen does not support {:?}` panic with either a `<...>` form
(`print`) or a type error naming the type and why it has no notation (`repr`).

**Fix while here:** the `none` literal is lowercase but `print_value`'s `Type::None` arm emits
`None` (the *type* name), and the top-level path doesn't reach that arm at all — `print(none)`
panics. Emit `none`; the value literal is what has to read back.

**Derive `Show` alongside `Eq`, field-recursively.** Today `type_implements` returns `true` for
any struct and the per-field comparison fails later; TRAITS.md Part 3 already calls for fixing
that. Report at the struct, not at the field.

**Guard against drift** between the two implementations of the struct walk (codegen for `print`,
typed AST for `repr`) with a test asserting `print(x) == repr(x)` for every `Show` type.

### Status

Done, all of it on the `print` side:

- `print_value` emits lowercase `none`, and the top-level `print` path routes `None`
  (with structs and unions) through `print_value` instead of the scalar match, so
  `print(none)` prints instead of aborting codegen.
- **`Eq` now recurses into field types** (`type_implements_rec` +
  `struct_field_types`, an immutable counterpart to `materialize_struct`). This was worse
  than the plan recorded: `build_struct_eq`'s synthesized per-field `Binary` nodes are
  never re-checked by `builtin_op_type`, so a `List` field wasn't caught "later" at all —
  it compiled to a raw pointer comparison, and `W(xs=[1]) == W(xs=[1])` answered `false`.
  The error now names the struct. A type that reaches itself (a nominal union's variant
  carrying that union) terminates via a `seen` list and is treated as satisfied.

Not done, deliberately:

- **`Show` as a `Trait` variant.** Its only consumer is `repr`, which is Stage 5 by this
  document's own layer table, and `print` is total and requires no trait. The recursion
  in `type_implements_rec` is trait-generic, so adding the variant is the whole change
  when Stage 5 arrives.
- **`<func ... @ span>`.** Function values can't be used as values at all yet ("'g' is a
  function — it can be called, or given another name with 'let', but not used as a
  value"), so there is nothing to print. Stage 3's source map and this land together.
- **`print(x) == repr(x)` drift test.** Needs `repr`.

### Why two layers is correct here, not an accident

An earlier draft argued for consolidating both derives at the typed-AST seam. The tiers want
different strategies for real reasons:

- **codegen owns diagnostic printing.** Total, allocation-free, direct FFI calls. This is
  `print_value` as it exists.
- **the typed AST owns notation.** Allocating, format-parameterized, law-abiding. One lowering
  serves `repr`, `json.to_str`, and eventually prelude-defined `Show`/`Serialize`, by swapping
  the fragments — where a codegen emitter must be rewritten per format. It is inspectable, gets
  GC rooting right by construction because it is ordinary frog code, and is one step from
  TRAITS.md stage 6's "derives are prelude froglang code." It joins `desugar_struct_eq` and the
  `?`/`catch` desugaring.

---

## Stage 2 — float and string notation

**Floats.** Add exponent literals to the lexer (`1e100`, `1E5`, `1.5e-3`; do not swallow `e100`
as an identifier) — wanted independently of this document. Add `inf` and `nan` as lowercase
keyword literals, parallel to the existing `none` / `true` / `false`; `-inf` falls out of unary
minus. `{:?}`'s shortest-roundtrip formatting is already correct and stays.

NaN then breaks the law via `nan != nan` rather than via notation — the standard carve-out.

**Strings.** Stop borrowing Rust's `escape_debug`; write a frog-specific escaper pinned to
exactly the lexer's accepted set. Otherwise a control character prints `\u{7}` and reads back as
`u{7}`, wrong and silent. Two options, and this needs a decision:

1. Extend the lexer with `\u{...}` and make the escaper emit it. Round-trips everything.
2. Keep the escape set as-is and have `repr` reject strings containing characters it cannot
   spell. Smaller, and consistent with "no notation → error, never a lie."

Leaning (1) — a `Str` that cannot be `repr`'d is a hole in the notation, and `\u{...}` is one
lexer arm.

**Also fix**: the lexer's "unknown escape keeps the literal character" rule (`lexer.rs:209`) is
the mechanism that makes the failure silent. Rejecting unknown escapes is a small breaking change
that turns a class of silent wrongness into a parse error, and it is exactly the kind of thing a
language for generated code should do.

**Future constraint**: if an interpolating string form (`'${...}'`) is added, `repr` must emit
the non-interpolating form or escape `$`. Note it in the interpolation design when it happens.

---

## Stage 3 — source map

A static fn-ptr → span table, built at compile time, plus `FrogState` retaining entry sources.

**The cost is zero words per runtime value.** The mapping is per-*lambda*, not per-closure-value:
every closure created from `x -> x + n` shares one code pointer and one span. Closure values
already carry a function pointer.

Wanted for three things at once: `print`'s `<func ... @ repl:3:12>` form, rustc-style span
rendering (the roadmap's top usability item), and ERRORS.md phase 7's error return traces.

**Printed source is diagnostic, never notation.** `let n = 5; let f = x -> x + n` prints
`x -> x + n`, which does not read back — `n` is not in scope at the read site. Satisfying the law
would require printing the closure's substituted captures, which needs every captured value to be
`Show` and gets worse once specialization gives one lambda several bodies. Hence: functions have
no `Show`, `print` shows them in `<...>`, `repr` rejects them.

---

## Stage 4 — `StrBuf` / `Sink`

`Str` is immutable, so a derive that builds a string by concatenation is O(n²). Every serializer
in this document needs a mutable buffer.

Not currently on `roadmap.md` or in any plan. It is the real gate on Stage 5 and Stage 8, and it
is worth listing as its own item rather than discovering it inside them.

Minimum: a GC-managed growable byte buffer with `push_str`, `finish(): Str`. Whether it is the
same object as a future `Sink` trait (so a serializer can write to a file or a socket without
materializing a `Str`) is worth deciding at design time rather than retrofitting.

---

## Stage 5 — `repr` / `read`, and the law under test

`repr` as a typed-AST desugar over the `Sink`, sharing the walk's shape with `print`. `read(s): T`
return-type directed, per TRAITS.md's `zero(): Self` mechanism.

Then the property test, which is the actual deliverable of this stage.

**Known dependency**: qualified variant names. `Circle(r=1)` printed from an anonymous union is
ambiguous if two `data ... is` declarations both have a `Circle`, so the notation wants
`Shape.Circle(r=1)`. README's "Up Next" records that qualified variant names are not yet
spellable in a `TypeExpr` nor resolvable as a pattern. Same fix, and Tier 1 needs it.

---

## Stage 6 — annotations

### Sigil and placement

`#`, per `DESIGN.md`. Two notes for the record: `//` comments mean the Python-comment reading is
resolvable from context, and `@` is reserved for other uses. `CONCURRENCY.md`'s `errors=#collect`
atom sketch is withdrawn — `data ErrorBehavior is Race | Collect | Ignore` with expected-type
inference is the explicit version of what atoms accomplish, and it is typed, validated, and
exhaustively matchable. **froglang does not need atoms because it has nominal unions.**

Attachment rule, stated once and total:

> An annotation attaches **backward** to the declaration ending on the same line to its left, if
> there is one; otherwise **forward** to the next declaration.

```frog
#db.model
data Person(
    id: Int          #primary_key
    name: Str        #unique
    last_name: Str   #json(name="lastName")
    #json(skip)
    internal: Str
)
```

The trailing form is what makes annotations worth having; leading-only is the `#[serde(rename)]`
ceremony `DESIGN.md` line 124 explicitly rejects.

### Annotations are typed values, not raw strings

`DESIGN.md`'s original sketch lexes `#...` as a raw string to the next whitespace, Go-style. That
is rejected. Go's struct tags are the canonical example of the failure: `json:"nmae"` compiles,
runs, and silently produces wrong output — bad enough that `go vet` grew a dedicated checker. In
a language whose stated purpose is trustworthy generated code, a silently-ignored annotation is
exactly the bug class froglang exists to catch, and a model that has read a great deal of Rust
will confidently emit `#serde(rename="lastName")`.

Instead, an annotation is a **compile-time-constructed value of a declared type**:

```frog
annotation json(name: Str? = none, skip: Bool = false, required: Bool = false)
annotation primary_key()
annotation db.model(table: Str? = none)
```

`#json(name="lastName")` then type-checks as a struct literal, reusing
`check_struct_construction` wholesale: unknown annotation → error naming the nearest match; wrong
field name, wrong type, missing required field → errors. Fields must be literals; const-eval is
restricted to literals initially.

Sugar that preserves Go's density:

- `#primary_key` ≡ `#primary_key()` for a fieldless annotation
- `#json("lastName")` ≡ positional construction — positional fields already exist
  (`is_positional_fields`), so single-field annotations get the terse form for free
- `#json(skip)` ≡ `skip=true` for a `Bool` field — one rule, and the pattern is ubiquitous

What is given up: a library cannot invent an annotation at the call site without declaring it.
That is the point, and the declaration is one line.

What is gained beyond validation: **reflection returns typed values.** The roadmap's
`f.annotations[0].split(":")[1]` string-fumbling disappears, and format-scoping is automatic
because each format's annotations are a distinct type.

### Placement targets

`data` declarations, fields, and variants first; `func` declarations and parameters next
(`#test`, `#deprecated`, `#get("/users")`). Java-style `#target(Field)` restrictions are deferred
— the consumer errors on misplacement anyway.

### Dependency

Struct-field defaults, which `DESIGN.md` wants for keyword arguments regardless.

---

## Stage 7 — host exposure

Expose the declaration table — names, fields, types, annotations — through the embedding API:

```rust
let defs = state.data_decls();
```

Underrated and nearly free. froglang is embeddable, and `DESIGN.md`'s motivating DB/ORM example
is a **host-side** library: a Rust ORM reads that table, builds `INSERT` statements, and never
needs froglang-level reflection at all. `FrogValue` already deep-copies values out with type
information.

This covers the DB half of `DESIGN.md`'s example for the cost of a getter.

**Decide**: a host embedding froglang already has `serde_json`. If froglang grows its own JSON,
name which is canonical for a value crossing the boundary, or `Person` → JSON will have two
answers that differ on exactly the annotations.

---

## Stage 8 — JSON

Generated per type at the typed-AST layer, annotation-configured, explicitly lossy. The
serialize direction is `print_value`'s walk with different fragments. The **deserialize direction
is the genuinely new work** — every existing derive *consumes* a statically-shaped value; parsing
*produces* one, filling flattened slots from dynamic input with per-field failure. Expect the
estimate to be wrong here and nowhere else.

`json.parse` is return-type directed, not `json.parse[Person](txt)` — TRAITS.md rules out
explicit type arguments in expression position, and that decision is load-bearing for the `<>`
ambiguity argument. The roadmap's own alternative needs no new syntax, since `expr : Type`
already parses:

```frog
let alice: Person = json.parse(txt)?
let alice = json.parse(txt): Person
```

Four semantics to settle:

1. **`null` vs missing.** `Str?` cannot distinguish them: union normalization flattens and
   dedups, so froglang structurally cannot express serde's `Option<Option<T>>`. **Decision:
   `#json(required)`**, making absence an error while `null` yields `none`. Adding a distinct
   `missing` value with three-valued logic is rejected — it is deforming the data model to
   accommodate JSON, and JS's `null`/`undefined` and SQL's 3VL are the two most-regretted
   instances of exactly that.
2. **Union wire shape.** `#json(tag="kind")` on the union declaration. External tagging
   (`{"Circle":{"r":1}}`) is the unambiguous default; internal (`{"kind":"Circle","r":1}`) is
   what people want. Pick a default and make the other reachable.
3. **Anonymous unions do not round-trip.** `Int | Str` serializes and parses back ambiguously;
   `Int | Float` is worse, since JSON has one number type. Make `Deserialize` a property of
   *nominal* unions only, and reject `parse` at an anonymous union type with a real message.
   Serialization stays permissive.
4. **Is `Serialize` automatic?** TRAITS.md Part 3 makes `Eq`/`Ord`/`Show` structural for every
   `data`, so consistency says yes, and then `#json(...)` only ever *configures* a derive that
   already exists. The counter-case is `data Password(hash: Str)`, which should not be silently
   serializable — that is TRAITS.md's deferred `without` opt-out, and serde is the first feature
   that makes it concrete.

---

## Reflection

The roadmap frames this as runtime vs compile time. Given froglang's representation choices the
two are not close in cost, and the ranking is the opposite of what the roadmap's pseudo-code
assumes.

**Runtime reflection is the expensive option here.** In Go/Java/Python `reflect.get_field(d, name)`
is cheap because every value carries a type pointer and fields are uniformly addressable.
froglang has neither: structs are unboxed flattened fields with no header, so there is no runtime
path from a value to its type; and `get_field` has no return type, because froglang has no `Any`
— you would have to invent a boxed dynamic value, GC-integrate it, and give the checker a story
for it, undercutting the unboxed-struct performance work. Descriptor tables also defeat
dead-code elimination, against the fast-compilation goal.

It buys exactly one thing: reflecting over a value whose type the compiler cannot see — a
host-provided value, or `any Trait`. TRAITS.md already defers `any Trait`. **Runtime reflection
and `any Trait` arrive together or not at all.**

**Compile-time reflection is the destination**, deferred until TRAITS.md stages 2–3 (schemes,
monomorphization) land. `reflect.fields(T)` as a compile-time constant list, the loop unrolled at
monomorphization, `reflect.get(d, f)` becoming a static field access — Zig's `@typeInfo` +
`inline for`, arrived at from froglang's own monomorphization pass. The roadmap's `to_json`
pseudo-code then compiles literally as written, minus the string-splitting.

Note how much smaller this is than the roadmap's macro sketch: no quote/unquote, no AST
manipulation, no `macro` keyword — just "evaluate a restricted subset during monomorphization."
`DESIGN.md`'s macro-footgun concern mostly does not apply.

The endgame is worth naming: `Show`, `Eq`, `Ord`, `Serialize` stop being Rust code in
`codegen/mod.rs` and become **prelude froglang code over comptime reflection**. That deletes
special cases (TRAITS.md's stated measure of success) and is a real down-payment on self-hosting.

**Sequence: Stage 7 now (free), Stage 8 next (delivers serde), comptime reflection when
TRAITS.md stages 2–3 land. Runtime reflection declined.**

---

## Deferred

- **Dict / Set literal syntax.** Revisit when generics land, since `Dict<K,V>` is blocked on them
  anyway. The analysis so far, for the record:
  - `{...}` is unavailable: froglang is expression-oriented, so blocks appear in expression
    position and `let d = {` is genuinely ambiguous. JS escapes this only via its
    statement/expression distinction, which froglang does not have.
  - Restricting blocks to keyword-led positions (`=`, `then`, `else`, `do`, `->`, `while`) is
    *empirically* viable — all 54 `{` in the corpus already qualify — but buys less than it
    appears: **`{}` is ambiguous between an empty dict and an empty set regardless**, so a
    nominal head is needed anyway. It also makes `let x = { let a = 1; a }` fail with "unexpected
    `let` in dict literal", misdiagnosing a natural mistake, and turns every future
    block-introducing keyword (`with`, `scope`, `catch`) into a grammar edit.
  - Leading candidate: **typed collection literals** — `Dict{"a": 1}`, `Set{1,2,3}`, `Dict{}` —
    which need no grammar restriction, extend to user collections, compose with generics
    (`Dict<Str,Int>{}`), and state the rule that resolves the key-vs-name confusion: **parens +
    `=` is the nominal form where the left side is a declared name (`Person(name="Alice")`);
    braces + `:` is the keyed form where the left side is an evaluated expression.** Bare
    `{k: v}` sugar stays available as a purely additive later change.
  - Runner-up: `[k: v]`, one character instead of five, at the cost of `[x : Int]` colliding with
    ascription and one delimiter carrying two collection types.
  - Rejected: `do ... end` (triple-books `do`, which is already comprehension syntax and
    `CONCURRENCY.md`'s effect-perform keyword, and costs tokens on the most common construct);
    `()` for blocks (collides with the tuples/records `DESIGN.md` wants, and over-subscribes
    parens further).
  - Whatever is chosen: **print order must be deterministic** (insertion order) or `repr` is
    useless for diffs and golden tests, even though dict equality is order-independent.
- **User-facing `reflect`.** See above; wanted eventually, blocked on TRAITS.md stages 2–3.
- **Records and tuples** (`DESIGN.md`'s `(foo=1, hello="world")`). If they land, their notation
  must be their literal too, and `DESIGN.md` line 98 already flags the grouping-paren collision.
- **`without` opt-out from structural derives.** TRAITS.md Part 3 defers it; serde's
  `data Password(hash: Str)` case is the first concrete customer.
- **Non-JSON interop formats.** The `Sink`-based lowering is format-parameterized by design, so
  CBOR / query-params slot in without new machinery.

---

## Decisions

| Question | Decision | Why |
|---|---|---|
| Notation tiers | Two, never sharing a mechanism | Keeps JSON's limits out of the data model |
| Annotations in Tier 1 | None at all | `repr` is canonical; configurable canon is a contradiction |
| `print` vs `repr` | Split: diagnostic vs canonical | Lets functions print without breaking the law |
| Non-notation forms | `<...>`, which cannot begin a literal | Fails loudly on `read`, never lies |
| The law | `read(repr(x)) == x`, modulo NaN, property-tested | Makes the intent enforceable |
| `Show` / `Eq` | Derived together, field-recursively | The law needs equal totality |
| `read` / `parse` type args | Return-type directed, never `f[T](x)` | TRAITS.md forbids expression-position type args |
| Self-description | None; the expected type supplies it | Strictly more information than a tag |
| Cycles | Prevented by value semantics; keep it that way | `repr` stays total, no seen-set |
| Annotation form | Typed, declared, validated | Go's silent-typo failure is disqualifying here |
| Annotation sigil | `#` | `//` disambiguates; `@` reserved |
| Atoms / symbols | Not added — use nominal unions | Typed, validated, exhaustive |
| `null` vs missing | `#json(required)`, no `missing` value | Don't deform the model for JSON |
| Runtime reflection | Declined | No type header, no `Any`; ships with `any Trait` or never |
| Compile-time reflection | The destination, deferred | Blocked on TRAITS.md stages 2–3 |
| Derive layers | codegen for diagnostic, typed AST for notation | Different tiers want different strategies |

---

## Open questions

- **`\u{...}` in the lexer, or `repr` rejects unspellable strings?** Leaning the former.
- **Should unknown escapes become a parse error?** Small breaking change; turns silent wrongness
  into a diagnostic, which is what the language is for.
- **Which JSON stack is canonical** for a value crossing the embedding boundary.
- **Default union tagging** — external or internal.
- **Is `StrBuf` the same object as a `Sink` trait**, or does `Sink` come later as an abstraction
  over it? Deciding at design time is cheaper than retrofitting.
- **Does closing `project_list_aliasing_gap` become a prerequisite** for asserting the law, or
  can `repr` ship with a depth limit as a stopgap? Prefer the former.

---

## Prior art

- **Clojure** — `(= x (read-string (pr-str x)))` as a stated law, one notation for code and data
  (both adopted); tagged literals (declined — the expected type carries more).
- **Go** — struct tags as the annotation model (adopted in placement, rejected in typing:
  `go vet`'s existence is the argument); `encoding/json`'s reflection (declined).
- **Zig** — `@typeInfo` + `inline for` as comptime reflection, the model for the deferred stage.
- **Python** — dict keys as evaluated expressions with no implicit identifier quoting, against
  JS's `{x: 1}` footgun.
- **Rust** — shortest-roundtrip float formatting (adopted, already in use via `{:?}`); `Debug`
  as deliberately non-parseable (declined — froglang gets the parseable form for free);
  `#[derive]` ceremony (declined, per `DESIGN.md`).
- **Lua** — `[k]=v` table constructors, the precedent behind the `[k: v]` runner-up.
- *The Shape of Data* (Jamie Brandon) — co-design the data model and its notation; trees over
  graphs; IDs over pointers. froglang already satisfies the last by having no reference types.

---

possible syntax decision
- We don't need anonymous record types, we have `data` types
- `["a": 1]` maps (expression keys)
- `Set(1, 2, 3)` is a valid built-in constructor, also `Set(...xs)` once we have that implemented
  - if one of (list, map, set) has to get demoted to a normal constructor then sets are less fundamental than maps
- Tuples are still open: either `(1, "a")` and `(,)` empty; or `#[1, "a"]` maybe