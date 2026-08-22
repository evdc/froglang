- Modules
- Better error messages / pretty-printing rustc-style using span info
- Enums / variants, common fields, matching expressions (e.g. via `if x is Circle(r) then ...`) — done: `data X is A | B(...)`, `match`, `is`, boxed+tagged GC representation, exhaustiveness checking
  - follow-ups: structural `==` on enums, named/nested pattern binds, multi-line leading-`|` variant lists
  - done: payload-less variants are unboxed to an immediate tag (`(tag << 1) | 1`, low bit distinguishes them from 8-aligned pointers — see gc.rs "Immediate (unboxed) values")
- ~~Perf: unbox variants *with* payloads~~ — **done, see RUNTIME.md Part 1**: a union that isn't self-referential and has at most 6 members is flattened into tagged-pointer + scalar columns the way structs already are, boxing only the self-referential ones (`Tree`) and the very wide ones. `benches/orders.frog` went 90ms → 60ms. Still open, and now unblocked by it: Cranelift stack maps (RUNTIME.md Part 2), which retire the shadow stack — `project_gc_shadow_stack_perf` measured shadow-frame maintenance at ~30% of `orders.frog`.
- Error handling, errors as values + early-return sugar, etc — builds on enums (Result/Option as compiler-known enums)
- Traits/interfaces, explicit-style
    - Resolve duality between `provides` and trait impls. Explicit `impl Trait for Type` `Type implements Trait {}` blocks?
- Annotations, and auto-deriving trait implementations (macros/comptime?)
- Structured concurrency

What it takes to get from here to self-hosting compiler
- User-definable generics (List(T) is a builtin hack)
- a Dict/Map type (which could be stdlib but I would give it blessed `{key=val}` syntax as it's ubiquitous)
- Traits/interfaces generalized beyond Error, user definable
- stdlib with file IO, string functions, etc.
- an AOT compile path, swapping Cranelift JIT for cranelift-object, linking against the runtime etc.

---

We need to improve performance of unions/errors. In the `orders.frog` benchmark, as one review agent put it:
> Widen (codegen/mod.rs:1492) always boxes a value crossing into a union type via box_into_variant — a real frog_alloc_variant malloc plus GC-header init — even for a
  plain Int. There's no unboxed fast path for scalar union members the way nullary members (None, payload-less variants) already get. In orders.frog, this fires twice per item:
  once widening checked_gross's Int into Int | PricingError, again widening price_item's own Int result. ?'s desugared match then does a tag load + field unbox to get the Int right
  back out — a full box-then-unbox round trip on a path that never actually holds an error, ~4M extra allocations on top of the Discount boxing the README already calls out.

Notes
- The union-representation question below is answered in RUNTIME.md, and implemented: neither
  two-slot nor always-boxed, but a tagged pointer carrying the member tag in the low 3 bits of
  the first pointer column (or in a dedicated column when that column is itself tagged).
  Measurements and the encoding rules are there.
- We switched from a two-slot (tag, payload) repr to a one-slot (always-boxed) repr - why?
    - We found the "global type id" isn't required anywhere else
    - Perhaps to save a slot?
- But now we pay the cost of boxing for every enum, even ones that just have primitive members. E.g. a `data Foo is Bar(Int) | Baz(Str)` has to box the Int, whereas it could fit in a (tag, payload) pair. 