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
[1, 2, 3] :: List<Int>
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
- Literals: `Int`, `Float` (including `1e100`, `inf`, `nan`), `Bool` (`true`/`false`), `Str`, `none`
- Arithmetic / comparison / logical operators with correct precedence
- Unary `-` and `not`
- Let bindings with optional type annotation: `let x: Int = 5`
- Lambdas: `x -> x + 1`, `(x, y) -> x + y`
- Named function declarations: `func f(x: T, y: T): T = body`
- `return expr` — early exit from a function body
- Function calls: `f(x, y)`
- Conditionals: `if cond then a else b`. `else` is optional — an `else`-less `if` has an
  implicit `else none`, so it is typed `T | None`, and it works in statement position
  (`if cond then side_effect()`) with the next statement on the following line
- `match subject { is Pattern then expr ... }` — exhaustive on declared unions, catch-all
  required on anonymous ones; arms must be newline-separated
- `?` — propagate a fallible value's `Error` member out of the enclosing function (postfix)
- `!` — unwrap a fallible value, calling the `panic` builtin on its `Error` member (postfix)
- `expr catch fallback` / `expr catch [e] -> handler_body` — coalesce a fallible value's
  `Error` member into a fallback expression or a one-argument handler
- Block expressions: `{ stmt; stmt; expr }` — newlines and `;` both work as separators
- Lists: `[1, 2, 3]`, list comprehensions: `[for x in xs if cond do expr]`
- Ranges: `0..10`
- Type annotations: `expr : Type`
- Comments: `// ...`
- Statements are separated by a newline or `;`, at the top level and inside `{ }` alike
- Multi-line expressions. A newline is whitespace in exactly two places, and a statement
  separator everywhere else:
  - **After a required delimiter** — `=`, `then`, `else`, `->`, `do`, `in`, `is`, `catch`,
    `provides` — since the grammar demands more input, so the newline can't mean "statement
    over". Also *before* an `else` or a `do`, for the same reason (but only when one actually
    follows: otherwise that newline is the separator ending an `else`-less `if`).
  - **Inside `( )` and `[ ]`** — the closing bracket is an unambiguous terminator, so nothing
    in there could start a new statement. Covers call arguments, list literals, and `func`/
    `data` parameter and field lists.

  A newline *mid-expression* — after a binary operator, or before `.` / `catch` — still ends
  the statement. `(1 +` ⏎ `2)` is a parse error, not a continuation.

## Type system

Bidirectional type checker with unification, over a dedicated type-expression grammar
(`List<T>`, unions, optionals, function types are spellable in every annotation site).

**Primitive types:** `Int`, `Float`, `Bool`, `Str`, `None`, `Never` (the bottom type — the
type of `return`, `panic`, and any expression that never produces a value)

**Compound types:**
- `(T1, T2 -> R)` — function types, inferred for lambdas, checked against annotations. The
  parens wrap the *whole* type, arrow included: `(Int -> Int)`, `(Int, Int -> Int)`. That is
  what makes an annotation unambiguous without a lookahead — `f: Int -> Int` is rejected with
  a message telling you to add them (see `Grammar::type_annotation`)
- `List<T>` — homogeneous GC-managed lists
- Type variables with optional trait bounds
- `T1 | T2` — sum / union types
- `T?` — sugar for `T | None`
- `data Point(x: Int, y: Int)` — nominal structs, unboxed flattened-field representation
- `data Shape is Circle(r: Int) | Rectangle(w: Int, h: Int)` — sugar for a set of nominal
  structs (`Shape.Circle`, `Shape.Rectangle`) plus a closed union alias `Shape`; this is
  froglang's *only* sum-type mechanism — there is no separate enum construct

**Built-in traits:**

| Trait    | Satisfying types              | What it gives                 |
|----------|--------------------------------|-------------------------------|
| `Num`    | `Int`, `Float`                | `+` `-` `*` `/` unary `-`    |
| `Eq`     | structural: any value type     | `==` `!=`                     |
| `Ord`    | `Int`, `Float`, `Str`         | `<` `>` `<=` `>=`            |
| `Error`  | granted, not structural        | `?`, `!`, `catch`             |
| `Linear` | granted, not structural        | "can't be aliased" (checked)  |

These are ordinary entries in the same trait registry a `trait` declaration lands in, so
`provides` accepts any of them alongside your own — but their behaviour is a compiler
intrinsic, so they can be *granted*, not implemented with a body. (`Truthy`, which decides
what `if x` means for a non-`Bool`, deliberately stays compiler-internal: making it
implementable would let any impl redefine control flow.)

Bound propagation: `x -> x + x` infers as `(Num t) => t -> t`.

## Traits

See `plans/TRAITS.md`. Members are plain functions with an explicit `Self` parameter — not
methods with a receiver, which is what lets `compare(a: Self, b: Self)` and `zero(): Self`
be members at all. They live in their impl's namespace, so nothing is overloaded.

```
trait Shape {
    func area(s: Self): Int
    func doubled(s: Self): Int = s.area() * 2      // default body
}

data Circle(r: Int) provides Shape {               // inline impl
    func area(c: Circle): Int = c.r * c.r
}

provides Shape for Int {                           // standalone, for a type you don't own
    func area(n: Int): Int = n
}

Circle(r=3).area()          // dot form
Shape.area(Circle(r=3))     // prefix form, trait-qualified
Circle(r=3).doubled()       // default body, specialized per implementing type
```

**`x.f(args)` resolves in a fixed order**: a *field* `f` of `typeof(x)`, then a *member* `f`
of an impl for `typeof(x)`, then a global `func f` whose first parameter accepts `typeof(x)`.
Fields always win, so no existing program changes meaning when a trait is introduced. Each
step is a hash lookup, never a search — the ambiguities that would make it one (two impls of
one trait for one type; two traits declaring one member name for one type) are rejected at
the impl, where the error can name both, rather than at the call site.

Calling a member on a **union** is legal iff every member implements it, and compiles to the
same dispatch `match` that `?`/`catch` build. Calling a member on a **`mut` receiver** needs
no call-site marker (`c.bump()`, not `mut c.bump()`) — the marker exists to make a hidden
mutated operand visible, and the receiver is the least hidden position there is.

**Type parameters** take optional bounds: `func twice<T: Num>(x: T): T = x + x`, and
`data Box<A: Eq>(v: A)`. Bounds are still *inferred* from the body; writing them is required
once inferred (`func twice<T>(x: T): T = x + x` is rejected, naming the missing `<T: Num>`),
since a caller reads the signature and not the body.

A bound is also what lets a generic *call* the trait's members: `func report<T: Shape>(x: T):
Int = x.area()` is checked once against `Shape`'s own signature, and each instantiation
resolves `area` to that type's impl — including impls declared after the generic itself.

Unlike the structural traits above, `Error` is granted only by a `provides Error` clause on
a `data` declaration, or the `error X(...)` shorthand for `data X(...) provides Error` — it
marks "this type can flow into `?`/`!`/`catch`", not "this type happens to look a certain
way". A union satisfies `Error` iff every member does, so `provides Error` on a `data ... is
...` union grants it to every variant and the alias gets it for free.

**Union types:**
- Mismatched if-else branches produce a union: `if c then 1 else "hi"` has type `Int | Str`
- An `if` without an `else` has an implicit `else None` so its type is `T | None`
- Unions normalize (flatten, deduplicate, sort): `Str | Int | Int` → `Int | Str`
- Subtype relation: `T ≤ (T | U)`, checked at every site a type is expected (annotated
  `let`, `return`, function args/return, struct fields, `if`/`match` branch joins) — this is
  the *only* way a union value comes into being; there is no explicit union constructor
- Field access `u.f` on a union is legal iff every member has a compatibly-typed `.f`
- `is`/`match` narrow a union to one member, dispatching on a runtime tag; an exhaustive
  `match` tests every arm but the last, which needs none. Nominal and anonymous unions share
  one representation (`plans/RUNTIME.md` Part 1): a union of at most six members that isn't
  self-referential is flattened into pointer-then-scalar columns with the tag in the low 3
  bits of column 0, so it allocates nothing — `Int | E` is a two-word register pair. Wider or
  self-referential unions fall back to a boxed `FrogVariant`

**Error handling** (see `ERRORS.md`): a fallible function returns `T | E` where `E: Error` —
there is no separate `Result` type, so propagating an error into a broader error union is
free widening, not a conversion:

```
error ParseError(msg: Str)

func parse(s: Str): Int | ParseError =
  if s == "bad" then ParseError(msg="nope") else 42

func run(s: Str): Int | ParseError = parse(s)? * 2   // ? propagates ParseError out of run
func lenient(s: Str): Int = parse(s) catch 0          // catch coalesces it into a fallback
func trusted(s: Str): Int = parse(s)!                 // ! panics on the error member
```

A statement-position value whose type is or contains an `Error`-providing type must be
consumed (bound, matched, or handled) — silently discarding a fallible call is a type error.
Binding it to a `let` counts as handling (defers it to the binding's later use); a tail
position or comprehension body does too.

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
- `if-then-else` expressions, including `Never`-typed conditionals (every branch supplies
  its own terminator — a `trap` rather than a merge block — when both arms are `Never`)
- `return`, and any `Never`-returning call (a `trap` after the call, then a fresh dead block)
- `match` over structs, nominal unions, and anonymous unions
- `?`/`!`/`catch` (desugared entirely in the type checker into an ordinary `match`; no
  dedicated codegen node)
- Block expressions (sequenced statements; result is the final expression)
- String allocation, concatenation, equality, `print`
- List allocation, indexing, push, list comprehensions
- Struct construction and field access (unboxed, flattened fields)
- Union construction via implicit widening; unboxed into tagged columns (`codegen::UnionLayout`)
  for the inline case, boxed as a `FrogVariant` for a union that is self-referential or has
  more than `MAX_INLINE_UNION_MEMBERS` members

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
| `panic(msg: Str)` | `Str -> Never` | Trap the process; `!`'s error arm just calls this, so it's callable directly too |

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

**Host functions** — froglang code can call back into Rust via `FrogState::builder()` and
`#[frog_fn]` (see `plans/EMBEDDING.md`):

```rust
use froglang_core::{frog_fn, state::FrogState};

#[frog_fn]
fn shout(s: String) -> String {
    format!("{}!", s.to_uppercase())
}

let mut state = FrogState::builder().func(shout_host()).build()?;
let (value, _) = state.eval(r#"shout("hi")"#)?;
```

Parameter and return types must implement `FromFrog`/`ToFrog`
(`froglang_core::host`) — implemented for `Int`/`Float`/`Bool`/`None`/`Str`/`List<T>` today;
structs and unions are on the ABI but not yet wired up on the Rust marshalling side.

---

## Standard library

A minimal stdlib — string functions, file/stdio IO — built entirely on the host-function
embedding API above; there's no separate mechanism. `FrogState::new()` doesn't include it
(existing embedders/tests keep today's minimal surface); opt in with:

```rust
use froglang_core::state::FrogState;

let mut state = FrogState::with_stdlib()?;
```

The CLI (`cargo run`, `cargo run -- run ...`) always uses `with_stdlib()`.

**`len`** is the one exception to "everything is a host function": `len(xs)` / `xs.len()` works
on any `List<T>` or `Str` and needs no registration at all, because it's a hardcoded builtin
(alongside `push`/`print`) rather than a host function — the type system has no generics yet, so
a *host* function can't be polymorphic over `T` the way `len` needs to be.

**String functions** — `Str` is UTF-8 and byte-indexed throughout (`len` is byte length, matching
Rust's own `str::len`); there is deliberately no `s[i]` indexing operator, since mixing a
byte-based `len`/`slice` with a codepoint-based `[]` would make `s[len(s) - 1]` silently wrong on
non-ASCII input. `chars` is the character-level escape hatch, and yields codepoints, not grapheme
clusters — full Unicode segmentation is out of scope for a minimal stdlib.

| Function | Signature | Notes |
|---|---|---|
| `slice(s, start, end)` | `(Str, Int, Int) -> Str \| ErrMsg` | byte range `[start, end)`; errors instead of panicking on an out-of-range or non-char-boundary split |
| `split(s, sep)` | `(Str, Str) -> List<Str>` | |
| `join(items, sep)` | `(List<Str>, Str) -> Str` | `["a","b","c"].join(" ")`, not Python's backwards `sep.join(items)` |
| `trim(s)` | `Str -> Str` | |
| `to_upper(s)` / `to_lower(s)` | `Str -> Str` | |
| `contains(s, pat)` / `starts_with(s, pat)` / `ends_with(s, pat)` | `(Str, Str) -> Bool` | |
| `index_of(s, pat)` | `(Str, Str) -> Int \| ErrMsg` | first byte offset, or `Err` if absent |
| `chars(s)` | `Str -> List<Str>` | one codepoint per element |
| `to_int(s)` / `to_float(s)` / `to_bool(s)` | `Str -> Int\|ErrMsg` / `Str -> Float\|ErrMsg` / `Str -> Bool\|ErrMsg` | |
| `int_to_str(n)` / `float_to_str(f)` / `bool_to_str(b)` | `_ -> Str` | infallible |

Every one of these works as a method too (UFCS: `x.f(...)` resolves to any free function whose
first parameter accepts `typeof(x)`, no trait/impl machinery involved) — `"3".to_int()`,
`"  hi  ".trim()`, `["a","b"].join("-")`.

**File and stdio functions** — blocking, thin wrappers over `std::fs`/`std::io`. `plans/CONCURRENCY.md`
eventually makes I/O a swappable value in the ambient context, but there's no scheduler for it to
swap into yet, so today's contract is just "call, block, get an answer."

| Function | Signature | Notes |
|---|---|---|
| `read_file(path)` | `Str -> Str \| ErrMsg` | |
| `write_file(path, contents)` | `(Str, Str) -> Int \| ErrMsg` | truncates/creates; returns bytes written |
| `append_file(path, contents)` | `(Str, Str) -> Int \| ErrMsg` | creates if missing |
| `file_exists(path)` | `Str -> Bool` | |
| `write_stdout(s)` / `write_stderr(s)` | `Str -> Int \| ErrMsg` | raw bytes, no trailing newline and no `Debug`-style quoting — `print`'s lower-level sibling, for a TUI or a partial line built up across calls |
| `flush_stdout()` | `() -> Bool` | |
| `read_line()` | `() -> Str \| ErrMsg` | newline stripped; `Err("EOF")` at end of input |
| `read_stdin_all()` | `() -> Str \| ErrMsg` | reads to EOF, for piped input |

**Errors** are a real `T \| ErrMsg` union, not a sentinel value or a Rust-side panic — `ErrMsg`
(`error ErrMsg(msg: Str)`) is the stdlib's shared error type, usable with `catch`/`?`/`match` like
any other error type:

```
let content = read_file(path) catch [e] -> {
    print("couldn't read " + path + ": " + e.msg)
    ""
}
```

`write_file`/`append_file` return the byte count on success rather than `None` — not a style
choice: a `Result<T, E>` whose `Ok` is `()` isn't representable in today's marshalling (see
`froglang_core::host`'s `ToFrog for Result<T, E>` doc comment), so returning something real sidesteps
it rather than routing around it with a placeholder.

**Not yet built**: `Dict`/`Map` and generic `map`/`filter`/`fold` both need real generics
(`List<T>` is a compiler-special-cased "builtin hack", not user-definable yet — see `roadmap.md`);
frog already has `for`/list-comprehension syntax covering much of what `map`/`filter` would buy in
the meantime. Struct/nominal-union marshalling beyond `ErrMsg`'s one fixed shape, and host
functions that mutate their arguments, are embedding-API gaps (`plans/EMBEDDING.md`), not stdlib
gaps.

---

## Benchmarks

`benches/run_comparisons.sh [fib|orders|life|words|all]` runs each benchmark in froglang and
in Rust, Go, Python, LuaJIT and Lua, and prints wall-clock time per language.
Every implementation is a line-by-line translation of the froglang one, and all
of them must print the same result — a differing row means one of them is wrong.

- **`fib`** — naive recursive `fib(35)`. Pure call overhead and integer
  arithmetic, no allocation.
- **`orders`** — an order-pricing pipeline over 2000 `Item` structs, repeated
  2000 rounds. Each round filters the catalogue into a fresh list, classifies
  every item into a `Discount` union variant, matches on that variant, and
  runs the result through a fallible `price_item` using `?` and `catch`.
  Exercises structs, unions + `match`, error propagation, list construction,
  and the GC. The other four implementations have no equivalent of `?`/
  `catch`, but since the error branch is never actually taken on this data,
  all five implementations must still print the same total.
- **`life`** — Conway's Game of Life on a 48x48 grid for 40 generations,
  over a `List<List<Int>>`. Every inner-loop read is a double index and each
  generation rebuilds the whole grid from nested comprehensions, so this
  covers the nested-list layout `orders` never touches. Writing it is what
  turned up the copy-on-write work (MUTABILITY.md Stage 7): passing the grid
  to a function used to deep-copy it, which made this benchmark ~80x slower
  than it needed to be and hid three value-semantics bugs behind the cost of
  fixing them.
- **`words`** — builds a 400-word document from a fixed vocabulary, splits
  it back apart and folds over the words, 800 times. The only benchmark that
  allocates `Str`s, so it is the one that covers variable-size GC objects,
  byte-wise string comparison, and the `Str` stdlib (`split`/`join`/
  `to_upper`/`starts_with`) — which, being host functions, means it also
  covers the embedding boundary in a hot loop.

To profile any of them, `FROG_JIT_SYMBOLS=syms.txt` makes codegen write out
where each compiled function landed, and `benches/symbolize.py syms.txt
prof.txt` attributes a `sample`/`perf` profile's addresses back to froglang
function names — without it a profile of a froglang program is entirely
`??? (in <unknown binary>)`.

nb. the result times change often, do not record them here, they are likely to go stale.
In general froglang should be within ~2x of LuaJIT, otherwise we are doing something Wrong

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

# Dump the Cranelift IR codegen emitted, per function, to stderr
FROG_DUMP_CLIF=1 cargo run -- run program.frog
```

---

## Up Next

Roughly in priority order:

### Usability

- **Better error messages** — span-aware, rustc-style rendered errors (consider `miette`
  or `ariadne`). Currently errors print as raw debug output.

### Types and constructs

- ~~**Cheaper primitive-member union matches**~~ — **done**. The boxing half was already gone
  when this was written: `plans/RUNTIME.md` Part 1's tagged-pointer representation flattens
  every non-recursive union of at most `codegen::MAX_INLINE_UNION_MEMBERS` members into
  `UnionLayout`'s columns, so `Int | E` is a `(tag+ptr, scalar)` register pair and `?`/`catch`
  allocate nothing at all (`tests/test_union_repr.rs`, and the `fallible`/`infallible` benchmark
  pair, which now run within noise of each other). Two real things were left, both fixed:
  a `match`/`?`/`!`/`catch` on a union with a **`Float` member** aborted in Cranelift's verifier
  (the unreachable branch's placeholder was built with the integer-only `iconst`, and the merge
  block is `F64`); and an exhaustive `match` emitted a **tag test on its rightmost arm**, which
  cannot fail — dropping it takes a two-member union's dispatch from two branches to one, and
  `orders` from 496 to 463 CLIF instructions across 80 blocks instead of 91. Wall-clock is
  unchanged: the redundant branch predicted perfectly. Still open here is the six-member
  inline ceiling and the boxed self-referential case, both deliberate (`plans/RUNTIME.md`,
  "As built").
- ~~**Flow narrowing and `Truthy`**~~ — **done**: narrowing a union after a bindless `is` arm,
  the `Truthy` trait for condition-position coercion (`if`/`for`/`match` guards, `and`/`or`/`not`
  short-circuit, including on a union whose members are all `Truthy` — desugared to a per-member
  tag dispatch in `coerce_truthy`), and `Error`-trait match arms with exhaustiveness all ship and
  are tested (`test_truthy_and_narrowing.rs`). Error return traces are still unbuilt.
  `lower_match`'s guard-clause cloning, previously flagged here as exponential, is fixed.
- ~~**User-defined traits and impls**~~ — **done, see "Traits" above** (`plans/TRAITS.md`
  Stage 5), including calling a member through a bound (`func f<T: Shape>(x: T) = x.area()`).
  Still missing from the trait system: operators desugaring to member calls (so
  `data Vec2 provides Num` can't give you `+`), structural `Show` and therefore
  `Error.message`, and impls for generic types (`data Box<A> provides Shape`).
- ~~**Qualified variant names in type position**~~ — **done**: `ParseError.UnexpectedEof` is
  spellable in a `TypeExpr` annotation and resolvable as an `is`/`match` pattern nested inside a
  further anonymous union.
- ~~**Modules**~~ — **done, undocumented above**: `import "./path.frog" { name }` (named) and
  `import "./path.frog" as alias` (qualified) both work, including import cycles, diamond
  imports, and cross-module structs/traits — see `froglang-core/tests/programs/modules/`.
- ~~**User-definable generics**~~ — **done**: both generic functions and generic structs
  (`data Pair<A, B>(...)`) work with real monomorphization (`plans/TRAITS.md` Stage 3). This was
  previously the top blocker for `Dict`/`Map` and for self-hosting; see "Embedding" below.

### Embedding

- ~~`FrogState::builder()`~~ — **done, see `plans/EMBEDDING.md`**: host Rust functions are
  registered before the JIT module is created (`JITBuilder::symbol` only accepts new symbols
  at construction), then installed into the type checker's scope, so froglang code can call
  back into the host with full type-checker support. Covers scalars/`Str`/`List<T>` today;
  struct/union marshalling and generic host functions are listed as follow-ups there.
- ~~Minimal stdlib~~ — **done for strings/file IO, see "Standard library" above**:
  `FrogState::with_stdlib()`, `Result<T, E>` ↔ `T | E` marshalling (`ToFrog for Result<T, E>`
  in `froglang_core::host`), and the shared `ErrMsg` error type. Still needed for a real
  self-hosting compiler: `Dict`/`Map` and generic `map`/`filter`/`fold` — no longer blocked on
  generics (user-definable generics are done), just unbuilt — and nominal struct/union
  marshalling beyond `ErrMsg`'s one fixed shape (`plans/EMBEDDING.md`'s follow-ups).
