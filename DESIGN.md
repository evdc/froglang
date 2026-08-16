# frog-lang

A language designed for LLM-assisted coding, while still being palatable to humans.
Design goals and decisions:
- Compiled, statically typed. Aim for fast compilation for quick iteration. Use Cranelift for backend
- Uses a runtime with a GC (like Go). Aim for "good enough" runtime performance, not maxxing
- Slightly higher-level / more convenience / QoL features than Go; "say what you mean" without verbosity obscuring intent and polluting the context window
- Imperative and pragmatic, not pure-functional and theoretical; but still features local type inference (annotations required at function boundaries)
- Verification features (a limited form of range/refinement types, pre/post-conditions on functions checked at runtime) to ensure trustworthiness of generated code. (Integrating anything approaching an SMT-solver is explicitly a non-goal)

# Basics

- primitive types: int, float, bool
    - support bit width eg i64 later, for now default `int` and `float` to `isize` and `f64`
- strings (guaranteed utf8, like Rust). iterate over bytes vs unicode scalar values vs graphemes.
- the usual arithmetic & comparison ops with precedence
- conditionals, control flow (`if` is an expression)
- lists and maps built-in, structs and enums
- list comprehension (`for` is an expression?)
- generics, and (explicit) interfaces/traits, but no HKTs, no inheritance
- Cranelift for fast compilation with reasonable runtime performance

# Data Structures

- Lists `let nums = [1, 2, 3]` (homogenous type, inferred; mutable)
    - Support range indexing `nums[1..3]`. 0-based index.
    - Support indexing from end, like Python `nums[-1]`
    - Support reverse indexing if the end is before the start? `nums[-1..0]` = `[3, 2, 1]`? Or is that too ambiguous
    - Bonus: support multi-indexing and "swizzling": `nums[2, 0]` == `[3, 1]`
    - Bonus: support predicate indexing as a shorthand for filter: `nums[$ > 1]` == `[2, 3]`
- Maps `let m = {"apple"=100, "banana"=200, "strawberry"=300}` (mutable)
    - Type inferred as much as possible. Heterogenous values become a union type
- Tuples `let items = (1, "hello", True)` (immutable, heterogenous; type here is `Tuple[Int, Str, Bool]`)
- Records `let r = (foo=1, hello="world")` - immutable, value types
    - Like tuples, with named fields
    - Anonymous type is inferred as `(foo: Int, hello: Str)`
- Named records aka structs `data Person(name: Str, age: Int); let alice = Person(name="Alice", age=42)`
- Enums
    - `data Color = Red | Green | Blue` plain kind
    - `data Shape = Circle(r: Int) | Rectangle(w: Int, h: Int)` data variants 
- Objects: mutable structs?
    - `mutable data Person = {name: Str, age: Int}`
    - `let alice = Person {name="Alice", age=42}`
    - `alice.age += 1`

Enum variants with common fields:
```
data Shape(position: (Int, Int), color: Color) = 
    Circle(r: Int) 
    | Rectangle(w: Int, h: Int)

let shape = Circle(position=(3, 2), color=Red, r=4)
shape.color     // No need to pattern match here, this field is common to all variants
shape.r         // error, need to match on it

func area(s: Shape): Int = when s (
    is Circle(r) then r * r
    is Rectangle(w, h) then w * h
)
```

Alternate syntax for structs/enums, under consideration:
```
data Shape {
    color: Color
    position: (Int, Int)
    case Circle {
        r: Int
    }
    case Rectangle {
        w: Int
        h: Int
    }
}

// here, a `data` def. without any `case` in it, is simply a struct

// This is a bit less pleasant; e.g. `data Color = Red | Green | Blue` could desugar to this?
data Color {
    case Red
    case Green
    case Blue
}
```

For working with immutable records/structs, some conveniences
```
let alice = (name="Alice", age=42)
let bob = (...alice, name="Bob")        // make a record with fields from another, override some
let ageless = alice -- age             
```

Some questions
- Is this too many kinds of built-in data structures? Some languages like Lua try to unify in a small conceptual set
    - But that's a dynamic scripting lang, not statically typed
    - Rust just implements data structures eg Vec, HashMap as library code - but we do want to privilege some common built-ins syntactically, for ergonomics
- syntax for all of these may vary
    - are we overloading `()` too much -- grouping for precedence, function calls, and also tuples/records?
    - `[]` could be lists but also maps a la `["apple"=2, ...]` ?
    - I do like the separation of mutable/immutable, eg tuples/records/structs are one syntax, and lists/maps/objects are another
    - `<>` is available too: `<1, 2, 3>` -- although unusual

## Annotations

we want to support lightweight annotations on structs and fields to be used for e.g. serde and DB libraries
inspired by Go's annotations 

```
// The content of these just gets lexed as raw strings
#db_model
#my-annotation:foo="bar"      
data Person(
    id: Int     #primary-key
    name: Str   #not-null #unique   // Can still put a comment here
)
```

Lexing rules might be something like:
- Anything from `#` to the next whitespace becomes part of the tag, as a raw string
- Although if there's a `"`, then whitespace inside quotes gets ignored?

some DB library could then interpret these and maybe do some sort of reflection/metaprogramming.

nb. I don't want to do a bunch of `#[derive(...)]` annotations like Rust, that's too heavyweight for our intended ergonomics
`data` records, aka immutable/value types, should have auto derived implementations for Display, Eq, Ord, Serialize, DBTable, etc
eg ideally:
```
#db:model       // maybe you have to opt in to this one at least
data Person(
    id: Int
    name: Str
    age: Int
    email: Str?
)

// imagine using -1 id as a sentinel or something
alice = Person(-1, "Alice", 42, None)
bob = Person(-1, "Bob", 34, None)
alice < bob     // true: default impl is lexicographic comparison of fields in order
alice == bob    // false
alice == Person(-1, "Alice", 42, None)  // true: structural equality, not reference equality
print(alice)
json.to_str(alice) == '{"id": -1, "name": "Alice", "age": 42, "email": null}' 
json.from_str(json.to_str(alice)) == alice      // true

// here we'd be able to call generated methods to get table name, construct an INSERT statement, etc
db.connect("test.sqlite").insert(alice)
```

# Functions

- Top level named functions `func add(x: Int, y: Int): Int = x + y`
- Anonymous lambdas `let add = [x, y] -> x + y`
    - Future: Implicit-arg lambdas e.g. `nums.map($ + 1)`, `let add = ($0 + $1)`
- Keyword/default arguments
    - Positional args must come before kwargs
    - kwargs must have a default value
    - positional args (those w/o default) may only be passed positionally; kwargs may only be passed by name. Avoid the confusing situation in Python with `/` and `*` separators etc.
- Function body is one expression, which may be a block. `return` kw can still be used for early return.
    - Bonus: `tailcall` kw to make tail-return explicit to compiler

# Error handling, Pattern Matching

- Errors as values, Result type, no exceptions (panic is still possible of course for things like OOM).
- Use a `?` operator for propagation, like Rust. no `if err != nil`. Look to Zig's design here, they do it well
- There is a built in type/set Error which the compiler knows how to handle specially, so `Result<T, E>` is actually just `T | E where E: Error`
- `Option<T>` is `T | None`, or equivalent to `Result<T, None>`.
- More syntax sugar than Rust. Result should be built-in, understood by compiler, as it's so pervasive, whereas in Rust it's "just another enum" even though it is implemented in prelude. E.g. `T?` as shorthand for option.
- `?` operator for propagation, `!` operator for unwrap?

- Error types are "falsey" in a boolean context, so something like `let x = foo() else get_default()` works
    - In the case of ambiguity eg foo() returns a boolean, we should be able to do a more explicit and verbose check: `let x = if foo() is Ok(boolean_value) then boolean_value else get_default()`
    - Otherwise we want a separate keyword like "or"/"else" but checks if operand is error, rather than operand is falsey

- `else` as a short-circuiting infix boolean op unifies if-expressions & error handling
    - But also special syntax within else blocks? e.g.:
```
get_user(id) else {
    // pattern matching syntax, but the subject of the match is implicit (it's the falsey value)
    if Error(e) then log("DB error", e)
    if None   then log("Not found")
}
```

Guards with `and`:
```
value match {
    if Int(i) and i < 0 then "negative"
    if Int(i)            then "non-negative"
}
```

nb. above we used `when value { is Pattern1(binding) then ...; is Pattern2(binding) then ... }` for matching instead
Decide on a single consistent syntax here

Error conversion should be more implicit than Rust:
```
type CompileError : Error
// possibly in a different module:
type ParseError : CompileError

func parse(source: Str): AST ? ParseError = ...
func typecheck(ast: AST): TypedAST ? TypeError = ...
func codegen(ast: TypedAST): Code? CodegenError = ...

// Implicitly ? can convert e.g. ParseError -> CompileError
func compile(source: Str): Code ? CompileError = {
    let ast = parse(source)?
    let checked = typecheck(ast)?
    return codegen(checked)?
}
```

if we have `type CompileError = ParseError | TypeError | CodegenError` (closed nominal sum typing), then the rule is:
- we can automatically convert from ParseError to CompileError by wrapping: `CompileError::ParseError(parse_error)`
- the type declaration above is basically expanded to something like the following Rust
```rust
enum CompileError {
    ParseError(ParseError),
    TypeError(TypeError),
    CodegenError(CodegenError)
}
```
i.e. variants are implicitly tagged with their name.

if we have `type ParseError : CompileError` (open subtyping), then the conversion is just an upcast, basically, right

In general perhaps the type system supports both, e.g.:

```
// closed — exhaustive match guaranteed
type Shape(color: Color) = Circle(r: Int) | Rectangle(w: Int, h: Int)

// open — exhaustive match not possible, must have catch-all
type Shape = (color: Color)
type Circle : Shape = (r: Int)
type Rectangle : Shape = (w: Int, h: Int)
```

Based on syntax for common fields in enums (in Data Structures, above), in both cases, Circle and Rectangle have a .color,
and the .color field is accessible on any Shape.

# Effects, Capabilities, Concurrency

This section is the most work-in-progress. Examples here should be treated as drafts.

```
func do_stuff(urls: list[str], path: str): str can fs.Write, http.Request, Suspend {
    scope {
        results = for url in urls {
            do Suspend(http.Get(url))
        }
    }
    final = combobulate(results)
    do fs.Write(final, path)
}
```

or
```
func do_stuff(urls: list[str], path: str): str {
    with Scope(errors=#collect) {
        results = for url in urls {
            spawn http.Get(url)
        }
    }
    final = combobulate(results)
    do fs.Write(final, path)
}
```

or even
```
func do_stuff(urls: list[str], path: str): str = {
    let results = with Scope(errors=#collect) {
        for url in urls {
            spawn http.Get(url)
        }
    }
    final = combobulate(results)
    do fs.Write(final, path)
}
```

all tasks spawned within a scope exit by the time the scope exits
scope-level policies for gathering results/errors (join all, exit on first settled, exit on first error, collect all errors, yield as completed, ...)

---

