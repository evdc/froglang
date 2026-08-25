# Host-function embedding API

Status: **implemented** (2026-08-24) — `FrogState::builder()`, `#[frog_fn]`,
`froglang_core::host::{HostFn, FromFrog, ToFrog}`. Scalars, `Str`, and `List(T)` marshal today;
structs/unions/`Result<T, E>` are deferred (see "What's deferred" at the end) even though the
codegen ABI itself already handles them uniformly.

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

## Verification

`tests/test_host_fns.rs` — scalar round trip; `Str` in and out with an explicit
`heap.force_collect()` between allocation and read-back to prove the result was rooted, not just
lucky; `List(Int)` round trip; a host call with the wrong argument type produces a clean
`FrogError::Type`, not a codegen panic; registering a reserved name (`print`) or the same name
twice is rejected at `build()`; a host function that allocates three `Str`s before writing any of
them into `out` (exercising `FrogCtx::scope`'s incremental rooting) round-trips correctly. All
pass under `FROG_GC_STRESS=1` (collect on every allocation) as well as normally — the whole
existing suite does too, unchanged, with `FrogState::new()` keeping its old signature as the guard
that nothing regressed for a caller that never touches the builder.

## What's deferred

Scope was deliberately cut down from the original design's "full type surface including structs
and unions" to what's implemented today (scalars, `Str`, `List(T)`), because the marshalling
complexity for the rest turned out to be real, not incidental:

- **`Result<T, E>` ↔ `T | E`.** Packing a Rust `Result` into an inline union's tagged-pointer
  columns (`UnionLayout`, `codegen/mod.rs`) needs the *frog* union type's member ordering and tag
  assignment, which depends on `T`/`E`'s normalized position relative to each other — not
  knowable from `T`/`E`'s Rust types alone without also threading `FrogCtx::unions()` through
  `ToFrog::to_frog`, and reasoning correctly about `union_is_inline` vs. the boxed fallback. The
  ABI already supports it (a union return is just N `out` slots); only the trait impl is missing.
- **Struct/union marshalling in general**, i.e. `#[derive(FrogData)]`/`#[derive(FrogUnion)]`
  mapping a Rust struct/enum onto a registered frog `data` type by field order. The raw ABI
  handles any type uniformly today (`struct_fields` already flattens both); what's missing is
  purely the Rust-side derive macro. Until it exists, a struct/union-shaped host signature has no
  ergonomic way to be declared — the escape hatch is writing a `HostFn` and its shim by hand,
  which is possible (nothing in `compile_host_call` assumes `#[frog_fn]` produced it) but
  undocumented tedium.
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
- **`FrogValue` still collapses `Struct`/`Union` to `None`** (`state.rs`) — unrelated to this work,
  but the other half of "embeddable": those types can't cross *outward* to the host either.
- **Host functions as capability grants** (`plans/CONCURRENCY.md`) — the builder is the natural
  place for a future `.grant(Fs)` once `can`/`without` exists.
