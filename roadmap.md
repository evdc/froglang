- Modules
- Better error messages / pretty-printing rustc-style using span info
- Enums / variants, common fields, matching expressions (e.g. via `if x is Circle(r) then ...`) — done: `data X is A | B(...)`, `match`, `is`, boxed+tagged GC representation, exhaustiveness checking
  - follow-ups: structural `==` on enums, named/nested pattern binds, multi-line leading-`|` variant lists
  - done: payload-less variants are unboxed to an immediate tag (`(tag << 1) | 1`, low bit distinguishes them from 8-aligned pointers — see gc.rs "Immediate (unboxed) values")
- Perf: copy-on-write for lists — **done**, MUTABILITY.md Stage 7. A `List` binding is marked
  `shared` where a second live path to it is created and copied only at a write, instead of being
  deep-copied at every aliasing read. `benches/life.frog` 2054ms -> 27ms; `benches/words.frog`
  108ms -> 97ms; `orders` unchanged (it never paid the cost). It also closed three value-semantics
  bugs eager copying could not afford to fix — a list extracted from a list, from a `for`-loop
  binding, or from a struct field was aliased, not copied, so pushing to it mutated the container.
  `FROG_COW_VERIFY=1` re-derives the sharing answer from the heap at every write barrier.
- Perf: **elide the mark when passing a list to a non-`mut` parameter.** The last quadratic cliff
  in MVS: `for i in .. { s = s + peek(xs); push(mut xs, i) }` is 51.6ms against an 11.5ms baseline
  at n=20000, because the call marks `xs` shared and the push then copies. Needs two things
  together — marking at `mut` binding sites (`Assign.mutable`), and a per-function "does a
  parameter reach a return position" escape summary. See MUTABILITY.md Stage 7, "Sharp corners".
- **Done (MUTABILITY.md Stage 8): paths as mutable places.** Assignment, `push`, and every `mut` call
  argument now take the same `typed_ast::Place` (root plus a `.field`/`[index]` path), resolved by
  one shared `lower_place`. So `push(mut b.items, v)`, `rows[y][x] = v`, `push(mut rows[0], v)`,
  mixed paths like `g.cells[1][1] = v`, **and `f(mut b.items)` for a user-defined `func`** all work.
  `codegen::emit_place_ref` walks a path unsharing every list on the way down and writing each
  private copy back into its parent slot — O(depth), not O(size).
  A `mut` argument is `Arg::Mut(Place)`, so `Call` shed its `mut_args: Vec<bool>` and a new mutating
  builtin (`pop`, `insert`, a `Map` operation) costs one arm in `compile_call` and no AST changes.
  Fixed a pre-existing soundness hole on the way: copying a struct did not mark its list fields
  shared, so `let c = b; b.items[0] = 99` was visible through `c`.
- Language: a user-defined mutating **method**'s receiver must still be a bare binding —
  `b.items.bump()` where `bump` takes `mut self` is rejected, though `b.items.push(3)` works.
  `lower_bound_member_call` and `lower_ufcs_call` build the receiver's `Place` from its root name
  because the raw receiver expression is consumed before they know the member is mutating; threading
  it through (as the `push` branch already does) would close this.
- Perf: `benches/life.frog` predates Stage 8 and still rebuilds whole grids from comprehensions
  because it could not write one cell. Rewriting it around `rows[y][x] = v` would make it a
  materially different (and more representative) benchmark against the Go/Rust/Lua/Python siblings.
- Perf, **measured and rejected**: eliding redundant copy-on-write barriers. Swiftlet's own
  postmortem (Racordon et al., JOT 2022 §7.5) blames most of its gap to Swift on unnecessary
  uniqueness checks, but that finding does not transfer: Swiftlet checks a *reference count*, so it
  also pays increments on every copy and decrements in every destructor, whereas our barrier is one
  load of a monotone byte off a cache line the mutation is about to touch anyway. Deleting the
  barrier outright — unsound, so an upper bound on any elision — is worth 0% on orders/life/words,
  ~1% on a pure `push` loop and ~10% on a pure index-assign loop (40M writes each). Removing the
  division in `frog_list_get`/`frog_list_set` recovers ~4% of that index-assign case and nothing
  elsewhere. Don't build the analysis; the headroom isn't there. If the indexed-write path is ever
  worth attention it's the per-leaf FFI call itself, not the barrier or the arithmetic inside it.
- Bug: `mut xs = []` then `print(xs)` prints `<?>` placeholders — the element type is still an
  unresolved type variable on that `Var` node when print's type-directed codegen reads it.
  Annotating works around it. Needs a final resolution pass over the typed AST.
- Perf: **inlining.** With calls out of line, `benches/orders.frog` spends ~30% in one-line functions (`modn` 20%, `checked_gross` 8%) that Rust/Go erase. Hand-inlining two call sites takes orders from 54.8ms to 45.1ms, so a small-leaf-function inliner is worth roughly 20% there.
- Perf: functions can't reference top-level `let` bindings — typeck accepts it, codegen panics with `unbound variable in codegen: <name>`. Both `life.frog` and `words.frog` had to thread constants through as parameters to work around it. Either implement the capture or reject it in typeck with a real error.
- Perf: unbox variants *with* payloads — flatten them into tag-plus-fields slots the way structs already are, boxing only self-referential enums (`Tree`). `benches/orders.frog` allocates one `FrogVariant` per enum value. Secondary: the `#[frog_fn]` boundary converts every `Str` argument into an owned Rust `String` (`__frog_shim_starts_with` is 14% of `benches/words.frog`) — taking `&str` would remove a copy per call.
- Error handling, errors as values + early-return sugar, etc — builds on enums (Result/Option as compiler-known enums)
- Traits/interfaces, generics, and methods — designed in `plans/TRAITS.md`; **Stages 0-3 and 5 shipped**
    - The `provides`/impl duality is resolved: one `provides` keyword, inline on `data` or standalone (`provides T for Int { ... }`), with a body
    - Members live in the impl's namespace, not the global one — so there is no function overloading anywhere
    - `x.f(y)` resolves field → member → free function; all three steps work, each a hash lookup
    - Bounded generics call trait members (`func f<T: Shape>(x: T) = x.area()`), resolved per instantiation during monomorphization — so a generic stdlib written against traits is now unblocked
    - Still missing: operators desugaring to member calls (so no user `provides Num`), structural `Show`/`Error.message`, and impls for generic types
- Annotations, and auto-deriving trait implementations (macros/comptime?)
- Structured concurrency

What it takes to get from here to self-hosting compiler
- User-definable generics (List(T) is a builtin hack)
- a Dict/Map type (which could be stdlib but I would give it blessed `{key=val}` syntax as it's ubiquitous)
- ~~Traits/interfaces generalized beyond Error, user definable~~ — done (`plans/TRAITS.md` Stage 5)
- stdlib with file IO, string functions, etc.
- an AOT compile path, swapping Cranelift JIT for cranelift-object, linking against the runtime etc.

---

Other Ideas
- tagged numeric literals, eg `14hr`, `33mi`, `12.34s`
    - lexer recognizes them, interpretation is up to libraries/user
    - perhaps it just desugars to creating a struct like `Tagged(tag='hr', val=14)`
    - built-in unit math / dimensional analysis is probably out of scope here
- similarly, native date/time syntax?

---

Methods — **decided**, see `plans/TRAITS.md` Part 1

- UFCS: `x.f(args)` resolves in a fixed order — a field of `x`, then a member of an impl for
  `typeof(x)`, then a free `func f` whose first parameter accepts `typeof(x)`
- `List.push(xs, x)` is rejected, as this note wanted. `xs.push(x)` and `s1.concat(s2)` work
- No overload on the first argument type: members are namespaced by their impl, so nothing
  collides. The OOP-ish and Go-style sketches below were both declined — a receiver cannot
  express `compare(a: Self, b: Self)` or `zero(): Self`, which are the two members that matter
  most for a language whose error story is trait-keyed
- The prefix form for a member is trait-qualified: `Ord.compare(a, b)`
- `mut` is not written on a dot-call receiver: `xs.push(x)`, not `mut xs.push(x)`. See the
  amendment at the end of `plans/MUTABILITY.md` §3

Declined sketches, kept for the record:

---

sketching JSON/etc serde

```
data Person(
    name: Str
    age: Int
    last_name: Str  #json:lastName
    email: Str?     // optional
)

let txt = '{"name": "Alice", "lastName": "Jones", "age": 42}'
// needs a generic argument in expression position here
let alice = json.parse[Person](txt)

let alice = json.parse(txt) as Person       // another way?
```

Intersects with reflection/meta-programming. You end up needing *some* facility for it, eventually
- Runtime reflection: more constrained, pays cost at runtime but you're likely not doing reflect in hot loops anyway
- Compile time (macros, comptime, etc): better runtime perf, can do more, costs compile time (which froglang wants to keep fast also), more footguns / surface area to do Weird Things

compile time: 
- `comptime` or `static` keyword or `$` sigil like `comptime if os == OSX`, `static if os == OSX`, `$if os == OSX` etc
- quote/unquote syntax, e.g. ` for quoting expressions and $ for unquote/splice/interpolate
- `macro` keyword for functions that run at compile time (after parse, before codegen), taking an AST and producing an AST

eg
```
macro adder(n: Int) {
    return `x -> x + $n`
}

let inc = adder(1)
// At macro expansion time, the AST is replaced with the equivalent of
// let inc = x -> x + 1

inc(2)  // 3
```

runtime:
- `reflect` or `meta` module that can be imported
```
import reflect

// example from above
data Person(
    name: Str
    age: Int
    last_name: Str  #json:lastName
    email: Str?     // optional
)

reflect.fields(Person)  // :: List[reflect.FieldInfo]
reflect.fields(Person)[2].annotations   // reflect.FieldAnnotation("json:lastName")

// psuedo
func to_json(d: T): Str = {
    let fields = []
    for f in reflect.fields(T) do {
        let field_name = if f.annotations[0].startswith("json:") then 
            f.annotations[0].split(":")[1]
        else f.name
        let field_val = reflect.get_field(d, field_name)        // ???
        fields.push('"${field_name}": ${field_val}')
    }
    // ...
}
```

how does Go work with struct field annotations?

---

QoL things
- Skip field/arg names when the variable has the same name as the field/arg:
```
data Person(name: Str, age: Int)
let name = "alice"
let age = 42
let alice = Person(name=name, age=age)      // long form
let alice2 = Person(name, age)  // short form

func foo(name: Str, age: Int) = ...
foo(name, age)      // works here too
```

- lambdas having `[param1, param2] -> e` syntax is an old holdover (at one time, `[]` was going to be tuple syntax)
    - should be `(param1, param2) -> e`
    - maybe `=>` actually to match JS, free up `->` for other uses


1. `{key=val}` for maps, `{thing1, thing2}` for sets (will want them eventually), and a different block syntax
    1a. `let result = do ... end` for blocks
    1b. `()` for blocks: `(expr)` is already grouping, which is a sort of degenerate case of blocks (a group with 1 element)
    1c. In unambiguous positions, e.g. `func foo(arg): Ret { ... }` we can still use curly braces (e.g. JS manages to do this)
2. `[key=val]` for maps, paralleling `[a, b, c]` for lists; a map is like a list with named instead of positional "arguments"
    2a. Then we don't have a dedicated literal for sets and have to do like `Set(a, b, c)` or something but maybe that's fine
    2b. This is a bit unusual syntax but does parallel struct constructors and fn calls, and has precedent in Lua

a fully consistent, unambiguous syntax would be like
- `{1, 2, 3}` for anonymous tuples, `{a=1, b=2}` for named tuples / anonymous structs, `Person{name="Alice", age=42}` for nominal structs
- `(x; y)` for blocks and grouping both (unambiguous with calls, `(` in prefix vs infix position)
- `[1, 2, 3]` for lists and `["apple"=1, "banana"=2]` (expression keys) for maps

... but I don't exactly like `Person{name="Alice", age=42}` for some reason (Python familiarity, perhaps)

---

Improvements to Ranges
- Range as a builtin type (Std prelude)
  - `data Range<T>(T, T)`
  - member functions for contains, iter, ...
  - `x .. y` syntax simply desugars to `Range(x, y)` (instead of materializing a List)
- That implies user-definable traits for 
  - Iterable (provides `next()`)
  - Container (provides `has()`, `len()`)
  - Builtin `for ... in` syntax uses Iterable, `x in y` operator uses Container
  - We still want the common case to be as performant as possible
- Range types (limited: only ints)
  - `let x: 0..256` for example
  - Subtyping: R <: S iff R.start >= S.start and R.end <= S.end
  - Probably don't attempt to widen via arithmetic, e.g. `func f(a: 0..3, b: 0..4) = a * b`, the return type isn't inferred `0..12`
    - Just widens to Int?
    - What languages do feature this, and what does e.g. Ada (one that I remember has range types) do?
  - Do not attempt to add generalized refinement types as a first step