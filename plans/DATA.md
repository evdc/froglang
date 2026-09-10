# Data, Notation, and Annotations

Status: **design**, with Stages 0, 2, and 3 done and Stage 1's `print` half done. Everything
from Stage 4 on is a sketch; the *decisions* table is the part meant to be stable.

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
| `1e100` | `1e100` | yes — *fixed, stage 2* |
| `1.0/0.0` | `inf` | yes — *fixed, stage 2* |
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
- ~~**`frog_float_print` uses Rust's `{:?}`**~~ *(Fixed in stage 2.)* Shortest-roundtrip
  formatting was the hard half already correct, but it emitted `1e100`, `inf`, and `NaN`, none
  of which the lexer accepted. Both float printers go through `notation::float_repr` now.
- ~~**`frog_str_repr_print` uses Rust's `{:?}`**~~ *(Fixed in stage 2.)* `escape_debug` spells a
  control character `\u{7}`, which the lexer had no escape for — and an **unknown escape
  silently degraded to the literal character**, so it read back as the four-character string
  `u{7}`: wrong, silently, with no error. `notation::escape_str` and the lexer now share one
  escape set, and an unknown escape is an error.
- **Blocks are a prefix parselet on `LeftBrace`** (`tokens.rs:185`), so `{` can begin an
  expression anywhere. But across all 38 `.frog` files, all 54 `{` are preceded by a keyword or
  `=` (`for..do`, `func..=`, `then`/`else`, `while`, `match`, `import`). Zero occurrences in
  argument, list-element, or expression-head position.
- **`{}` is already a parse error** and **`{x}` already collapses to `x`** (`grammar.rs:160-169`).
- **There is no interpolating string form.** The roadmap's `'${...}'` is aspirational, so the
  context-sensitivity hazard it would create for `repr` is future-only. *(No longer true as of
  2026-09-06 — see the "Future constraint" note below, and `plans/INTERPOLATION.md`.)*
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
| 1 | **done** — `print` / `repr` split; `Show`+`Eq` totality | the law | 0 |
| 2 | **done** — float and string notation fixes | the law | — |
| 3 | **done** — Source map: fn-ptr → span, rustc-style error rendering | diagnostics (function printing still blocked on function values existing) | — |
| 4 | `Sink` (implemented by `StrBuf`) — **`Linear`'s enforcement now exists** (`TRAITS.md` Part 7), `Sink` itself doesn't | every serializer | — |
| 5 | **done** — `repr` / `read` at the typed-AST layer; the property test | Tier 1 | 1, 2, 4 |
| 6 | **done** — Annotations: syntax, typed declarations, validation | Tier 2, host-side libs | — |
| 7 | Host exposure of the declaration table | ORM/DB use case | 6 |
| 8 | **done** — JSON interop tier | Tier 2 | 5, 6 |
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

Since done, on the `repr` side (Stage 5 landed this):

- **`Trait::Show` exists** (`typeck.rs`), exactly the trait-generic extension of
  `type_implements_rec` this section predicted — structural like `Eq`, with the same
  recursion, a `Type::Function` member excluded. `check_reprable`/`is_repr` consult it.
- **`print(x) == repr(x)` drift test**: `tests/test_repr.rs::repr_and_print_agree_on_every_form`,
  a fixed table across every `print_value` arm.

Still not done, deliberately:

- **`<func ... @ span>`.** Function values can't be used as values at all yet ("'g' is a
  function — it can be called, or given another name with 'let', but not used as a
  value"), so there is nothing to print. Stage 3's source map and this land together.

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

## Stage 2 — float and string notation — **done**

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

**Future constraint** *(resolved 2026-09-06 — `plans/INTERPOLATION.md`)*: if an interpolating
string form (`'${...}'`) is added, `repr` must emit the non-interpolating form or escape `$`.
Note it in the interpolation design when it happens.

**As built**: every `"..."` interpolates, so `escape_str` escapes `${` — and only `${`, so a
lone `$` stays unmarked — with `\$` added to the lexer's escape set on the same change, per
this module's rule that the two halves move together. The law needed one more thing this note
did not anticipate: `read` shares the parser, so **data** would have started interpolating
too. `Lexer::for_data`/`Parser::parse_data` keep notation non-interpolating, which is what
holds `read(repr(x)) == x` for a string containing `${`.

### What it took

Option (1) was taken: the lexer grew `\u{...}` (1–6 hex digits, `char::from_u32`'s range,
with a surrogate rejected rather than replaced), and unknown escapes are a lex error.

The formatters moved out of `ffi.rs` into **`src/notation.rs`**, a crate-root module, because
three places print a value and all three were formatting it differently: `runtime::ffi` (what a
compiled `print` calls), `state::FrogValue::display_str` (what the embedding API and the REPL
render — it was still on `{:?}` for both floats and strings), and stage 5's `repr`, which will
be the third. `float_repr` and `escape_str` are stated there as the lexer's counterpart, and
their unit tests are round trips *through the lexer* rather than expected strings, so the two
sides cannot drift silently again — which is the whole failure this stage existed to fix.

Three things the plan didn't call:

- **`inf`/`nan` are keyword tokens** (`Token::Inf`/`Token::Nan`, `#[prefix(Grammar::literal)]`),
  not lexed as `Token::Float(f64::NAN)` — `Token` derives `PartialEq`, and a `Float(NAN)` token
  is not equal to itself, which is a hazard to leave lying in the parser's token comparisons.
- **The exponent scan needs two characters of lookahead**, not one: `e` is a name character, so
  the sign may sit between it and the first digit, and `1 else x` / `1e` must keep their `e`
  for `read_name`.
- **A bad escape resyncs to the closing quote** instead of returning immediately. Returning
  early left the lexer positioned *inside* the string, so one mistyped escape cascaded into an
  unterminated-string error plus whatever the rest of the literal lexed as.

Not done, deliberately: `1e400` parses to `inf` rather than erroring, which is Rust's `f64`
parse behaviour and round-trips (it prints `inf`).

---

## Stage 3 — source map — **done**

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

### What it took

Built the source-map *infrastructure* and spent it on the one consumer that was actually ready:
rustc-style span rendering of type errors. `print`'s `<func ... @ repl:3:12>` form stays blocked
on function values existing at all — Stage 1's status note ("Function values can't be used as
values at all yet ... so there is nothing to print") turned out to still be exactly true;
`validate_codegen_constraints` (`typeck.rs`) statically rejects any function-typed value from
reaching codegen, so there is still no runtime closure value for a source-map lookup to describe.
That part of the plan's premise ("closure values already carry a function pointer") was aspirational,
not a description of what exists — worth recording so the next reader doesn't go looking for it.

What does exist now, and what it's for:

- **`codegen::Codegen::source_map()`** — a `Vec<FnSourceInfo { name, span, entry_id }>`, appended
  to by `compile_entry`'s existing Pass 1 (the pass that already declares a `FuncId` per top-level
  `func`/lambda) at zero extra walks. Checkpointed and truncated on a failed entry exactly like
  `func_ids` already was — same reasoning, same shape, so a rolled-back entry never leaves a stale
  source-map row pointing at code that was never defined.
- **`state::FrogState::entry_sources`** — one `EntrySource { filename, source }` per successful
  `eval`/`eval_file` call, indexed by the same `entry_id` `FnSourceInfo` and the JIT symbol mangling
  (`{name}__frogfn{entry_id}`) already used. A failed entry doesn't push one, matching
  `entry_count`'s own "only success consumes a number" rule.
- **`diagnostics::render_span`** — the actual payoff: `FrogError::Type`'s three construction sites
  in `eval_with_base` (the ones that used to do `e.to_string()` on a `Spanned<TypeError>`, which
  rendered as a bare `(line:col .. line:col)\tTypeError { msg: ".." }` Debug dump, span and all,
  and threw the source line away entirely) now render a rustc-shaped snippet: the offending source
  line, gutter, and a caret under the exact span. `FrogState` didn't retain `src` at the point an
  error was formatted before this — `entry_sources` (above) is what makes it available there, one
  reason the two land together instead of source-map first, rendering later.

One thing the plan didn't call: **`Position::line`/`.col` are 0-indexed** (`Lexer::current_line`
starts at `0`), not 1-indexed as the doc comments elsewhere loosely implied. `render_span` is the
first consumer to print a line number for a human, so it's the first place that had to get this
right — it converts on the way out (`+ 1`) rather than changing the lexer's internal convention,
which every existing span-comparison test already depends on.

Not done, deliberately: wiring `render_span` into `FrogError::Parse` (never actually constructed
today — `modules::resolve_source` folds parse errors into `FrogError::Module` before `eval_with_base`
ever sees one) or into `FrogError::Codegen` (a panic message, no span to render). Revisit either if
that changes.

---

## Stage 4 — `Sink`, implemented by `StrBuf`

Not currently on `roadmap.md` or in any plan. It is the real gate on Stage 5 and Stage 8, and it
is worth listing as its own item rather than discovering it inside them.

**Prerequisite partly cleared**: `Linear`'s enforcement (`TRAITS.md` Part 7) is implemented and
tested — as a built-in `Trait::Linear` variant, not a prelude declaration (TRAITS.md Stage 5, the
general `trait` machinery `Sink`'s own `trait Sink { ... }` declaration below still needs, doesn't
exist yet). See `TRAITS.md` Part 7's own status note for what that bought and what it didn't.
`Sink` itself — the trait declaration, `StrBuf`, `write_stdout`/file rewiring — is still unstarted.

### `StrBuf` is not the primitive here — `Sink` is

The original framing was "`Str` is immutable, so a derive that builds a string by concatenation
is O(n²), add a mutable buffer." That motivation turns out to be smaller than it looks: froglang
already has a mutable-accumulator idiom with no new type at all.

```frog
func emit(mut out: List<Str>, depth: Int): Int = {
    out.push("(")
    if depth > 0 then emit(mut out, depth - 1) else 0
    out.push(")")
    0
}
mut acc = []
emit(mut acc, 3)
acc.join("")   // "(((())))"
```

`mut` parameters thread an accumulator through recursion with the pushes visible to the caller —
copy-in/copy-out, no aliasing, the mutation marked at the call site the way the language already
insists on. `List<Str>` + `join` is O(n) bytes copied once; the O(n²) case is `s = s + x`, which
this was never going to reach for. What a byte-buffer type would still buy over it is a constant
factor — one GC object instead of one per fragment — which is real for a hot serializer but is
*tuning*, not a semantic gap, and tuning is exactly what shouldn't force a language decision this
early.

**The part that is semantic is identity, and it's the more important of the two reasons `Sink`
exists.** Streaming to a file or socket without materializing the whole output first is a real
memory-complexity difference — but the sharper point is that **a sink is the first thing in
froglang with identity.** Every value in the language today is copied, not aliased (`mut`
params included — copy-in/copy-out preserves that, it doesn't break it); nothing has "the same
object" semantics, and that absence is exactly what keeps `repr` total with no seen-set (see
"Why the notation is a tree," above). Writing to one sink twice is not writing to two copies of
it — a `Sink` value has to be checked against duplication, not merely documented against it.
That's a bigger decision than a buffer type, and it's the one worth making explicitly rather than
backing into via `StrBuf`. `TRAITS.md` Part 7 (`Linear`) is that decision: a marker trait, no new
trait-system machinery, checked by extending the liveness pass that already exists.

### The sketch

```frog
trait Sink {
    func write(mut self, s: Str): None
    func flush(mut self): Result<None, ErrMsg>
}
```

- **`mut self`, not a return-new-value method** — a `Sink` implementation needs no new mutation
  mechanism; `mut` is already the language's spelling for "this call has an externally visible
  effect." What makes aliasing through it safe is `Linear` (`TRAITS.md` Part 7), required
  alongside `Sink` on every implementer: `data StrBuf(...) provides Sink, Linear`.
- **Monomorphized per implementation, not `any Trait`.** A serializer's sink type is always
  known statically at its call site (`repr(x, mut out: StrBuf)` vs `repr(x, mut out: FileSink)`
  are two monomorphizations, exactly like any other generic function here) — so `Sink` needs
  none of TRAITS.md's deferred dynamic-dispatch machinery. `any Trait` and runtime reflection
  were declined together for the same reason (see "Reflection," below); `Sink` is the
  counterexample that shows a trait doesn't need that machinery just because it crosses an I/O
  boundary.
- **`StrBuf` is an implementation of `Sink`, not a peer of it** — the accumulating case, backed
  by (at minimum) `List<Str>` under the hood, or a dedicated growable byte buffer if a benchmark
  ever asks for the constant factor. Its representation is private either way, so which one it
  starts as is not a decision that has to be made now, and changing it later is not a signature
  change. This resolves the open question of whether `StrBuf` and `Sink` are the same object:
  they aren't, and `Sink` should land first since it's the thing every other decision here hangs
  off of.
- **`write_stdout`/`write_stderr`/file writes (`stdlib/fs.rs`) become `Sink` implementations**
  once `Sink` exists, rather than the free path-based functions they are today (`fs.rs`'s own
  doc comment already flags this: no file *handles* exist yet, every operation is whole-file).
  That is the concrete reason to want `Sink` beyond `repr`/serde — it is also the shape a real
  file-handle API needs, and inventing it twice (once for I/O, once for serialization) would be
  the special-case duplication this whole document argues against.

### `<<` — deliberately not sketched here

UFCS already gets most of what the operator would buy: `out.write(x)` reads as well as
`out << x` would, with no new token, no precedence question (`<<` sits next to `<`/`<=`, and
`a << b == c` needs a ruling), and no divergence from how every other method call in the
language looks. What the operator would actually change is that `out << x` visually *hides* the
mutation the way `f(mut x)` deliberately doesn't — defensible, since `Sink`'s `Linear` marker is
the same "this is safe to alias through" ruling that would justify hiding it, but that is the
identity decision above wearing different clothes. Decide `Sink`'s `Linear` story first (`TRAITS.md`
Part 7); the operator, if wanted, is a one-line sugar afterward, not a design of its own.

---

## Stage 5 — `repr` / `read`, and the law under test — **done**

`repr(x): Str` and `read(s): T | ReadError` both ship, the law is property-tested, and
`FrogState::new()` (not just `with_stdlib()`) has both — `read`/`repr` are core builtins
like `print`, not stdlib-gated.

**Known dependency, resolved before this stage started**: qualified variant names
(`Shape.Circle(r=1)`) were already spellable and resolvable — `grammar.rs` keeps a dotted
callee as one `FieldAccess` node, and `resolve_variant_callee` already handled it. The
README's "Up Next" entry recording this as outstanding was stale; Tier 1 needed nothing
here.

**Built without Stage 4 (`Sink`), contrary to this doc's own dependency listed above.**
`repr` concatenates via ordinary `Str + Str` (`Binary{Plus}` → `frog_str_concat`) and a
new internal `__str_join` builtin for `List<T>`'s per-element fragments, exactly the way
the "central decision" table always allowed (Tier 1 is canonical notation, not
performance-optimal notation) — `Sink`/`StrBuf` would cut the allocation count for a wide
struct, not enable the law. Recorded as a real, accepted cost, not a gap: see Stage 4's
own "Risks" note below for the O(n²)-in-fragment-count mitigation taken instead
(adjacent-`StrLit` folding at build time).

### Architecture

Two layers, each doing what it's already good at — not the codegen-emitter design this
section originally sketched:

- **`repr` is a typed-AST desugar** (`TypeChecker::desugar_notation`, beside
  `desugar_struct_eq`), run once per entry **after** `monomorphize_generics` and before
  `liveness::number_nodes` (both in `state.rs`'s `eval_with_base` and
  `codegen::compile_and_run`). Post-monomorphization placement is load-bearing, not
  cosmetic: a value's type is only guaranteed fully substituted at that point, and
  `self.lookup` on an unresolved binder earlier would risk emitting `"[]"` for a
  possibly-non-empty list. `build_repr`/`build_repr_struct`/`build_repr_list`/
  `build_repr_union` mirror `print_value`'s exact arm order and output format
  byte-for-byte (asserted by `test_repr.rs`'s drift test), synthesizing ordinary
  `FieldAccess`/`VariantField`/`IsVariant`/`Narrow`/`Comprehension` nodes — the same
  vocabulary `desugar_struct_eq`/`lower_match` already use, not a new mechanism.
- **`read` is a runtime parser plus a typed-AST desugar over it, not a codegen walk.**
  `repr`'s output is frog source by definition, so `runtime::read`'s authority on reading
  it back is `frontend::parser::Parser` itself — the same "round-trips through the lexer"
  discipline `notation.rs` already established, extended one level. `frog_read_open`
  parses once into an ordinary `Expression` tree; every other `frog_read_*` accessor is
  **sticky-error-and-continue** (records the first mismatch, returns an always-valid
  placeholder, keeps going) — which is what lets `build_read` synthesize one
  unconditional "happy path" expression per type, checked against `frog_read_failed()`
  exactly once at the end, with no early-exit control flow of its own. `read`'s dispatch
  hangs off `lower_expected` exactly like `zero(): Self` (TRAITS.md) does — return-type
  directed, no new mechanism there either.

Both `Show` (structural, mirrors `Eq`'s recursion) and the property test's law were built
as this section originally specified.

### What it took beyond the sketch

- **Return-type-directed dispatch already existed** (`zero_self_target`/
  `lower_zero_self_call`, TRAITS.md's `zero(): Self`) — `read` is one more arm in
  `lower_expected`, not new machinery.
- **`is_repr`'s `Show`/recursive-union checks had to defer past a bare generic
  `TypeVar`**, mirroring `join_operand_types`'s existing defer for `Num`/`Eq`/`Ord`
  bounds — otherwise `func show<T>(x: T): Str = repr(x)` could never type-check for any
  concrete `T`, since `Show` has no bound-inference story of its own yet. Re-checked in
  `desugar_notation` once monomorphization gives each call site a concrete type.
- **Union-into-union widening doesn't exist** (`lower_widen` rejects it explicitly —
  different tag numbering/representation). `read(s): Shape | ReadError`, where `Shape`
  is itself `Circle | Rect`, needs it: the narrow `Shape`-typed happy-path value can't be
  widened into the wider `Shape | ReadError` afterward. Fixed by building
  `VariantInit`/`Widen` nodes stamped with the **wide** target type from the start
  (`build_read_union_as`) rather than building narrow and widening after —
  `compile_variant_init` already recomputes a variant's tag/column layout from whatever
  type is stamped on the node, so this sidesteps the limitation instead of needing to
  lift it. `catch`'s own handler-widening hits the identical wall independently (a
  pre-existing gap, not something this stage introduced or fixed).
- **`TypedExprKind::Range`'s own doc comment is stale.** It says "eagerly materialized as
  `List<Int>`"; `lower_range` actually types a bare `a..b` as `Range<Int>` (RANGES.md),
  and codegen compiles it as the flat `(start, end)` pair that type's unboxed
  representation is — never allocating a list. `build_read_list`'s synthesized `0..len`
  iterable had to be typed `Range<Int>`, not `List<Int>`.
- **A real GC bug, exactly the kind this doc's own risk section anticipated**:
  `frog_read_str` was missing `jit_frame_guard!()` before its allocating call. Silent
  without `FROG_GC_STRESS=1`; under it, a `List<Str>` built via `read` silently aliased
  element N with element N+1, then a `bytes_allocated` underflow panic in `gc::sweep` on
  a later collection. Caught immediately because every new construct was tested under
  `FROG_GC_STRESS=1` from the start, not at the end — the mitigation this doc's own
  "Risks" section prescribes.
- **The property test found two pre-existing bugs unrelated to this stage**, both in
  `!`/`==`/`catch` machinery that existed before `repr`/`read` did and reproduce with
  plain hand-written source: (1) `==` is wrong for a union whose only members are `None`
  plus a struct/error type (tag narrowing is fine; only `==` is wrong); (2) `!` + `==`
  crashes the Cranelift verifier for an anonymous union mixing a `Bool`/`Float` member
  with another scalar, joined with an `Error`-providing type. Both reported; the
  generator and one hand-written test were adjusted to route around them rather than
  fixing them here.

### Verification

`tests/test_repr.rs` (20), `tests/test_read.rs` (21), `tests/test_notation_law.rs` (40
generated seeds) — all green, including under `FROG_GC_STRESS=1`. Full crate suite (44
binaries) unaffected.

---

## Stage 6 — annotations — **done**

Built as a **core slice**, deliberately narrower than "validation + host exposure": parsing,
`annotation` declarations, both attachment directions, and full typed validation all ship;
*consuming* a validated annotation does not, because nothing exists yet to consume one — that's
Stage 7 (host exposure) and Stage 8 (JSON), both still separate, later stages. A validated
annotation is checked and then discarded. See `tests/test_hash_annotations.rs` (26 tests) for the
full behavioral spec this section describes.

### What shipped, and one deliberate narrowing

- **The sigil, both attachment directions, both sugars, dotted names** — all exactly as sketched
  below (`#db.model`, trailing `#unique`/`#json(skip)`, `#primary_key` with no parens,
  `#json("lastName")` for a single-field annotation).
- **`annotation name(field: Type = default, ...)` declarations**, hoisted two-pass like `data` (so
  forward reference within one block works), reusing `Grammar::field_list` wholesale for the
  field grammar.
- **Validation**: unknown annotation, unknown field, wrong field type, missing required field,
  duplicate field — all reported at the `#name(...)` use site (or, for a missing-required error,
  at the decorated declaration, since there's no narrower span for "you didn't write this").
- **Narrowed from the sketch**: placement is *unrestricted* rather than validated against a
  target list (`data`/field/variant/`func`/param) — "Java-style `#target(Field)` restrictions are
  deferred, the consumer errors on misplacement anyway" turned out to mean exactly that: with no
  consumer built yet, there is nothing to define a valid-placement rule *against*. `#foo` on a
  bare `let` typechecks (against its annotation's own field shape) and is simply discarded. Also
  narrowed: whole-variant annotations (`#foo Circle(...)`) and function-parameter annotations
  are not wired up — a variant's own *fields* get them for free (they reuse `field_list`), and a
  top-level `func` declaration gets them for free (it's an ordinary `Assign`, and attachment is
  generic over any top-level statement), but the variant-as-a-whole and per-parameter cases
  would need their own grammar hookup, deferred as genuinely lower-value than what shipped.

### Struct-field defaults — the stated dependency, and how it was scoped

Built as literal-constant-only (`name: Type = <literal>`, unary-minus-on-a-number included), not
arbitrary expressions — the same restriction the sketch below states for annotation field values
("Fields must be literals; const-eval is restricted to literals initially"), extended to ordinary
struct fields too so one small `ConstValue` enum and one `eval_const_expr` serve both. Named
fields only (`Grammar::field_list` rejects `= default` on a positional field with a clear error) —
a positional field's default couldn't be triggered by an omitted keyword argument anyway, since
positional construction has no keywords to omit. Works on both a plain struct's fields and a
nominal union variant's (`data Shape(color: Str = "black") is Circle(...) | Square(side: Int = 1)`).

### Architecture

- **`ConstValue`** (`typeck.rs`) — the five-shape compile-time constant (`Int`/`Float`/`Bool`/
  `Str`/`None`) both a struct field's default and every annotation field value actually are.
  `eval_const_expr` produces one from a literal (or `-`-prefixed numeric literal) `Expression`,
  nothing else. `TypeChecker::const_value_to_typed` lifts one into an ordinary `TypedExpr` leaf
  (`IntLit`/`FloatLit`/.../`NoneLit`) so it can be widened into a wider declared slot (`T?`, a
  union member) through the *existing* `lower_widen` — no new widening machinery.
- **`lower_widen` is not itself a type check** — for a non-union target it silently returns its
  argument unchanged if the types don't already match, because every existing call site had
  already checked compatibility (`widens_to`/`unify`/`unify_with_one_union_member`) before
  calling it. This stage's own validation needed the identical check, so
  `TypeChecker::widen_const_checked` wraps `const_value_to_typed` + that same three-way
  compatibility test + `lower_widen`, and every one of this stage's validation sites goes through
  it — never through `lower_widen` directly. Caught by a hand-written test
  (`an_annotation_field_of_the_wrong_type_is_an_error`) that silently passed before this wrapper
  existed: `#json(name=5)` against a `Str` field validated with no error at all.
- **`annotation` declarations hoist two-pass**, exactly like `data` — `TypeChecker::
  hoist_annotation_decls` registers every `annotation name(...)` in a block before any `#name(...)`
  use in that same block is checked, so declaration order within one file doesn't matter (matching
  `data`'s existing forward-reference story).
- **Attachment desugars to one wrapper node, stripped before anything else runs.** `Grammar::
  leading_annotations`/`trailing_annotations` (parser.rs's `block`/`statement`, and
  `Grammar::field_list`) collect `#name(...)` runs and wrap the target in
  `Expression::Decorated { annotations, target }` (or set a `FieldDecl`'s own `annotations`
  directly, for the field/variant-field case). `TypeChecker::strip_and_validate_annotations`
  — run once per block, immediately after `hoist_annotation_decls` and before `hoist_data_decls`/
  `hoist_trait_names`/everything else — validates each annotation and unwraps `Decorated` back to
  its bare `target`, so every other pass in the compiler (hoisting, lowering, codegen) only ever
  sees the plain node it already knows how to handle. `Decorated` reaching `check_and_lower`
  itself is `unreachable!()`.
- **Same-line trailing detection needed no new mechanism.** `Token::Newline` is an explicit token
  in this lexer (never auto-skipped), so "the current token is `Hash`" already means "still on the
  same source line" — if a line break had occurred, `Newline` would be the current token instead.
  `trailing_annotations` is exactly that one check in a loop.

### A real bug found building this: module name-mangling

`frontend::modules`' per-file rewrite pass mangles every top-level declared name (so two modules
can each declare `Point` without colliding) — including, once `collect_names_in`/`rewrite`
learned about `Expression::AnnotationDecl`/`Decorated`, an `annotation` declaration's own name.
But the mangling pass only rewrote the *declaration* site (`a.name` inside the `AnnotationDecl`
arm) — a `#name(...)` *use* site's `AnnotationUse.name` was never touched, at either the
top-level `Decorated` wrapper or a `FieldDecl`'s own `annotations`. Symptom: any annotation used
outside the exact file that declared it — the ordinary case for a shared `#json`/`#primary_key`
library — read as `unknown annotation`, one file after its own declaration was silently renamed
out from under it. Fixed by `rewrite_annotation_uses`, called from both sites, using a direct
`subst` lookup rather than the existing `rewrite_name` helper: `rewrite_name` splits a dotted
name into `alias.member` and resolves it through the *import*-qualification table, which is wrong
for an annotation's own dotted identity (`db.model` is one flat key, not an aliased reference).
Caught by a dedicated two-file test (`an_annotation_declared_and_used_in_an_imported_module_
resolves`), not by any single-file test — worth remembering for the next feature that adds a new
top-level declaration kind: `frontend::modules`' three separate `Expression`-matching walks
(`check_no_nested_imports`, `collect_names_in`, `rewrite`) all need to learn about it, and only a
cross-module test exercises the third.

### Verification

`tests/test_hash_annotations.rs` (26 tests): struct-field defaults (used, overridden, on an optional
field, on a union variant field, wrong type, non-literal, positional-field rejection, no-default-
still-errors); annotation validation (valid use, unknown annotation, wrong field type, missing
required field, unknown field, duplicate field, both sugars, dotted names, unnamed-field
rejection, redeclaration rejection); attachment (leading, trailing, stacked leading, leading+
trailing together); and the two cross-module regression tests above. Full crate suite (46
binaries) unaffected.

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

*Half-answered by stage 8*: froglang's JSON is `serde_json` (or `simd-json`), so the two sides
already agree on everything below the annotation layer — escaping, float formatting, number
parsing. The divergence this warned about is now precisely scoped to `#json(...)`, and it is not
yet reachable, since stage 8 ships on defaults and stage 6 discards annotations rather than
storing them. Both halves of that — the storage table and exposing it here — are this stage's
work, which makes "the host reads froglang's annotations" the cheap answer rather than a
reconciliation problem.

Note also that `FrogValue` deep-copies values out **with type information only for scalars,
`Str` and `List`** — `from_bits` returns `None` for `Named`/`Union`/`Function` — so the claim
below is narrower than it reads, and a struct/union crossing the boundary needs work this stage
would have to do.

---

## Stage 8 — JSON — **done**

`json.to_str(x): Str` and `json.parse(s): T | JsonError` both ship, on a real Rust JSON
library rather than a hand-written parser, and the JSON law
(`json.parse(json.to_str(x))! == x`) is property-tested by the same generator that tests
the notation law. See `tests/test_json.rs` (31 tests) for the behavioural spec.

**The library is behind a compile-time seam** (`runtime/json/dom.rs`, ~140 lines):
`serde_json` by default — portable, and already in the workspace lockfile via
`playground-server`, so it costs no new build — and `simd-json`'s `OwnedValue` under
`--features json_simd`, SIMD-accelerated on x86_64 and aarch64. `tests/test_json.rs`
passes identically under both, which is the seam's contract: the feature is a performance
choice and never a semantic one. Owned values, not `simd-json`'s faster borrowed
`Tape`/`BorrowedValue`, because the latter borrow their strings out of the input buffer
and would make the thread-local arena self-referential for no gain the compiler could
use — every string it reads is copied onto the GC heap on the way out regardless.

**`json.` is a builtin namespace, not a module.** No `import "std/json"`, no in-memory
module machinery, and — the reason it was worth choosing — no *second* import just to
spell `JsonError` in the annotation that drives `json.parse`'s return-type dispatch.
`JsonError` is seeded in the base prelude beside `ReadError`. The namespace is recognized
by callee shape in `lower_call`, before the `FieldAccess` callee is lowered as a value,
exactly where the `Ord.compare(a, b)` trait-prefix form is recognized — and, like it, it
is *gated* on the name being free, so `let json = ...` still shadows it.

### Architecture — a second instance of stage 5, not a new mechanism

Almost every piece here has a named counterpart built and proved in stage 5:

| stage 8 | stage 5 |
|---|---|
| `runtime/json/mod.rs`'s handle arena + sticky errors | `runtime/read.rs`, whole file |
| `build_json*` (serialize) | `build_repr*` |
| `build_read_json*` (parse) | `build_read*` |
| `lower_json_parse` | `lower_read` |
| the `__json_to_str` placeholder → `desugar_notation` | `repr`'s placeholder, same pass |
| `JsonError` in the prelude | `ReadError`, same seeding |

That is the concrete cash value of this document's own "one lowering serves `repr`,
`json.to_str`, ... by swapping the fragments": `json.to_str` is a **fragment swap** on
`repr`'s existing typed-AST walk, expanded by the same `desugar_notation` pass under the
same post-monomorphization guarantees, reusing `str_cat`/`str_lit`/`build_str_join`/
`Comprehension`/`IsVariant`/`TypeTag` unchanged. No intermediate DOM is built to
serialize; the library is used on the write path only for the one string-escaping leaf.
Had `repr` been a fourth `print_value`-shaped codegen walk, this stage would have needed a
fifth.

`Int` and `Bool` reuse `__repr_int`/`__repr_bool` outright — their output is already
JSON-legal, and a second pair of leaves that merely happened to agree would just be two
things to keep in agreement. Only `Str` and `Float` get Tier-2 twins, and both for reasons
about JSON the format: the two tiers must not share an escape table, and JSON has no
spelling for `inf`/`nan`.

### The wire format

| froglang | JSON |
|---|---|
| `Int` | number (integer) |
| `Float` | number; **non-finite → `null`** — `to_str` returns `Str`, not `Str \| JsonError`, so `null` is the only total answer |
| `Bool` / `None` | `true`/`false` / `null` |
| `Str` | string, with **JSON's escapes**, never `notation::escape_str` |
| named struct | object, declared order, key = declared name |
| positional struct | array — a "tuple struct" has no field names, and froglang's internal synthetic `"0"`/`"1"` keys have no business on the wire |
| `List<T>` | array |
| `Range<T>` | `{"start":…,"end":…}` |
| nominal union | **externally tagged**: `{"Circle":{"r":1.5}}`, `{"Both":[3,7]}`, `{"Blank":{}}`. Common fields ride in the variant payload. |
| anonymous union | untagged, each member as its own JSON |

### The four semantics questions, answered

1. **`null` vs missing** — absent *or* `null` is permitted iff the field's type admits
   `None`, and yields `none`; either on a non-optional field is a `JsonError`. Serde's
   rule. This **refines what this section originally said**: `#json(required)` is not what
   *creates* the error, it will *narrow* the rule by adding one for an optional field. And
   the implementation is smaller than the sketch — a present `null` needs no special case
   at all, because the field's own `T | None` dispatch already tests `frog_json_is_null`,
   so the entire rule is one `Conditional` on `frog_json_has` around the ordinary read.
2. **Union wire shape** — **external tagging is the default**, uniformly. Internal tagging
   cannot represent a positional variant or a non-object payload at all, so making it the
   default would mean two incompatible shapes *at* the default. `#json(tag="kind")` is the
   deferred way to ask for the other one. (Resolves this doc's open question.)
3. **Anonymous unions** — parse rejects them **when two members share a JSON shape**,
   rather than always. Stricter than a naive kind check exactly where this section asked
   for strictness, and more permissive where there is no actual ambiguity: `Int | Str` and
   `Int?` parse; `Int | Float` and `Circle | Rect` (anonymous) are rejected with a message
   naming both members and the shape they collide on. `Int`/`Float` count as *one* shape
   even though the DOM can tell `1` from `1.0`, because that distinction is a spelling
   choice of the producer that no schema constrains — which is this section's own argument
   for rejecting `Int | Float`, applied precisely rather than by blanket rule. Nominal
   unions are exempt: external tagging tells them apart by name however alike their
   payloads are, which is what "make Deserialize a property of nominal unions" was
   pointing at. Serialization stays permissive.
4. **Is `Serialize` automatic?** Yes, and with **no new `Trait` variant**: `json.to_str`
   gates on `Trait::Show`, which is already structural over every `data` and already
   excludes function types — exactly the line JSON needs. A `provides`-granted `Json`
   trait would be meaningless until `without` exists, and a granted trait nobody can opt
   out of is worse than none. `data Password(hash: Str)` stays serializable and stays a
   named follow-up.

### What it took beyond the sketch

- **A backend disagreement, caught by the seam's own test.** `serde_json`'s `as_f64`
  converts an integer node; `simd-json`'s `ValueAsScalar::as_f64` returns `None` for one
  (`cast_f64` is the converting method). JSON has one number type, so `1` is the only
  spelling a whole-valued float has on the wire from any other producer, and both backends
  have to accept it. Found on the first run of `runtime::json`'s unit tests under
  `--features json_simd` — which is the entire argument for having written those before
  any codegen existed.
- **`is_positional_fields` is vacuously true for a nullary variant**, which would have put
  `[]` on the wire for `Shape.Blank()`. `{}` is the right empty payload, and keeps every
  named variant's shape an object whether or not it happens to have fields today.
- **JSON object keys are escaped at build time, not run time** — a declared field name is
  compile-time known, so escaping it in the desugar folds it into the surrounding
  `StrLit` and costs nothing at all at run time.
- **`JsonError.offset` is exact for a parse error and `0` for a shape mismatch.** A frog
  `Expression` node carries its own source `Position`, which is what lets `read` point
  `ReadError.offset` at the offending part of the input; a parsed JSON value carries no
  such thing in either backend. The message names the field and the expected type, which
  is the granularity that actually diagnoses. Fabricating an offset would be worse than
  admitting there isn't one.
- **The `!` union-into-union gap is still there** and `json.parse` hits it identically to
  `read`: `json.parse(s): Shape | JsonError` where `Shape` is itself a union parses fine
  (`build_read_json_union_as` uses stage 5's build-wide-from-the-start technique), but
  `back!` on the result does not. Pre-existing, in the `!` desugaring, and reproducible
  with `read` and no JSON in sight — routed around in the tests, not fixed here.

### Deferred, deliberately

`#json(name=…)`, `#json(skip)`, `#json(required)`, `#json(tag=…)`. Stage 6 validates a
`#json(...)` annotation and then **discards** it — nothing is stored — so honoring one
needs an annotation-storage table first, which is groundwork stage 7 (host exposure) wants
anyway. The defaults above were chosen so that every one of those annotations *narrows* a
default rather than replacing it.

### Verification

`tests/test_json.rs` (31) — every wire-format row asserted **literally**, not merely
round-tripped (a round-trip test passes just as happily against a private encoding nobody
else can read, and JSON's whole point is that somebody else reads it), plus one named test
per semantics decision; `runtime::json` and `runtime::json::dom` unit tests (14); the JSON
law folded into `tests/test_notation_law.rs`'s existing generated programs, so the same
generated value drives both tiers. All green under `FROG_GC_STRESS=1` and under
`--features json_simd`. Full crate suite (46 binaries) unaffected.

---

## Stage 8 — the original sketch

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

- ~~**Dict / Set literal syntax.**~~ **Decided and shipped, 2026-09-09**: the runner-up below,
  `["key": value]` (`[:]` for empty) — one character instead of `Dict{...}`'s five. The
  ascription collision it costs (`[x : Int]`) is handled by a parser-level colon-suppression
  flag scoped to a `[...]` literal's own element list (reset around any nested `(...)`/`{...}`
  group, so `["a": (x: Int)]` still ascribes) — see `Parser::colon_suppressed` and
  `Grammar::tuple`. Print order is insertion order, as this section required. `Set` did not
  ship (see `project_dict_map` / roadmap's "Newly done, 2026-09-09"). The analysis that led
  here, kept for the record:
  - `{...}` is unavailable: froglang is expression-oriented, so blocks appear in expression
    position and `let d = {` is genuinely ambiguous. JS escapes this only via its
    statement/expression distinction, which froglang does not have.
  - Restricting blocks to keyword-led positions (`=`, `then`, `else`, `do`, `->`, `while`) is
    *empirically* viable — all 54 `{` in the corpus already qualify — but buys less than it
    appears: **`{}` is ambiguous between an empty dict and an empty set regardless**, so a
    nominal head is needed anyway. It also makes `let x = { let a = 1; a }` fail with "unexpected
    `let` in dict literal", misdiagnosing a natural mistake, and turns every future
    block-introducing keyword (`with`, `scope`, `catch`) into a grammar edit.
  - Runner-up (shipped): `[k: v]`, one character instead of five, at the cost of `[x : Int]`
    colliding with ascription and one delimiter carrying two collection types.
  - Not chosen: **typed collection literals** — `Dict{"a": 1}`, `Set{1,2,3}`, `Dict{}` — which
    need no grammar restriction, extend to user collections, compose with generics
    (`Dict<Str,Int>{}`), and state the rule that resolves the key-vs-name confusion: **parens +
    `=` is the nominal form where the left side is a declared name (`Person(name="Alice")`);
    braces + `:` is the keyed form where the left side is an evaluated expression.** Bare
    `{k: v}` sugar stays available as a purely additive later change — still an option if `Set`
    or user-collection literals get built.
  - Rejected: `do ... end` (triple-books `do`, which is already comprehension syntax and
    `CONCURRENCY.md`'s effect-perform keyword, and costs tokens on the most common construct);
    `()` for blocks (collides with the tuples/records `DESIGN.md` wants, and over-subscribes
    parens further).
- **User-facing `reflect`.** See above; wanted eventually, blocked on TRAITS.md stages 2–3.
- **Records and tuples** (`DESIGN.md`'s `(foo=1, hello="world")`). If they land, their notation
  must be their literal too, and `DESIGN.md` line 98 already flags the grouping-paren collision.
- **`without` opt-out from structural derives.** TRAITS.md Part 3 defers it; serde's
  `data Password(hash: Str)` case is the first concrete customer.
    -> follow up note: perhaps it's just a null impl: `data Password implements Show {}`
- **Non-JSON interop formats.** The `Sink`-based lowering is format-parameterized by design, so
  CBOR / query-params slot in without new machinery.
- **A `Byte` primitive (`u8`).** Came up sketching stage 4: a byte-buffer `Sink` implementation
  wants a real byte type rather than treating everything as `Str`/`List<Int>`, and CBOR/binary
  interop formats want it more than JSON does. `Int` is froglang's one integer type today
  (`isize`-width); the general fixed-width family (`i8`/`i16`/`i32`/`u8`/... ) is out of scope —
  it's a much bigger surface (arithmetic overflow behavior per width, conversions, literal
  suffixes) for a use case this document doesn't need. `Byte` alone is narrower and has an
  independent justification: it's the element type a byte buffer, a file's raw contents, and a
  socket read all actually want, none of which is well-modeled by `Int` (8x too wide, silently
  allows out-of-range values) or `Str` (implies UTF-8, which raw bytes aren't). Revisit alongside
  stage 4 if a byte-buffer `Sink` implementation is actually built; no urgency before then.

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

- ~~**`\u{...}` in the lexer, or `repr` rejects unspellable strings?**~~ *Settled, stage 2*: the
  lexer has `\u{...}`, so no string is unspellable.
- ~~**Should unknown escapes become a parse error?**~~ *Settled, stage 2*: yes.
- **Which JSON stack is canonical** for a value crossing the embedding boundary. *Narrowed,
  stage 8*: froglang's own JSON now **is** a serde-family stack — `serde_json` by default,
  `simd-json` under `json_simd` — so a host and a froglang program no longer disagree about
  escaping, float formatting, or number parsing. What is still open is the *shape* question, and
  only for annotations: once `#json(name=…)`/`#json(tag=…)` are honored, a host that reads the
  same `Person` through `serde` derives will disagree with froglang on exactly those. Stage 7's
  declaration table is where that gets reconciled, and the answer is likely "the host reads
  froglang's annotations", not "pick a winner".
- ~~**Default union tagging** — external or internal.~~ *Settled, stage 8*: **external**
  (`{"Circle":{"r":1.5}}`), uniformly. Internal tagging cannot represent a positional variant or
  a non-object payload at all, so making it the default would mean two incompatible shapes at the
  default; `#json(tag="kind")` is the deferred way to ask for the other one.
- **Is `StrBuf` the same object as a `Sink` trait**, or does `Sink` come later as an abstraction
  over it? Deciding at design time is cheaper than retrofitting.
- ~~**Does closing `project_list_aliasing_gap` become a prerequisite** for asserting the law?~~
  *Settled, stage 5*: not a prerequisite — the gap can't build a cycle. `finish_push` (and the
  general `check_mut_exclusivity`) rejects `push(mut xs, xs)` by name against the place's own
  root; a self-referential *type* is rejected earlier still (`check_no_recursive_union`, which
  `check_reprable`/`check_readable` both call); and even a nominal self-reference can't alias at
  runtime because copy-on-write's write barrier deep-clones the shared root before the store
  lands (MUTABILITY.md Stage 7). The law is asserted with no seen-set and no depth limit
  (`tests/test_notation_law.rs`).
- ~~**`Sink`'s exemption from value semantics**~~ *Settled*: `Sink` implementers require
  `provides Linear` (`TRAITS.md` Part 7) — a checked marker trait, not a special-cased rule, and
  it generalizes to any future host-provided handle type (locks, channels, one-shot futures) with
  no `Sink`-specific machinery.

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