# String interpolation

`"n is ${n}"`. Shipped 2026-09-06.

## What existed before

Almost all of it. `plans/DATA.md` Stage 5 had already built `repr` — a total, structural
to-string for every `Show` type, as a **typed-AST desugar** (`build_repr`) expanded by
`desugar_notation` after monomorphization. `Str + Str` concatenation already existed and
already lowered to `frog_str_concat`. So interpolation needed no new formatting machinery,
no runtime support, and no codegen arm: it is a lexer change, a grammar rule, and one
lowering that folds pieces together with the fragments `repr` already produces.

That is the whole reason this was cheap, and it is worth stating the other way round: the
reason `repr` was built as a desugar over *types* rather than a runtime `to_string` over
values is that a second consumer would eventually want the same expansion. This is that
consumer.

## Foundational choices

| Decision | Chosen | Why |
|---|---|---|
| Which literals interpolate | **all `"..."`** | One string form to learn. The alternative — a marked `f"..."` — costs a second form to spare `repr` one escape. |
| Sigil | `${...}` | Already the spelling in `roadmap.md` and `DATA.md`. `{...}` alone would collide with data and JSON-ish text far more often. |
| What may appear inside | **any expression** | The fragment is handed to the ordinary expression parser, so there is no second grammar to keep in sync, and nesting works by construction. |
| How a value is formatted | `print`'s rule | A `Str` raw, everything else `repr`. Anything else would give one value two spellings. |
| Data (`read`) | **does not interpolate** | Data is not code. |

### Why all `"..."` interpolating is affordable

The cost of the choice is exactly one thing: `repr`'s output must still read back as source,
so `escape_str` now escapes `${` — and only `${`, so a price or a shell-ish string is
unmarked. The lexer gained `\$` to match, and `notation.rs`'s existing round-trip test (it
lexes what `escape_str` emits) covers it, since that module's whole discipline is that the
two halves are changed together.

The subtler cost is the one that had to be looked for rather than reasoned about: **`read`
parses with the same parser**. Without care, adding interpolation would have turned data
containing `${` into a parse error — a silent regression in a feature (`read(repr(x)) == x`)
whose entire point is fidelity. `Lexer::for_data` / `Parser::parse_data` split the two
readings, and one test pins it.

## How it works

1. **Lexer** (`read_string`) splits a literal into `StrPart`s: literal runs, and `${...}`
   pieces kept as *unparsed source* plus the `Position` they start at. A literal with no
   `${` produces one `Lit`, which `next_token` collapses back to a plain `Token::String` —
   so nothing downstream sees a new token shape unless interpolation was written.
2. **Grammar** (`interp_string`) parses each piece by re-entering the parser on its
   substring (`Parser::fragment`), and builds `Expression::Interp(Vec<Expression>)` with
   the literal runs as ordinary string literals.
3. **Typeck** (`lower_interp`) wraps each `${}` piece in a `Notation::Interp` placeholder
   and folds the pieces with `Str + Str`.
4. **`desugar_notation`** expands each placeholder per resolved type: `Str` inserts the
   value itself, everything else expands `build_repr`.

### Two things that had to be got right

**Spans have to point inside the string.** Keeping the fragment as source with its start
`Position` — rather than as pre-lexed tokens, or as a span covering the whole literal —
is what makes `"value: ${nope}"` put its caret under `nope`. `Lexer::new_at` exists for
exactly this.

**The raw-vs-quoted choice cannot be made during lowering.** The obvious implementation
checks the piece's type in `lower_interp` and inserts a `Str` directly. That is wrong inside
a generic function, where the type is still an unresolved `TypeVar` at that point: one body
serves every instantiation, so `show("hi")` printed `<"hi">` while `show(1)` printed `<1>`.
Caught by a test, not by review. The fix is that `Interp` is a third *notation* rather than
a decision — the choice happens in `desugar_notation`, per monomorphized instantiation,
which is precisely the ordering guarantee `repr` already documents and relies on.

A nested string literal inside a `${}` is copied **verbatim** by the brace scanner rather
than scanned for braces, so `"${ id("}") }"` counts the right ones — and nesting then works
for free, because the fragment parser re-lexes that substring and meets the same rule one
level down.

## Cost

Codegen: untouched. Runtime: untouched. A literal with no `${` lexes to exactly what it
lexed to before.

Concatenation is a left fold, so an n-piece literal allocates n-1 intermediate strings. That
is what the surface syntax would have cost anyway, and it keeps codegen out of it; an n-ary
`frog_str_concat_n` is the obvious fix if interpolation ever lands in a hot loop, and it is
a runtime change only — the desugar would not move.

## Not done

- **`Show` as a user-implementable trait** (`TRAITS.md` Stage 6). Interpolation formats via
  `repr`, which is compiler-intrinsic and structural; a type cannot yet choose its own
  display. Interpolation makes this want doing far more often than `repr` alone did.
- **Format specifiers** (`${x:.2f}`, padding, alignment). Deliberately absent: the sigil has
  room for them, and adding them later breaks nothing, whereas guessing at a mini-language
  now would.
- **Multi-line literals / a raw string form.** A `"..."` may already span lines, but the
  lexer does not track line breaks inside one, so spans after a multi-line literal drift.
  Pre-existing, and unchanged by this.
