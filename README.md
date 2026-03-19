# froglang

🐸 A cute little compiled, statically typed language designed for LLM-assisted coding.
inspirations include "Go but nicer to use".

See `DESIGN.md` for more scattered thoughts on language vision.

---

## Syntax (so far)

```
// Named function declarations with typed parameters and return type
func fib(n: Int): Int =
  if n <= 1 then n
  else fib(n - 1) + fib(n - 2)

// Multi-statement block bodies — newlines and ; both separate statements
func hypotenuse(a: Int, b: Int): Int = {
  let a2 = a * a
  let b2 = b * b
  a2 + b2
}

// Inline block with semicolons
func clamp_pos(x: Int): Int = { let z = 0; if x < 0 then z else x }

// Multi-line if-then-else
func ack(m: Int, n: Int): Int =
  if m == 0 then n + 1
  else if n == 0 then ack(m - 1, 1)
  else ack(m - 1, ack(m, n - 1))

// Top-level let bindings
let x = 42
let f: (Int -> Int) = n -> n * 2
```

**Supported expression forms:**
- Literals: `Int`, `Float`, `Bool` (`true`/`false`), `Str`
- Arithmetic / comparison / logical operators with correct precedence
- Unary `-` and `not`
- Let bindings with optional type annotation: `let x: Int = 5`
- Lambdas: `x -> x + 1`, `(x, y) -> x + y`
- Named function declarations: `func f(x: T, y: T): T = body`
- Function calls: `f(x, y)`
- Conditionals: `if cond then a else b` (`else` optional)
- Block expressions: `{ stmt; stmt; expr }` — newlines and `;` both work as separators
- Tuples/lists: `[1, 2, 3]`
- Type annotations: `expr : Type`
- Comments: `// ...`
- Multi-line expressions: newlines are skipped after `=`, `then`, and `else`

## Type system

your basic bidirectional type checker with unification. no HKTs, we're not Haskell.

**Primitive types:** `Int`, `Float`, `Bool`, `Str`, `None`

**Compound types:**
- `(T1, T2, ...) -> R` function types, inferred for lambdas, checked against annotations
- `List(T)` — homogeneous lists
- Type variables with optional trait bounds
- `T1 | T2` — sum / union types

**Trait-bounded polymorphism:**

A few built-in traits or bounds exist to demonstrate polymorphism works
we don't have the ability for user-defined traits yet.

| Trait | Satisfying types              | Operators                   |
|-------|-------------------------------|-----------------------------|
| `Num` | `Int`, `Float`                | `+` `-` `*` `/` unary `-`  |
| `Eq`  | `Int`, `Float`, `Bool`, `Str` | `==` `!=`                   |
| `Ord` | `Int`, `Float`, `Str`         | `<` `>` `<=` `>=`          |

Bound propagation: `x -> x + x` infers as `(Num t) => t -> t`.

**Union types:**
- Mismatched if-else branches produce a union: `if c then 1 else "hi"` has type `Int | Str`
- An `if` without an `else` has an implicit `else None` and so has type `T | None`
  - which will have a convenient alias `T?` when we get around to that
- Unions normalize (flatten, deduplicate, sort): `Str | Int | Int` → `Int | Str`
- Subtype relation: `T ≤ (T | U)`; values are accepted wherever a wider union is expected

## Cranelift JIT backend

Compiles to native code via Cranelift. Supported:
- All arithmetic, comparison, and logical operators on `Int` and `Bool`
- Let bindings and variable references
- Named and anonymous functions, including mutual recursion
- Function calls (direct and via value)
- `if-then-else` expressions
- Block expressions (sequenced statements; result is the final expression)
- Two-pass compilation: forward-declares all functions so mutual recursion works

## Test suite

12 end-to-end programs in `tests/programs/`, exercising:
Fibonacci, factorial, sum-to-N, power, GCD, Collatz, Ackermann, multi-function programs,
let bindings, block expressions, and multi-line syntax.

---

## Running

```sh
cargo run -- run <file.frog>   # compile and run a program
cargo test                      # all tests (lexer, parser, type checker, codegen)
```

---

## Up Next

Roughly in priority order:

### Usability

- **Better error messages** — span-aware, rustc-style rendered errors (consider `miette`
  or `ariadne`). Currently errors print as raw debug output.

### Types and constructs

- **Named struct and enum types** — `type Shape = Circle { r: Int } | Rect { w: Int, h: Int }`,
  with field access and exhaustive `match`.
- **User-defined traits and impls** — `trait Foo { ... }`, `impl Foo for MyType`, and
  explicit `T: Trait` bounds in function signatures.
- **`Result` / `Option` built-ins** — sugar over union types, plus `?` propagation syntax.
- **`match` expressions** — the primary way to consume `Union` types and destructure
  enums. Needed before user-facing union types are fully usable.

### Runtime

- **GC and heap allocation** — minimal GC sufficient for strings and heap-allocated
  structs and lists.
- **Standard library stubs** — `print`, basic string operations, list operations.
