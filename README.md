# froglang

🐸 A compiled, statically-typed, expression-oriented language with a Cranelift JIT backend
and a mark-sweep GC. Designed to be embeddable in Rust applications, fast compilation, decent runtime performance.
Inspirations include Go (but better usability) and Lua.

```
🐸 froglang repl
>> func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)
:: Int -> Int
>> fib(30)
832040 :: Int
>> let greeting = "hello " + "world"
"hello world" :: Str
>> [1, 2, 3]
[1, 2, 3] :: List(Int)
```

---

## Syntax

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
- Lists: `[1, 2, 3]`
- Type annotations: `expr : Type`
- Comments: `// ...`
- Multi-line expressions: newlines are skipped after `=`, `then`, and `else`

## Type system

Bidirectional type checker with unification.

**Primitive types:** `Int`, `Float`, `Bool`, `Str`, `None`

**Compound types:**
- `(T1, T2, ...) -> R` — function types, inferred for lambdas, checked against annotations
- `List(T)` — homogeneous GC-managed lists
- Type variables with optional trait bounds
- `T1 | T2` — sum / union types

**Trait-bounded polymorphism:**

| Trait | Satisfying types              | Operators                   |
|-------|-------------------------------|-----------------------------|
| `Num` | `Int`, `Float`                | `+` `-` `*` `/` unary `-`  |
| `Eq`  | `Int`, `Float`, `Bool`, `Str` | `==` `!=`                   |
| `Ord` | `Int`, `Float`, `Str`         | `<` `>` `<=` `>=`          |

Bound propagation: `x -> x + x` infers as `(Num t) => t -> t`.

**Union types:**
- Mismatched if-else branches produce a union: `if c then 1 else "hi"` has type `Int | Str`
- An `if` without an `else` has an implicit `else None` so its type is `T | None`
- Unions normalize (flatten, deduplicate, sort): `Str | Int | Int` → `Int | Str`
- Subtype relation: `T ≤ (T | U)`

## Cranelift JIT backend

Compiles to native machine code via Cranelift. Two-pass compilation: all named functions
are forward-declared before any body is emitted, so mutual recursion works without
forward declarations in user code.

Supported in codegen:
- All arithmetic, comparison, and logical operators on `Int`, `Float`, and `Bool`
- `Int` → `Float` widening coercion at use sites
- Let bindings and variable references
- Named and anonymous functions, including mutual recursion
- Function calls (direct and via value)
- `if-then-else` expressions
- Block expressions (sequenced statements; result is the final expression)
- String allocation, concatenation, equality, `print`
- List allocation, indexing, push

## Runtime / GC

Heap-allocated values (`Str`, `List`) are managed by a **mark-sweep garbage collector**.
Each object has a `GcHeader` in an intrusive linked list. The GC triggers automatically
after each JIT call when `bytes_allocated > threshold` (initial: 1 MB; grows with the
live set).

- `FrogStr` — immutable, inline bytes after the header; NUL-terminated
- `FrogList` — header + separate `i64` data buffer; grows via `realloc`

FFI functions (`frog_alloc_str`, `frog_list_push`, etc.) are `extern "C"` symbols
resolved by the JIT linker. They always allocate into the currently-active `FrogState`'s
heap, set via an `ACTIVE_HEAP` thread-local pointer during JIT execution.

**Builtins callable from froglang:**

| Name | Type | Description |
|---|---|---|
| `print(value)` | `Any -> None` | Coerce a value to text and print it with a newline |
| `gc_dump()` | `() -> None` | Dump GC heap contents to stderr |

## REPL

The REPL maintains full state across entries — variables and functions defined in earlier
entries remain in scope.

```
🐸 froglang repl
>> let x = 10
10 :: Int
>> func double(n: Int): Int = n * 2
:: Int -> Int
>> double(x)
20 :: Int
```

**REPL meta-commands:**

```
:gc    — dump live GC heap objects to stderr
:help  — show available commands
```

## Embedding API

`FrogState` owns all interpreter state and is the main embedding entry point:

```rust
use froglang_core::state::{FrogState, FrogValue};

let mut state = FrogState::new();

// State persists across eval calls
state.eval("let x = 6 * 7")?;

let (value, ty) = state.eval("x")?;
assert!(matches!(value, FrogValue::Int(42)));
```

`FrogValue` deep-copies strings and lists out of the GC heap on return, so the host's
lifetime is independent of the GC. Multiple `FrogState` instances on different threads
need no coordination — each owns its heap and JIT module (Lua model, no GIL).

---

## Benchmarks

`benches/run_comparisons.sh [fib|orders|all]` runs each benchmark in froglang and
in Rust, Go, Python, LuaJIT and Lua, and prints wall-clock time per language.
Every implementation is a line-by-line translation of the froglang one, and all
of them must print the same result — a differing row means one of them is wrong.

- **`fib`** — naive recursive `fib(35)`. Pure call overhead and integer
  arithmetic, no allocation.
- **`orders`** — an order-pricing pipeline over 2000 `Item` structs, repeated
  2000 rounds. Each round filters the catalogue into a fresh list, classifies
  every item into a `Discount` enum variant, and matches on that variant to
  price it. Exercises structs, enums + `match`, list construction, and the GC.

Apple M-series, release build; times are the whole process, so froglang's
include JIT compilation:

| | fib(35) | orders |
|---|---|---|
| Rust -O3        | 50ms   | 31ms  |
| Go (gc)         | 52ms   | 38ms  |
| LuaJIT          | 71ms   | 56ms  |
| **froglang**    | 64ms   | 182ms |
| Lua             | 670ms  | 579ms |
| Python 3        | 2359ms | 1381ms |

On straight-line arithmetic froglang sits between Go and LuaJIT, as expected
from a Cranelift backend. `orders` is where the runtime shows: it started at
427ms, and two rounds of profiling took it to 182ms.

- **Inline heap access.** Reading a list element, reading or writing a variant
  payload slot, and appending to a list with spare capacity were each an
  out-of-line call to an FFI symbol that did a single load or store. Emitting
  the memory operation directly in Cranelift IR instead (see "Inline
  heap-object access" in codegen/mod.rs) took 427ms → 242ms.
- **Unboxed shadow frames.** Every JIT call that touches the heap registers a
  GC shadow frame; each one was a `Box`, so a `malloc`/`free` pair per call.
  Keeping them in a `Vec` took 242ms → 182ms.

What's left is allocation: one `FrogVariant` per enum value, so this program
mallocs ~4M times, and `malloc`/`free`/`memset` plus the mark phase are now
about half its profile. Payload-less variants are already unboxed into
immediates; unboxing variants *with* payloads — flattening them into
tag-plus-fields slots the way structs already are, and boxing only what is
recursive — is the next real win.

---

## Running

```sh
# Interactive REPL
cargo run

# Run an expression or .frog file
cargo run -- run '1 + 2 * 3'
cargo run -- run program.frog

# Type-check only
cargo run -- check program.frog

# Run all tests
cargo test

# Verbose GC tracing (allocations, marks, frees to stderr)
cargo test --features gc_trace
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
  enums.

### Embedding

- **`FrogState::builder()`** — register host Rust functions before the JIT module is
  created, so froglang code can call back into the host with full type-checker support.
