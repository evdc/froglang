# froglang

A compiled, statically typed language designed for LLM-assisted coding — terse enough
to keep the context window lean, explicit enough that generated code is trustworthy.

See `DESIGN.md` for the full language vision.

---

## Current state

The frontend pipeline (lexer → parser → type checker) is working and interactive via a REPL.
No backend / code generation exists yet.

### What's implemented

#### Lexer (`lexer.rs`)
Tokenises the full expression syntax: literals, identifiers, operators, keywords (`if`, `then`,
`else`, `not`, `and`, `or`, `true`, `false`), and punctuation. Recovers from errors and collects
multiple lex errors before aborting.

#### Parser (`parser.rs`)
Pratt (top-down operator precedence) parser producing a `Spanned<Expression>` AST.

Supported expression forms:
- Literals: integers, floats, strings, booleans
- Arithmetic / comparison / logical binary operators, with correct precedence
- Unary `-` and `not`
- Variable assignment with optional type annotation: `x: Int = 5`
- Lambda / anonymous functions: `x -> x + 1`, `(x, y) -> x + y`
- Function calls: `f(x, y)`
- Conditionals: `if cond then a else b` (else optional)
- Blocks: `{ stmt; stmt; expr }`
- Lists (tuple syntax): `[1, 2, 3]`
- Type-annotated expressions: `expr : Type`

#### Type checker (`typeck.rs`)
Bidirectional type checker with Robinson unification and the following type system:

**Primitive types:** `Int`, `Float`, `Bool`, `Str`, `None`

**Compound types:**
- `Function { params, result }` — inferred for lambdas, checked against annotations
- `List(T)` — homogeneous lists
- `TypeVar { name, bounds }` — type variables with optional trait bounds (see below)
- `Union(Vec<Type>)` — sum / union types (see below)

**Trait-bounded polymorphism (Phase B complete):**

Operators are polymorphic via explicit trait bounds on type variables, Rust-style:

| Trait | Implements     | Operators          |
|-------|----------------|--------------------|
| `Num` | `Int`, `Float` | `+` `-` `*` `/` unary `-` |
| `Eq`  | `Int`, `Float`, `Bool`, `Str` | `==` `!=` |
| `Ord` | `Int`, `Float`, `Str` | `<` `>` `<=` `>=` |

```
1 + 2       :: Int        ✓  (Int implements Num)
1.0 + 2.0   :: Float      ✓  (Float implements Num)
1 + 2.0     → type error  ✗  (can't unify Int and Float)
"a" + "b"   → type error  ✗  (Str does not implement Num)
"a" == "b"  :: Bool       ✓  (Str implements Eq)
"a" < "b"   :: Bool       ✓  (Str implements Ord)
true < false → type error ✗  (Bool does not implement Ord)
```

Bound propagation: `x -> x + x` correctly infers as `(Num t) => t -> t` — the lambda
parameter inherits the `Num` bound from the operator.

**Union types (Phase A complete):**

`Type::Union(Vec<Type>)` is the representation for sum types.

- If-expressions with mismatched branch types produce a union rather than an error:
  `if cond then 1 else "hello"  →  Int | Str`
- If-expressions without an else branch return `T | None`
- `Type::normalize()` canonicalises unions: flattens nesting, deduplicates, sorts
  variants alphabetically. `Int | Str | Int` → `Int | Str`, `Str | Int` → `Int | Str`.
- Subtype relation `A ≤ B`: `T ≤ T | U`, `T | U ≤ V` iff each variant ≤ V.
  `check()` uses `is_subtype` so values are accepted wherever a wider union is expected.

#### REPL (`main.rs`)
`cargo run` starts an interactive session that parses input, prints the AST, and infers
the type:

```
>> 1 + 2
...
:: Int

>> x -> x + x
...
:: ~t1:Num -> ~t1:Num

>> if true then 1 else "hello"
...
:: Int | Str
```

---

## What's next

Roughly in priority order:

### Near-term

- **Named function declarations** — currently only anonymous lambdas exist. Add `func f(x: Int): Int = x + 1` syntax, parsed and type-checked at the top level.
- **Typed AST** — integrate parsing and type checking to produce a typed IR as a prerequisite for any backend work. Right now the AST is untyped and type checking is a separate pass.
- **CLI check tool** — `frog check "1 + 2"` (or reading from a file) so snippets can be tested non-interactively. Useful for scripting and for Claude to use as a tool.
- **Better error messages** — span-aware, rustc-style rendered errors (consider `miette` or `ariadne`).

### Type system

- **User-defined traits and impls (Phase D)** — `trait Add { fn add(self, other: Self) -> Self }`, `impl Add for MyType`, and explicit `T: Trait` bounds in function signatures. No let-generalisation — generic parameters must be declared explicitly.
- **Pattern matching / narrowing** — `match` expressions that narrow union types at each arm, the primary way to consume `Union` values.
- **Named struct and enum types** — `type Shape = Circle { r: Int } | Rect { w: Int, h: Int }`, with field access and exhaustive match.
- **Intersection function types (Phase C, optional)** — represent built-in operators as `(Int,Int)->Int & (Float,Float)->Float` instead of trait bounds. Formally cleaner but adds a subtype-based dispatch algorithm; the user-visible behaviour is identical to Phase B. Worth considering if set-theoretic types become a first-class design goal.
- **Result / Option built-ins** — `Result(T, E)` and `Option(T)` as sugar over union types, plus `?` propagation syntax.

### Backend (stretch)

- **Cranelift codegen MVP** — compile the core expression language (arithmetic, variables, conditionals, function calls) to native via Cranelift. No GC or runtime yet; let memory leak. `main()` return value printed to stdout.
- **Runtime and GC** — minimal GC sufficient for strings and heap-allocated structs/lists.

---

## Running

```sh
cargo run          # REPL
cargo test         # all tests (lexer, parser, type checker)
```
