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
- ~~**"Cheaper primitive-member union matches"** (was listed below as near-term).~~ **Done.**
  The premise was already stale when written: `plans/RUNTIME.md` Part 1's tagged-pointer
  representation had removed the boxing, so `Int | E` is an unboxed `(tag+ptr, scalar)` register
  pair and the `fallible`/`infallible` benchmark pair now run within noise of each other.
  What the audit did turn up in that area, both fixed:
  - **Bug (crash): a union with a `Float` member could not be matched at all.** Any
    `match`/`?`/`!`/`catch` on `Float | E` aborted in Cranelift's verifier (`iconst_bounds`,
    "entered unreachable code"). `TypedExprKind::Conditional`'s missing-false-branch path built
    its unreachable placeholder with `iconst`, which is integer-only, while the merge block a
    `Float`-member match joins into is `F64`. `Int`/`Bool` members were fine, which is why it
    survived — one-line fix to use the existing `placeholder_value` helper.
  - **The rightmost arm of an exhaustive `match` carried a tag test that cannot fail.**
    `fold_match_arm` folds right-to-left, so a `None` tail plus an unguarded arm means every
    other member is handled to its left; the test is now dropped. A guarded last arm keeps
    both test and guard, since the guard may have side effects. Two-member unions — the shape
    `?`/`!`/`catch` desugar to — go from two branches per dispatch to one; `orders` drops from
    496 to 463 CLIF instructions and 91 to 80 blocks. Wall-clock is unchanged (the branch
    predicted perfectly), so this is a code-size and compile-work win, not a speed one.
  - Tests: four new cases in `tests/test_union_repr.rs`, each also run under `FROG_GC_STRESS=1`.
  - Added `FROG_DUMP_CLIF`, the sibling of `FROG_DUMP_LIVENESS`/`FROG_JIT_SYMBOLS`: without a
    view of the emitted IR, a verifier panic names no froglang code at all.
- ~~**Bug (parse): an `else`-less `if` parsed only as the last expression in a file.**~~
  **Fixed.** `if cond then side_effect()` followed by any further statement failed with
  "expected an operator" on the *next* line — at top level, in a `{}` block, and as a function
  body alike. `Grammar::conditional` skipped newlines after the true branch before looking for
  `else`, eating the separator the enclosing block was about to require. Nothing was wrong with
  the semantics: the implicit `else none` (so the type is `T | None`) has always been there in
  the type checker; this was purely the newline rule. Fixed with the existing
  `Parser::peek_past_newlines_is`, which commits to the skip only when an `else` really
  follows and otherwise rolls back onto the newline — the same primitive the `data ... is`
  variant list already uses for the same ambiguity. Tests in `test_parser.rs` (structural, both
  directions) and `test_run.rs` (statement-position behaviour).
- ~~**A sweep of the rest of the newline/separator grammar.**~~ **Fixed.** Probing every
  delimiter turned up four more gaps of the same kind — "when is a newline whitespace and when
  is it a separator?" — answered with one rule: a newline is whitespace after a *required*
  delimiter (the grammar demands more input, so it cannot mean "statement over") and inside
  `( )`/`[ ]` (the closing bracket is an unambiguous terminator), and is a separator everywhere
  else.
  - **`;` did not separate *top-level* statements.** `print("a"); print("b")` was "expected
    newline, found `;`", though the identical line inside `{ }` worked — `Grammar::block_expr`
    accepted both separators, `Parser::statement` only `Newline`.
  - **Newlines were not allowed inside `( )`.** `(1\n)` and `(\n1)` failed;
    `Parser::expression_list` had long since made them insignificant inside `[ ]` and call
    argument lists, and documented why. Extended to `func`/`data` parameter and field lists,
    which had the same gap on the declaration side of the same parens.
  - **Bug: `(x, y) -> x + y` did not parse** — a form the README documents. `grouping` gave up
    at the comma; `arrow_func` had had the `Expression::Tuple` → parameters branch all along,
    so only the way in was missing. `()` is now a zero-parameter lambda's list too. The `->` is
    *required* after a parenthesised comma list — `Expression::Tuple` is also the list
    literal's node, so an arrow-less one would quietly make `(1, 2)` mean `[1, 2]`; it is an
    error naming the missing `->`, mirroring `type_atom`'s type-level wording.
  - **`->`, `do`, `in`, `catch`, `provides` did not skip a following newline**, though `=`,
    `then`, `else` and `is` did — nothing distinguished them, it was just an oversight. So
    `x ->`⏎`body`, `for i in xs do`⏎`body` and `[for i in 0..3`⏎`do i * 2]` all work now.
  - **Doc fix:** README described function types as `(T1, T2, ...) -> R`. The parser has only
    ever accepted `(T1, T2 -> R)` — parens around the whole type, arrow included — which is
    what makes `f: (Int -> Int)` unambiguous without lookahead.

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

### Newly done, 2026-09-06

- ~~**First-class functions.**~~ **Done, tier 1** — `plans/CLOSURES.md`, `tests/test_closures.rs`
  (50 tests), README's "First-class functions". A function can be passed as an argument, a
  lambda can capture enclosing `let` bindings, and a `func` can be nested. No closure object,
  function pointer, or indirect call was added: `TypeChecker::lower_function_values` hoists
  every lambda to a top-level declaration, turns each capture into a by-value parameter, and
  clones a higher-order callee per function it is passed, so codegen only ever sees direct
  calls. This also fixed two crash-on-valid-code bugs listed below — a nested `func` panicked
  with `no entry found for key`, and a function reading a top-level `let` with `unbound
  variable in codegen` — since both are the same missing feature (a free name becomes a
  parameter). Capture is by value and immutable by design; capturing a `mut` is rejected with
  the fix in the message. **Escaping** function values (returned, stored in a list/struct
  field, bound to a `mut`) remain rejected — that is tier 2, deliberately unbuilt.
- ~~**String interpolation.**~~ **Done** — `plans/INTERPOLATION.md`, `tests/test_interpolation.rs`,
  README's "String interpolation". Every `"..."` interpolates `${expr}`, holding any expression;
  a value formats exactly as `print` formats it (a `Str` raw, everything else via `repr`), so the
  three spellings of the notation can't drift. As predicted, this was cheap because `DATA.md`
  Stage 5's `repr` is a typed-AST desugar over *types*: it is a lexer change, a grammar rule, and
  one lowering, with codegen and the runtime untouched. Two things the prediction missed —
  `read` shares the parser, so **data** had to be kept non-interpolating (`Parser::parse_data`)
  or `read(repr(x)) == x` would break for any string containing `${`; and the raw-vs-quoted
  choice has to be made in `desugar_notation` rather than at lowering, because inside a generic
  body the piece's type is still an unresolved `TypeVar` (it printed `<"hi">` for `show("hi")`
  until it moved). Format specifiers and user-implementable `Show` are still open.
- ~~**`frog check` accepted programs `frog run` rejected.**~~ **Fixed** as part of the above:
  `check` ran only `check_and_lower`, never any post-lowering validation, so it printed a type
  for programs that then failed to compile. It now runs monomorphization and the function-value
  pass too.
- **Correction to this file and to README's "Up Next":** both led their usability section with
  *"Better error messages — errors still print as raw `Debug` output"*, called it the
  single highest-leverage item, and were **stale** — `plans/DATA.md` Stage 3 shipped the
  rustc-style renderer (`src/diagnostics.rs`, `render_span`), and errors have carried a
  filename, source line and caret since. The one place still printing a raw `Spanned<TypeError>`
  is `frog check`'s own two `println!`s in `main.rs`, which is a small, specific fix rather
  than a project-level priority.

### Newly done, 2026-09-09

- ~~**`Dict`/`Map`.**~~ **Done** — `["key": value]` literal syntax, `[:]` for empty
  (the bikesheded "Other Ideas" runner-up, one character instead of `Dict{...}`).
  `Dict<K, V>` is a builtin-boxed type (`FrogDict` in `runtime/gc.rs`, following `List`'s
  model, not `Range`'s), backed by a swappable `runtime::dict::DictBackend` — the default
  is `hashbrown::HashTable` — installed per-`FrogState` via `FrogStateBuilder::dict_backend`.
  A new structural `Trait::Hash`, deliberately narrow for now: `Int`/`Float`/`Bool`/`Str`
  keys only, not recursed into `List`/struct/union members, since there is no runtime
  polymorphic hashing or equality (`codegen::eq_value` is emitted per statically-known
  type) to hash a compound key with — widening this is real future work (synthesized
  per-`K` `__hash_K`/`__keyeq_K` functions), not a rule change. Full surface: `d[k]`
  (panics on a missing key, mirroring `xs[i]`), `.get(k)` → `V | KeyError`, `k in d`
  (O(1), not a value scan), `==` (structural, order-independent), `len`, truthiness,
  `d[k] = v` (insert-or-overwrite, COW-safe, nested paths work), `.keys()`/`.values()`
  (insertion order — no tombstones, so `0..len` already *is* that order), `for k in d`
  (desugars to `.keys()`), `.remove(k)` → `V | KeyError` (compacts, preserves order).
  `repr`, `read`, and `json.to_str`/`json.parse` (JSON object, `Str` keys only) all round-
  trip; `tests/test_dict.rs` (41 tests) plus extensions to `test_host_fns.rs`,
  `test_repr.rs`, `test_read.rs`. Along the way, fixed a real pre-existing gap in
  `TypeChecker::lower_expected`: it only tried `unify`ing an unresolved `TypeVar` when the
  *value's* type was the var, never when the *expected slot* was — invisible until `Dict`
  gave the checker its first "grow into an unconstrained slot via index-assignment" case
  (`mut d = [:]; d[k] = v`), since `List` has no analogous path (`xs[i] = v` requires `i`
  already in bounds). Not done: structural (non-scalar) keys, `Set`, and `Dict` in the
  "impls for generic types" gap below (`provides` on a generic builtin still errors).

### Near-term (usability + gaps most likely to bite real code)

- **`frog check`'s own diagnostics** — it prints `Spanned<TypeError>`'s `Debug` form where
  `run` renders a caret. One call site, `main.rs`.
- **A real line-joining rule** — the one parse gap deliberately left open by the sweep above.
  A newline after a binary operator (`(1 +\n2)`), or before a `.` or a `catch`, still ends the
  statement. Every other delimited context now tolerates newlines, so this is the last
  inconsistency, but it is the one that is genuinely ambiguous: fixing it properly means a
  bracket-depth counter in the lexer suppressing `Newline` inside `(`/`[` (and restoring it
  inside a nested `{`), Python-style, rather than another local `skip_newlines`. Worth doing as
  its own change, with its own tests.
- **Operators desugaring to member calls** — `data Vec2(...) provides Num` still can't give you
  `+`; `join_operand_types`/`compile_binary` are hardcoded to the built-in numeric types.
- **Structural `Show` and `Error.message`** — `provides Show for X` errors "Unknown trait 'Show'".
- **Impls for generic types** — `data Box<A> provides Shape` errors "Unknown type 'A'" when the
  `provides` clause references the type parameter.

### Path to a self-hosting compiler — updated gap list

- ~~User-definable generics~~ — **done**.
- ~~Modules~~ — **done**.
- ~~`Dict`/`Map`~~ — **done** (see "Newly done, 2026-09-09" above).
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
- ~~Top-level `let` bindings aren't capturable from functions~~ — **fixed 2026-09-06** by the
  first-class-functions work above: a free name in a function body becomes a by-value parameter,
  which is real capture rather than the clean rejection this entry offered as the alternative.
  `life.frog`/`words.frog` no longer need to thread constants through as parameters, though
  neither has been rewritten to stop doing so.
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