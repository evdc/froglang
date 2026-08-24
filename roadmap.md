- Modules
- Better error messages / pretty-printing rustc-style using span info
- Enums / variants, common fields, matching expressions (e.g. via `if x is Circle(r) then ...`) — done: `data X is A | B(...)`, `match`, `is`, boxed+tagged GC representation, exhaustiveness checking
  - follow-ups: structural `==` on enums, named/nested pattern binds, multi-line leading-`|` variant lists
  - done: payload-less variants are unboxed to an immediate tag (`(tag << 1) | 1`, low bit distinguishes them from 8-aligned pointers — see gc.rs "Immediate (unboxed) values")
- Perf: unbox variants *with* payloads — flatten them into tag-plus-fields slots the way structs already are, boxing only self-referential enums (`Tree`). `benches/orders.frog` allocates one `FrogVariant` per enum value (~4M mallocs); after inlining heap access and un-boxing shadow frames it is at 182ms vs 31ms for the same program in Rust, and `malloc`/`free`/`memset` plus the mark phase are ~half of what's left. Secondary: pool/free-list the fixed-size GC blocks, and make shadow frames cheaper than an FFI push/pop + zeroing per call.
- Error handling, errors as values + early-return sugar, etc — builds on enums (Result/Option as compiler-known enums)
- Traits/interfaces, generics, and methods — designed in `plans/TRAITS.md`, decisions settled
    - The `provides`/impl duality is resolved: one `provides` keyword, inline on `data` or standalone, with a body
    - Members live in the impl's namespace, not the global one — so there is no function overloading anywhere
    - `x.f(y)` resolves field → member → free function; UFCS over free functions ships first, on its own
- Annotations, and auto-deriving trait implementations (macros/comptime?)
- Structured concurrency

What it takes to get from here to self-hosting compiler
- User-definable generics (List(T) is a builtin hack)
- a Dict/Map type (which could be stdlib but I would give it blessed `{key=val}` syntax as it's ubiquitous)
- Traits/interfaces generalized beyond Error, user definable
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
