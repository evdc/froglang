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

# Basic syntax examples

```
type RGBColor = (int, int, int)     // tuple type

type Position = {     // struct type
    x: int
    y: int
}

type Shape = {
    color: RGBColor     // Common field
    Circle {            // struct variant
        radius: int
    } |
    Rectangle {
        width: int
        height: int
    } |
    Point               // unit variant 
}

let shape1 = Shape.Circle(color=(255, 0, 0), radius=4)

print(shape1.color)     // Can access a common field directly w/o pattern matching

func area(s: Shape): int = match s {
    Circle(r) then PI * r * r
    Rectangle(w, h) then w * h
    Point then 0
}

// single-arm match, a version of rust's if-let
if shape1 is Circle(r) then print("Radius is ${r}")
// you can nicely nest these, like
if shape1 is Circle(r) and r > 50 then print("big circle!")

friends = ["Alice", "Bob", "Carol"]
greetings = ["Hello", "Hola", "Bonjour"]
// List comprehension, aka, for is an expression. They can nest sanely (main problem with Python's)
results = for f in friends; g in greetings { "${g}, ${f}!" }
// Another example. names bound by the first clause can be used in the second. also `if` is supported
results = for u in users if not u.deactivated; p in u.posts { frobnicate(p) }
```

# Error handling

- Errors as values, Result type, no exceptions (panic is still possible of course for things like OOM).
- Use a `?` operator for propagation, like Rust. no `if err != nil`. Look to Zig's design here, they do it well
- Options can be thought of as Results with `()` for their error type? If we want to keep the set of core primitives smaller
- More syntax sugar than Rust. Result should be built-in, understood by compiler, as it's so pervasive, whereas in Rust it's "just another enum" even though it is implemented in prelude. 
- `!` operator for unwrap?
- all errors are members of the type/set Error, so conversion should be easy, defining new variants extends this type
- Error types are "falsey" in a boolean context, so something like `let x = foo() or get_default()` works
    - In the case of ambiguity eg foo() returns a boolean, we should be able to do a more explicit and verbose check: `let x = if foo() is Ok(boolean_value) then boolean_value else get_default()`
    - Otherwise we want a separate keyword like "or"/"else" but checks if operand is error, rather than operand is falsey

# Effects, Capabilities, Concurrency

This section is the most work-in-progress. Examples here should be treated as drafts.

```
fn do_stuff(urls: list[str], path: str): str can fs.Write, http.Request, Suspend {
    scope {
        results = for url in urls {
            do Suspend(http.Get(url))
        }
    }
    final = combobulate(results)
    do fs.Write(final, path)
}
```

