# AOT compilation

Status: **not started** (2026-09-12). `roadmap.md`: "swap the Cranelift JIT for `cranelift-object`,
link against the runtime. Nothing built." This plan is the how, and the list of gaps that must
close first.

## Goal

`frog build prog.frog -o prog` produces a standalone native executable that runs the program
with no compiler, no Cranelift, and no per-run codegen. `frog run` (JIT) stays the default; `build`
is a second consumer of the same `Codegen`.

The runtime is *already* largely AOT-shaped, which is what makes this tractable rather than a
rewrite:

- Every `frog_*` runtime entry point is `#[no_mangle] extern "C"` (`ffi.rs`, `dict.rs`,
  `runtime/host.rs`, `read.rs`, `json/mod.rs`) — a linker can resolve them by name.
- Runtime calls in codegen are already `Linkage::Import` (`declare_rt`), never a baked address.
  The JIT resolves them with `JITBuilder::symbol`; the object backend emits the identical import
  relocation and lets the linker resolve it. This was a deliberate design point (`EMBEDDING.md`,
  "AOT-clean").
- Host-function shims use the same uniform `(ctx, args, out)` imported-symbol ABI (`EMBEDDING.md`).
- Marshalling needs no serialized `StructDefs`/`UnionDefs` — `leaves()` is compositional
  (`EMBEDDING.md`, "AOT considerations").

So the work is concentrated in three places: (1) two codegen sites that bake host pointers into
code, (2) abstracting `Codegen` over the `Module` trait so an `ObjectModule` can drive it, and
(3) a generated program entry point + a load-time stack-map registration story. Everything else is
plumbing.

---

## Architecture today (what `build` must reuse)

- `Codegen` wraps a concrete `JITModule` (`codegen/mod.rs:33`). `new_with_hosts` builds a
  `JITBuilder`, registers ~60 runtime symbol *addresses* via `builder.symbol(name, addr)`, declares
  each as a `Linkage::Import` signature (`declare_rt`), and sets ISA flags: `is_pic=false`,
  `preserve_frame_pointers=true`, `opt_level=none`.
- `compile_entry` is three passes: (1) declare top-level funcs `Linkage::Local`, mangled per
  `entry_id`; (2) define bodies; (3) build `__frog_main_N(out_ptr: i64) -> i64`. Then
  `finalize_definitions()`, and register each function's stack maps into the process-wide
  `gc::JIT_CODE` keyed by the *finalized absolute address*.
- `__frog_main` writes each top-level binding into `out_ptr` (one i64 slot per flattened leaf
  field) and returns the first leaf of the last expression as an i64.
- `FrogState::call_jit` (`state.rs:254`) is the run harness: publishes `ACTIVE_HEAP`, builds a
  `FrogCtx` from `self.tc`'s struct/union defs, `push_scanned_span`s the out-buffer, `transmute`s
  the finalized pointer to `fn(i64) -> i64`, calls it, tears down.
- GC roots come from three sources (`gc.rs`, `for_each_root`): precise Cranelift stack maps keyed
  by return address (`JIT_CODE`), explicit `push_root` roots, and `push_scanned_span` buffers. The
  stack walk needs frame pointers on both sides (Cranelift flag + `.cargo/config.toml`
  `force-frame-pointers=yes`).

---

## Gaps that must close (blocking)

### G1 — String/byte literals bake a host pointer into code
`TypedExprKind::StrLit` (`codegen/mod.rs:2654`) and `print_fragment` (`:1432`) both do
`bytes.as_ptr() as i64` → `iconst`, and keep the bytes alive in a transient `string_arena: Vec<Vec<u8>>`
only for the duration of the JIT call. In an AOT object this pointer is meaningless — it points
into the compiler's heap, not the produced binary.

**Fix:** emit the literal bytes as a module *data object* and reference it by relocation, not
immediate. For each unique literal: `module.declare_data(sym, Linkage::Local, false, false)`,
`DataDescription::define(bytes.into())`, `module.define_data(id, &desc)`; at the use site
`module.declare_data_in_func(id, bcx.func)` → `bcx.ins().symbol_value` (or `global_value`) for the
pointer, and a separate `iconst` for the length. This works identically under `JITModule` (it also
implements `define_data`), so **both backends switch to it** and `string_arena` is deleted, not
branched. Dedup literals in a `HashMap<Vec<u8>, DataId>` to avoid one data object per occurrence
(also a small JIT win). This is the single largest correctness change and the only one that
touches the hot expression path.

Note: string interpolation (`INTERPOLATION.md`) lowers to `StrLit` + concat/`print_fragment`, so it
is covered by this one fix — verify no third `as_ptr()` site hides in the interpolation desugar.

### G2 — `Codegen` is hard-wired to `JITModule` — **DONE (2026-09-12)**
Implemented as below. `Codegen<M: Module>` is generic; `Ctx.module`, `declare_rt`, `build_func_body`,
and `build_main_body` take `&mut dyn Module` (trait dispatch is compile-time only — zero cost in
emitted code — so the hundreds of `compile_expr*` call sites stay non-generic). `compile_entry` was
split: shared `emit_entry` (declares + defines every function, no finalize; returns `(main_id,
bindings, pending_maps, pending_names)`) lives in `impl<M: Module> Codegen<M>`; the JIT-only
finalize/stack-map-registration wrapper `compile_entry`, plus `new`/`new_with_hosts`/
`dump_jit_symbols`, live in `impl Codegen<JITModule>`. `pub type JitCodegen = Codegen<JITModule>`
is the alias `state.rs` uses. `cranelift_module::Module` is object-safe, so `dyn` worked with no
fallback to a generic `Ctx`. Verified: full suite 1175 passed / 0 failed, JIT behavior unchanged.
The AOT backend (G8) will call `emit_entry` then `ObjectModule::finish()` + serialize `pending_maps`
for load-time registration (G4) instead of the JIT wrapper's live `JIT_CODE` registration.

<details><summary>Original gap description</summary>
`Codegen.module: JITModule` and `build_main_body(&mut JITModule, ...)` are concrete. Most codegen
calls (`declare_func_in_func`, `declare_function`, `make_signature`, `declare_data_in_func`) are on
the `cranelift_module::Module` trait and are backend-agnostic already; the only JIT-only calls are
`get_finalized_function` and `finalize_definitions` (the run/finish step), both of which live in the
harness, not the shared body-emitting code.

**Fix:** make the shared compilation generic over `M: cranelift_module::Module` (either
`Codegen<M>` or pass `&mut dyn Module` into the pass functions). `compile_entry`'s passes 1–3 and
every `compile_expr*` helper become `M`-generic; the finalize/run tail stays JIT-only in `state.rs`,
and a new `finish()` tail for the object backend calls `ObjectModule::finish()` → `object::write`
bytes → `.o` file.

</details>

### G3 — `is_pic=false` is wrong for a relocatable object
Object code linked into an executable needs position-independent code; on aarch64 macOS it is
mandatory. **Fix:** the object ISA sets `is_pic=true`. (Memory: there was already an "aarch64 PIC
fix" in the JIT work — re-use that understanding.) Keep `preserve_frame_pointers=true`
unconditionally; the GC stack walk depends on it in AOT exactly as in JIT. AOT may also raise
`opt_level` to `speed` — the 2026-08-29 measurement that kept it at `none` was explicitly because
JIT compile time is per-run; AOT pays it once, so the GVN/LICM tradeoff flips. Gate opt_level on
backend.

### G4 — Load-time stack-map registration
Stack maps are currently pushed into `gc::JIT_CODE` keyed by absolute addresses known only after
`finalize_definitions`. In an AOT binary those addresses aren't known until load (PIE/ASLR), so the
maps must be *emitted into the object* and *registered at startup*.

**Fix:**
- Serialize each function's `JitFunctionMaps` (return-address offset from function start + the
  SP-relative offset list) into a data section. The per-function `start` becomes a **relocation to
  that function's symbol** (resolved at link/load), while all the offsets are static bytes.
- Emit a startup routine — a generated `__frog_register_stackmaps()` that, at program start, walks
  the serialized table, adds each function's load-time base to build `JitFunctionMaps { start, len,
  maps }`, and calls the existing `JitCode::register`. `JIT_CODE`/`register`/`lookup` are reused
  unchanged; only the *feed* changes from finalize-time to startup-time.
- Call it before `__frog_main`. Simplest wiring: the runtime's generated `main` (G5) calls a
  `#[no_mangle]` runtime helper `frog_register_stackmaps(table_ptr, count)` that the emitted data
  section is passed to. Keeps all address arithmetic in Rust, none in emitted code.

This is the subtlest gap — get the relocation direction right (function symbol → its base) and the
serialization endian/layout pinned by a test.

### G5 — No program entry point / run harness in the binary
`__frog_main` is not a `main`; it needs the `call_jit` harness (heap publish, `FrogCtx`, out-buffer
scanned span, result handling). None of that exists in an AOT binary.

**Fix:** provide a `#[no_mangle] extern "C" fn frog_rt_main(main_fn, bindings_desc) -> i32` in the
runtime that replicates `call_jit`: it initializes the thread-local heap (auto on first touch),
publishes `ACTIVE_HEAP`, builds a `FrogCtx`, allocates the out-buffer, `push_scanned_span`s it with
the GC-slot mask, calls `frog_register_stackmaps` (G4), calls `__frog_main`, then handles the
result and exit code. Generate a tiny `main` (either emitted by codegen, or a fixed Rust `main` in
a `froglang-rt` shim that calls `frog_rt_main` with the exported `__frog_main` symbol). The
bindings descriptor (names, types, GC-slot masks) is compile-time known — emit it as a data table
alongside the code, the same way stack maps are.

### G6 — `FrogCtx` still requires `StructDefs`/`UnionDefs`
`FrogCtx::new` takes `*const StructDefs`/`*const UnionDefs`, and `call_jit` feeds it `self.tc`'s.
`EMBEDDING.md` states no shipped `ToFrog`/`FromFrog` consults them any longer, and no pure-frog
runtime path does — but the *type* still demands them.

**Fix:** confirm (by audit + a test) that a program with host functions but no def-consulting path
runs with empty defs, then let `frog_rt_main` construct `FrogCtx` with empty/leaked `StructDefs`/
`UnionDefs` (or make the pointers nullable and the two accessors panic if ever hit). If any path
*does* need them, serialize the two tables into the binary — but the goal is to prove they're dead
weight at runtime and pass empties. This is a verification task more than a build task.

### G7 — Runtime must be a linkable artifact built with frame pointers
The runtime (gc, ffi, dict, json, host, read, and the frontend parser that `read` pulls in) lives
in `froglang-core`'s lib and is exercised only in-process by the JIT today.

**Fix:** produce a linkable staticlib exposing the `frog_*` symbols. Options: (a) add
`crate-type = ["staticlib", "rlib"]` to `froglang-core` (or a thin `froglang-rt` crate that
re-exports the runtime modules) → `libfroglang_rt.a`. The `#[no_mangle]` attributes already make
the symbols externally visible. **It must be built with `-C force-frame-pointers=yes`** (already in
`.cargo/config.toml`, but confirm it applies to the staticlib profile) or the GC stack walk starts
at the wrong frame — silently, only on a collection that lands wrong. `read` parses at *runtime*
from a runtime string, so the parser is genuinely part of the runtime; that's a binary-size cost,
not a correctness gap, and could later be feature-gated for programs that don't use `read`.

### G8 — Linking + CLI
No `build` subcommand exists (`main.rs` handles only `run`/`check`).

**Fix:** add `frog build <file> [-o out]`: front matter mirrors `compile_and_run` + `eval_file`
(`modules::resolve_file` → typecheck → `monomorphize_generics` → `lower_function_values` →
`desugar_notation` → `number_nodes`), then drive the object `Codegen`, write `prog.o`, and shell
out to `cc`/`clang` to link `prog.o` + `libfroglang_rt.a` (+ the generated `main` / `froglang-rt`
shim) → executable. Add `cranelift-object = "0.135"` (and `target-lexicon`) to `Cargo.toml`.
Cross-compilation is out of scope for v1 (host target only).

---

## Non-blocking considerations

- **Efficiency wins AOT unlocks:** `opt_level=speed`, literal dedup (G1), zero per-run compile
  latency, and no `string_arena` allocation. Cranelift has no cross-function LTO, so inlining is
  limited; don't expect C-level codegen. Startup cost moves from "compile the program" to "register
  stack maps + init heap", which is negligible.
- **Result printing semantics — DECIDED (a):** AOT programs are side-effecting only. Output comes
  from explicit `print(...)`; the final top-level expression's value is evaluated (for its side
  effects) and then **discarded**, not printed. This matches essentially every AOT-compiled
  language — a compiled program is a script, not a REPL entry — and means `frog_rt_main` (G5) needs
  no `FrogValue` decoder at all: it calls `__frog_main` for effect and ignores the returned i64.
  `frog run` keeps its current REPL-style final-value printing; only `build` differs. This
  divergence is intentional and must be documented in `README.md` so it isn't mistaken for a bug.
- **Exit codes / panics:** `frog_panic` already prints and exits the process; keep that. `?`/`!`
  desugar to `match` with no special codegen, so error propagation needs nothing new. Decide the
  process exit code for an uncaught top-level error path.
- **Determinism / GC stress:** `FROG_GC_STRESS` env var still works in the AOT binary (read at heap
  init). Good for a differential test that runs the corpus under stress in both modes.

---

## Suggested order of work

1. ~~**G2** (abstract `Codegen` over `Module`)~~ **DONE** — no behavior change; JIT suite green.
2. **G1** (literals as data objects) — flip both backends, delete `string_arena`, dedup. Verified
   entirely by the existing JIT test suite (behavior must be identical).
3. **G7** (runtime staticlib) + **G8** skeleton (`frog build` that emits `.o` and links a trivial
   `main` that just calls a hand-written `__frog_main`) — prove the link line and symbol resolution
   on a program with no strings and no GC first.
4. **G5**/**G6** (`frog_rt_main`, empty `FrogCtx`) — run a real side-effecting program end to end.
5. **G4** (load-time stack maps) — the correctness keystone; validate with a GC-stress differential
   test over the whole `tests/*.frog` corpus, AOT output vs JIT output byte-for-byte.
6. **G3** opt-level bump + measurement; polish, docs, `roadmap.md` update.

## Testing strategy

A single differential harness: for every existing `tests/*.frog` / example, run it via `frog run`
(JIT) and via `frog build && ./out` (AOT), assert identical stdout/exit — once normally and once
under `FROG_GC_STRESS=1` (which forces a collection at every allocation, so it stresses G4's
stack-map registration and G1's data-object roots hardest). Pin the stack-map serialization layout
with its own round-trip unit test (`register` then `lookup` a known offset), mirroring
`tests/test_cranelift_stack_maps.rs`.
