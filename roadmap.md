Consolidated 2026-09-01 from a full audit of `plans/*.md`, README's "Up Next", and this file's
own prior contents against the actual code/tests. Several previously-listed "not started" items
turned out to be shipped and undocumented; a few "done" claims turned out stale; one previously
unknown crash-on-valid-code bug was found in the process. Superseded entries below have been
removed rather than struck through — check `plans/*.md` git history if you want the trail.

### Newly found, fixed 2026-09-02

- ~~**Bug (crash): `Truthy` coercion panics on a real union type.**~~ **Fixed.** `if find() then
  ...` / `find() or 0` where `find(): Int | None` used to hit `unreachable!("Truthy on non-Truthy
  type {}", other)` in `codegen::compile_truthy` (`codegen/mod.rs:3522`) — the type checker
  correctly accepted the union (`type_implements_rec`'s "every member" rule), but codegen only
  ever learned to read a bare scalar/`List`/`None`'s own bits, never how to dispatch on a union's
  runtime tag first. Fixed by desugaring in `coerce_truthy` instead of teaching codegen a new
  node — the same pattern `?`/`!`/`catch` already use: tag-test each member right-to-left,
  narrow, and recurse `coerce_truthy` on the narrowed value to reuse its own (already-working)
  scalar rule. No codegen changes needed. `test_truthy_and_narrowing.rs` and the full suite still
  pass; spot-checked 2- and 3-member unions (`Int | Str | None`) by hand across `if`/`or`.

### Corrections to formerly-stale docs (fixed 2026-09-01)

- **README "Up Next"**: "Flow narrowing and `Truthy`" was listed as ERRORS.md phases 6-7, "not
  started" — actually shipped (`test_truthy_and_narrowing.rs`, 12 passing tests: Truthy in
  `if`/`for`/`match` guards, short-circuit coercion, nominal/anonymous-union flow narrowing,
  `Error`-trait arm exhaustiveness). The `Truthy`-on-union crash above is the one real gap left in
  that area. The exponential match-guard bug and the qualified-variant-in-type-position gap it also
  listed are both fixed this session (see commit history).
- **ERRORS.md**: header said "Status: design. Nothing here is implemented" — badly stale. Verified
  live: `data X is A | B` sugar, `match`/`is`/destructuring, `error X(...)`, `provides`, `?`
  propagation, `catch`, `!`, the tagged-pointer union representation. `Type::Enum` is gone, folded
  into `Type::Union` exactly as the doc's "what this deletes" section wanted.
- **EMBEDDING.md**: listed `Result<T, E>` ↔ `T | E` marshalling as deferred ("ABI supports it, only
  the trait impl is missing") — actually implemented (`ToFrog for Result<T, E>`, `host.rs`), and
  the stdlib already depends on it for every fallible file/string function.
- **Modules**: this file used to list "Modules" as a bare, unannotated open item — actually fully
  implemented (imports, qualified/named, cycles, diamonds, cross-module structs/traits/shadowing;
  see `froglang-core/tests/programs/modules/`).
- **User-definable generics**: also previously listed here as blocking self-hosting — done, both
  generic functions and generic structs, with real monomorphization (`plans/TRAITS.md` Stage 3,
  `test_generics_stage2.rs`/`test_generics_stage3.rs`). This *unblocks* `Dict`/`Map` below as a
  pure stdlib task.
- **`mut xs = []; print(xs)` printing `<?>` placeholders**: no longer reproducible (tried the
  original repro plus two variants) — fixed silently at some point, undocumented.

### Near-term (usability + gaps most likely to bite real code)

- **Better error messages** — span-aware, rustc-style rendered errors (`miette` or `ariadne`).
  Errors still print as raw `Debug` output. Still the single highest-leverage usability item.
- **Cheaper primitive-member union matches** — `?`/`catch`/`is`/`match` on a union whose
  non-error member is `Int`/`Float`/`Bool` still goes through the same boxed-union machinery as
  the `Str`/struct/list case (the `orders` benchmark regression this caused is documented in
  `README.md`'s Benchmarks section).
- **Operators desugaring to member calls** — `data Vec2(...) provides Num` still can't give you
  `+`; `join_operand_types`/`compile_binary` are hardcoded to the built-in numeric types.
- **Structural `Show` and `Error.message`** — `provides Show for X` errors "Unknown trait 'Show'".
- **Impls for generic types** — `data Box<A> provides Shape` errors "Unknown type 'A'" when the
  `provides` clause references the type parameter.
- **`Dict`/`Map`** — no type exists yet (`{"a": 1}` doesn't parse); purely a stdlib task now that
  generics are done, though the map-literal syntax itself is still an open bikeshed (see "Other
  Ideas" below).

### Path to a self-hosting compiler — updated gap list

- ~~User-definable generics~~ — **done**.
- ~~Modules~~ — **done**.
- `Dict`/`Map` — still missing, no longer generics-blocked (see above).
- An AOT compile path — swap the Cranelift JIT for `cranelift-object`, link against the runtime.
  Nothing built; `host.rs` has one doc-comment line describing the idea, no dependency, no `build`
  CLI subcommand (only `run`/`check`).
- Struct/union marshalling, `mut` parameters, and generic host functions in the embedding API —
  all still open (`plans/EMBEDDING.md`'s own deferred list, still accurate).

### Perf backlog (deduplicated from this file + `RUNTIME.md`)

- Unbox variants *with* payloads — flatten into tag-plus-fields slots like structs already are,
  boxing only self-referential enums (`Tree`). `orders.frog` still allocates one `FrogVariant` per
  enum value.
- Elide the COW mark when passing a list to a non-`mut` parameter — the last quadratic cliff in
  MVS (`MUTABILITY.md` Stage 7, "Sharp corners").
- Small-leaf-function inlining — ~20% on `orders.frog` from hand-inlining two call sites.
- Top-level `let` bindings aren't capturable from functions — typeck accepts the reference,
  codegen panics with "unbound variable". `life.frog`/`words.frog` both had to work around it by
  threading constants through as parameters. Needs either real capture or a clean typeck rejection
  — the codegen panic is the actual bug, not the missing feature.
- `#[frog_fn]`'s `Str` argument marshalling copies into an owned `String` per call
  (`__frog_shim_starts_with` is 14% of `benches/words.frog`); a `&str`-based ABI would remove it.
- `RUNTIME.md`'s open item: list-stride overhead is 7.5ns/elem vs. Rust's 2.6ns, unexplained —
  nobody has profiled why since the doc was written.
- **Rejected, keep rejected** (already measured, `RUNTIME.md`): eliding copy-on-write barriers
  entirely. Worth 0% on orders/life/words, ~1-10% on synthetic push/index-assign loops — not
  enough headroom to justify the analysis. Don't revisit without new measurements.

### `plans/TRAITS.md` — Stage 6/7 + Deferred (still accurate, doc self-audits well)

- Stage 6 (partial): operators-as-member-calls and structural `Show`/`Error.message` — see
  "Near-term" above.
- Stage 7 (untouched): capabilities (`provides`/`can`/`without` as implicit instances) — merges
  with `plans/CONCURRENCY.md` Stage 7.
- Deferred (by design, not urgency): associated types, blanket impls, higher-kinded parameters,
  `any Trait` boxed existentials, opt-out from structural traits, annotation-driven derives,
  trait members in `data` fields + variance, general trait inheritance (`trait A: B`).

### `plans/CONCURRENCY.md` — fully unimplemented, single largest remaining design surface

539 lines of staged design, zero corresponding code (verified: no `spawn`/`Scope`/fiber-switch/
`defer` anywhere in `froglang-core/src`). Staged plan, in order: **Unwinding** (JIT frame unwind,
`defer`/`with`, panic-as-unwind) → **Fibers** (stack-switch trampoline, pooled stacks, per-task
shadow-stack registry) → **Scheduler and I/O** (run queue, timers, kqueue, swappable `Io` value,
deterministic test `Io`) → **Scopes** (`spawn`/`scope`, lexical-nesting-only) → **Supervisors**
(`Scope` library type, `all`/`collect`/`race`/`n_of`, `Deadline`/`Shield`/`Limit`) → **Regions**
(region-bound types, contagion, whole-program retention checking) → **Capabilities** (shared with
`TRAITS.md` Stage 7). Cross-doc TODO neither doc has closed: whether `Scope`/`Task(T)` need
`Linear` (`plans/TRAITS.md` Part 7 flags it, `CONCURRENCY.md` hasn't picked it up).

### `plans/DATA.md` — data notation/serialization, stages 0/2/3 done, 1 half-done, 4-8 open

Stage 4 (`Sink`/`StrBuf`) is unblocked now — `Trait::Linear` (its prerequisite) already exists —
but still unstarted. Stages 5-8 (repr/read round-trip, field annotations, host exposure, JSON) are
fully open, design-only. Open question worth resolving before more code depends on it: whether
closing `project_list_aliasing_gap` (see `~/.claude` memory, or just: does a `List<T>` still alias
through a `mut` binding once index-assign exists) is a hard prerequisite for the repr/read
round-trip law, since `MUTABILITY.md` Stage 7/8 (COW, paths-as-places) may have already narrowed
or closed it without DATA.md's design being updated to reflect that.

### Open design questions worth resolving before more code depends on them

- **`plans/TRAITS.md`**: does `[Int]` survive as sugar for `List<Int>`? How much bound inference
  before requiring the bound be written? Do prelude operator traits admit user impls on user types
  (`data Vec2(...) provides Num`)? A display form for bounds on function types (`[~t2:Num] ->
  ~t2:Num` prints today, unreadable). "Instantiated from here" error chains for per-instantiation
  trait failures.
- **`plans/MUTABILITY.md`**: call-site marker spelling for a `mut` argument, `List` covariance,
  the large-struct flattening threshold, `mut` + UFCS composition, REPL `let`/`mut` persistence
  across entries.
- **`plans/DATA.md`**: which JSON stack is canonical for a value crossing the embedding boundary;
  default union tagging (external vs. internal); is `StrBuf` the same object as `Sink`, or does
  `Sink` come later as an abstraction over it.

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