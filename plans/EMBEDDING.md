# Host-function embedding API

Status: **implemented** (updated 2026-09-10) — `FrogState::builder()`, `#[frog_fn]`,
`froglang_core::host::{HostFn, FromFrog, ToFrog, FrogDecl}`, and `#[derive(FrogData)]`/
`#[derive(FrogUnion)]` (`froglang_macros`). Scalars, `Str`, `List<T>`, `Dict<K,V>`,
`Option<T>`/`Result<T, E>`, and now arbitrary structs and inline (≤ 6-variant, non-recursive)
nominal unions marshal both directions. Still deferred: boxed/wider nominal unions, `mut`
parameters, generic host functions, and reading a struct/union *out* through `FrogValue` (see
"What's deferred" at the end).

## Why

froglang can be embedded (`FrogState::eval`), but the traffic is one-way: the host pushes source
in and pulls a `FrogValue` out. froglang code cannot call back into Rust. That blocks the stdlib
— file IO, string functions, math, time are all host functions — and it blocks the capability
story in `plans/CONCURRENCY.md`, where the host decides what authority a script gets.

Before this, the only functions callable by name from frog source were `print`, `panic`,
`gc_dump`, and `panic!builtin`. Adding one meant editing three hardcoded places that had to agree
on the name: `TypeChecker::default_context()`, a `builder.symbol(...)` call, and a `declare_rt`
call, all inside `Codegen::new()` — none of it reachable from `FrogState::new()`.

## Design

### The uniform shim ABI

Every host function is exposed to Cranelift with one signature, whatever its frog type:

```rust
extern "C" fn(ctx: *mut FrogCtx, args: *const i64, out: *mut i64)
```

`args` points at the caller's flattened argument leaves, one `i64` per leaf in `struct_fields`
order (`codegen/mod.rs`), with `Float`/`Bool` normalized through `to_i64_repr`. `out` points at a
caller-allocated buffer of `struct_fields(ret_ty, structs).len()` slots.

This is the pivot everything else falls out of:

- **Any frog type works** — structs, inline unions, boxed unions, `Never` — because the C
  signature never mentions one. Slot counts come from the existing `struct_fields`, which already
  covers inline unions (`RUNTIME.md` Part 1) as well as structs.
- **Symbols can be declared before any frog type is resolved.** `JITBuilder::symbol` only accepts
  new symbols at construction time, never afterward — this is what makes `Codegen::new_with_hosts`
  possible at all, and why host functions must go in *before* the type checker even exists.
- **It fits `declare_rt` as-is** — the shim signature is `(I64, I64, I64) -> ()`, no changes
  needed to that helper.
- **Rust `extern "C"` cannot return N values**, which a struct or inline-union return needs. A
  buffer sidesteps it, along with the `Bool == I8` / `Float == F64` mismatch in `cl_type`.
- **AOT-clean.** A shim is a named symbol: the JIT resolves it by address via `builder.symbol`,
  and a future `cranelift-object` backend declares the identical `Linkage::Import` and lets the
  linker resolve it. No host address is ever baked into JIT-emitted code.

Cost: an argument spill and reload per call, via a Cranelift stack slot (see "As built" — this is
the codebase's first use of one, since the shadow frame's were deleted along with the shadow
stack). Acceptable for host calls, which do real work; the all-scalar case is a listed follow-up.

### Getting the `ctx` pointer without baking an address

Codegen supplies `ctx` as argument 0 by calling a new runtime import, `frog_ctx_current() -> i64`
(`runtime/host.rs`), rather than emitting `iconst(I64, <host address>)` — which is exactly the
pattern that still makes froglang's string literals not-yet-AOT-clean (`codegen/mod.rs`'s
`TypedExprKind::StrLit` arm and `print_fragment`, both `bytes.as_ptr() as i64`).

The layering matters: the *ABI contract* has an explicit ctx parameter, which is what
`plans/CONCURRENCY.md` asks for ("route allocation through an explicit heap handle rather than a
thread-local"). A thread-local (`runtime::host::ACTIVE_CTX`) is merely how `frog_ctx_current` is
implemented today, mirroring `gc::ACTIVE_HEAP`. When the concurrency work removes the thread-local,
that one function's body changes and no host function's signature moves.

### `FrogCtx`

```rust
#[repr(C)]
pub struct FrogCtx {
    heap:    *mut GcHeap,
    structs: *const StructDefs,
    unions:  *const UnionDefs,
}
```

Carrying `structs`/`unions` is what would let struct and union marshalling work without baking
layout into each shim — a shim resolves field order and union columns at call time from the same
tables codegen uses, rather than each `#[frog_fn]` expansion needing to know layout it was never
told. (Not yet exercised by any shipped `FromFrog`/`ToFrog` impl — see "What's deferred" — but the
plumbing is in place because retrofitting it later would mean re-touching every call site.)

One `FrogCtx` lives on `FrogState::call_jit`'s stack per JIT entry, published via
`runtime::host::with_active_ctx` around the call — every host call made during that one entry
sees the same `FrogCtx`, not a fresh one per call.

### GC safety

A host shim is precisely a runtime function that takes GC pointers and can collect, so it follows
the rule `gc.rs` already states for that class of function: *if a runtime function takes a GC
pointer and can collect, it holds its pointer arguments for its whole body.* Four obligations, all
enforced by the generated shim rather than left to whoever writes the Rust function body:

1. **`jit_frame_guard!()` expanded directly in the shim body.** The collector's stack walk starts
   at the frame pointer of the function the mutator called into, and `caller_frame_pointer!`'s doc
   comment is explicit that this must happen in the function the mutator calls, not a helper it
   calls. `#[frog_fn]` expands the guard inline in the generated `extern "C"` shim for exactly this
   reason — it cannot be factored into a shared function.
2. **`RuntimeRoots::hold(args)` for the whole shim body.** From the JIT caller's point of view
   every argument died *at* the call (its stack map doesn't cover it), and the argument buffer is
   a raw Cranelift stack slot, which stack maps don't cover either. `hold` takes raw words — the
   collector filters with `is_heap_ptr` itself — so the shim hands it every argument slot
   indiscriminately, no per-slot pointer-ness reasoning required.
3. **`FrogCtx::scope()` roots everything the shim allocates**, releasing on `Drop` at the end of
   the shim's body — after the result has been written into `out`. A value the shim has built but
   not yet written is reachable from nothing until then; `gc::GcHeap::clone_obj`'s pattern (root,
   populate, keep) is the precedent.
4. **Preserve tag bits when returning a rebuilt heap value.** Not yet exercised (no shipped impl
   rebuilds a union's tagged pointer), but stated for whoever adds one: `ffi::frog_clone` re-ORs
   `w & TAG_MASK` onto a cloned pointer, and a raw `Int` leaked into a scannable column trips the
   `debug_assert!` in `GcHeap::mark_from`.

## As built

The design above is what's implemented, with a few things settled during implementation that the
original proposal left open or got wrong.

**Marshalling traits build `Type` values directly, no source text involved.** The original design
had each `HostFn` carry a `sig_src: String`, parsed at registration time by the `TypeExpr` parser.
That turned out to be unnecessary complexity: `FromFrog`/`ToFrog` (`host.rs`) each expose a
`fn frog_type() -> Type` associated function, so `i64 -> Type::Int`, `Vec<T> ->
Type::List(Box::new(T::frog_type()))`, and so on are built directly as Rust values. No
`TypeExpr`-parsing dependency, no string round-trip, and it composes for free through generic
impls (`Vec<T>`'s impl doesn't need to know `T`'s spelling, only its `frog_type()`).

**`HostFn` descriptors are functions, not `const`s.** The original sketch used
`pub const READ_FILE: HostFn`. `HostFn::params: Vec<Type>` isn't const-constructible (`Vec`
allocates; `Type::List` requires a `Box`), so `#[frog_fn]` instead generates
`pub fn read_file_host() -> HostFn`, called once when `.func(...)` is chained during
`FrogStateBuilder::build()`.

**The frame-pointer probe described in the original design isn't implemented — and can't be, the
way it was specified.** The proposal wanted `build()` to "call one trivial registered probe shim
... and confirm the walk resolves its return address in `gc::JitCode`," failing loudly if the
embedding crate lacks `-C force-frame-pointers=yes`. The problem: any probe `build()` could run
would itself be compiled inside `froglang-core`, which *does* have that rustflag set
(`.cargo/config.toml`) — it cannot observe whether the *embedder's* crate (where a real
`#[frog_fn]` shim actually lives) was compiled with it. There is no portable way to inspect another
crate's codegen flags at runtime. What actually catches a misconfigured embedder: `mark_from`'s
existing `debug_assert!` ("GC followed {:p}, which is not a heap object — a word reached the
collector under the wrong encoding") fires the first time a corrupted frame walk follows a bad
address in a debug build, and this document plus `#[frog_fn]`'s own rustdoc both state the
requirement explicitly. Listed as a real, unresolved gap below rather than papered over with a
probe that would have passed regardless of whether the flag was set.

**`FrogCtx::scope()` uses a root-count mark/release rather than a fixed-size guard.**
`RuntimeRoots::hold` (used for arguments, a known-size slice) doesn't fit a shim that allocates an
unknown number of values one at a time while building its result — `GcHeap` grew a `roots_len()`
accessor so `FrogCtx::scope()` could snapshot a mark and truncate back to it on `Drop`, the same
discipline `RuntimeRoots` uses, generalized to an incrementally-growing set.

## AOT considerations

Struct/union marshalling (above) was designed to preserve two invariants the eventual
`cranelift-object` backend (`roadmap.md`'s "path to a self-hosting compiler") depends on:

- **No runtime type tables.** `leaves()` is compositional — each impl computes it from its own
  field/variant types, never from `ctx.structs()`/`ctx.unions()` (which `FrogCtx` carries and
  `Result<T, E>`'s *old* impl used to consult). An AOT binary therefore needs no serialized
  `StructDefs`/`UnionDefs` to marshal a host type; the only thing consulted at runtime is the
  fixed, per-type leaf list baked into the derive's generated code. `FrogCtx::structs()`/
  `unions()` are consequently unused by any shipped `ToFrog`/`FromFrog` impl again — left in place
  (per the original design note) rather than removed, since retrofitting them back in later would
  mean re-touching every call site.
- **No baked addresses.** Nothing here emits an address into JIT code; a host type's identity
  crosses the boundary as a `Type` value (built from `Display`-rendered frog source in
  `frog_decl()`, or compared by `Type`'s own `PartialEq`/`Ord` at marshalling time), the same way
  every other host-function signature already does (`plans/EMBEDDING.md`'s original "AOT-clean"
  design point). The two pre-existing un-clean sites — string literals and `print_fragment`
  (`codegen/mod.rs`, both `bytes.as_ptr() as i64`) — are untouched and unrelated.
- **`build()`'s audit needs the same declarations at AOT-compile time and at link time.** The
  audit runs against whatever `TypeChecker` the prelude produced, which for the JIT is the one
  `build()` itself constructs. An AOT driver consuming the same `FrogStateBuilder` (its `prelude`/
  `data` lists are plain data, available before any compilation happens) can and should run the
  identical audit ahead of time, so a layout mismatch is a build-time error there too rather than
  a runtime one discovered only by whoever links the artifact.

## Verification

`tests/test_host_fns.rs` — scalar round trip; `Str` in and out with an explicit
`heap.force_collect()` between allocation and read-back to prove the result was rooted, not just
lucky; `List<Int>` round trip; a host call with the wrong argument type produces a clean
`FrogError::Type`, not a codegen panic; registering a reserved name (`print`) or the same name
twice is rejected at `build()`; a host function that allocates three `Str`s before writing any of
them into `out` (exercising `FrogCtx::scope`'s incremental rooting) round-trips correctly. All
pass under `FROG_GC_STRESS=1` (collect on every allocation) as well as normally — the whole
existing suite does too, unchanged, with `FrogState::new()` keeping its old signature as the guard
that nothing regressed for a caller that never touches the builder.

## What's deferred

Scope was deliberately cut down from the original design's "full type surface including structs
and unions" to what's implemented today (scalars, `Str`, `List<T>`), because the marshalling
complexity for the rest turned out to be real, not incidental:

- ~~**`Result<T, E>` ↔ `T | E`.**~~ **Done**, now on the general compositional footing below —
  both directions (`FromFrog` too, not just `ToFrog`), any leaf shape (not just single-slot
  members), and `Result<T, ()>`'s old restriction is subsumed by `Option<T>` (also both
  directions) rather than lifted on `Result` itself — a payload-less union member is a genuinely
  different case (zero leaves) from `()` as a standalone type (one leaf), and conflating them was
  the root cause, not an oversight in the old preconditions.
- ~~**Struct/union marshalling in general.**~~ **Done** (updated 2026-09-10) —
  `#[derive(FrogData)]`/`#[derive(FrogUnion)]` (`froglang_macros`) map a Rust struct/enum onto a
  frog `data`/`error` declaration by field order, plus `FrogStateBuilder::data::<T>()` to register
  one (appends `T::frog_decl()` to the prelude, or — under `#[frog(declared)]` — maps onto a
  declaration already registered some other way). `FromFrog`/`ToFrog` dropped their `const SLOTS`/
  `IS_PTR` in favor of `fn leaves() -> Vec<Type>`, composed purely from the impl tree (a struct's
  `leaves()` is the concatenation of its fields' `leaves()`; a union's is computed via
  `codegen::union_layout_of_leaves` from each variant's) — **no `StructDefs`/`UnionDefs` lookup at
  marshalling time**, which is what keeps this AOT-clean (see "AOT considerations" below).
  `build()` audits every `.data::<T>()` type's `leaves()` against the real, registered
  `codegen::struct_fields` output once, so a mismatch (a hand-written impl that forgot to override
  `leaves()` for a compound type, a field reordered on one side and not the other) is a clear
  `FrogError::Type` at `build()`, not a silent slot desync at the first call — this is exactly the
  bug `stdlib::ErrMsg`'s `leaves()` override exists to avoid (see its doc comment): its `frog_type()`
  is a compound `Named` type, not itself a leaf, so the trait's default `leaves()` (`vec![Self::
  frog_type()]`) misclassifies its one `Str` field as a non-pointer scalar. Still open: only the
  *inline* union layout (`codegen::MAX_INLINE_UNION_MEMBERS`, currently 6; `FrogUnion` asserts this
  against the real exported constant, not a hardcoded number) — a boxed/self-referential nominal
  union (frog's own recourse for a wider or recursive `data ... is ...`) has no derive support yet,
  nor does `FrogValue` gain a way to read one back out (see the next point, still open).
- **`mut` parameters on host functions.** `make_sig` already appends copy-out returns for a `mut`
  frog parameter; a host function's `out` buffer would just need the same trailing slots. Not
  wired up.
- **Generic host functions.** `plans/TRAITS.md` doesn't yet define a monomorphization contract for
  frog generics, so there's nothing for a generic host signature to instantiate against.
- **The frame-pointer probe**, per "As built" above — currently just documentation plus an
  existing debug assertion, not an active `build()`-time check.
- **Elide the argument/out spill for all-scalar host functions.** Every call goes through the
  stack-slot buffer today, even `host_add(a: i64, b: i64) -> i64`, which could in principle pass
  scalars directly in registers. Not measured; likely small relative to whatever work the host
  function actually does.
- **`FrogValue` still collapses `Struct`/`Union` to `None`** (`state.rs`) — the other half of
  "embeddable": a struct/union *host function result* marshals back into Rust fine (that's what
  the tests above exercise — a host fn destructures it or matches on it before returning a plain
  scalar), but a top-level frog expression or REPL binding whose value is a struct or union still
  can't be read out as a `FrogValue` — `build_main_body`'s final-statement path only ever returns
  its first flattened leaf, and `FrogValue::from_bits` only ever reads one `i64`. Fixing this needs
  `build_main_body`/`FrogState::eval` to carry every leaf of the final result (the way a `let`
  binding already does, `state.rs:438-452`) through to a new `FrogValue::from_slots`, plus new
  `Struct`/`Variant` variants. Not attempted here: it touches the REPL entry's core result-value
  plumbing, a different risk class from the marshalling work above, which only ever added new
  trait impls behind the existing `compile_host_call`/`FrogCtx` machinery.
- **Boxed (self-referential or > `MAX_INLINE_UNION_MEMBERS`-variant) nominal unions** on the host
  side — no `#[derive(FrogUnion)]` support (it rejects too many variants at compile time via a
  `const _: () = assert!(...)` against the real exported constant), and no `FrogCtx` helpers for
  `frog_alloc_variant`/`frog_variant_tag`/`frog_variant_get` (`runtime/ffi.rs`) analogous to
  `alloc_list`/`alloc_dict`. Matters for the self-hosting case in particular: a frog-declared AST
  union (e.g. an expression type) will routinely exceed six variants.
- **Host functions as capability grants** (`plans/CONCURRENCY.md`) — the builder is the natural
  place for a future `.grant(Fs)` once `can`/`without` exists.
