# Self-hosting via an emit-C backend

Status: **design** (2026-09-16). Nothing here is implemented. This is the plan for rewriting
froglang's *compiler* in froglang, with a backend that emits C and links against the existing
Rust runtime staticlib (`libfroglang_core.a`). It supersedes the bare "Path to a self-hosting
compiler" bullet list in `roadmap.md`, which tracked the language-feature prerequisites (generics,
modules, `Dict`, AOT — all now done) but never named the two things that actually gate the work:
a backend a frog program can drive, and the compiler-as-a-program plumbing to run it.

## Goal

`froggc.frog` — the froglang compiler, written in froglang — reads a `.frog` file, lexes, parses,
type-checks, and **emits C**, then shells out to the system `cc` to compile that C and link it
against `libfroglang_core.a`. Self-hosting is reached when `froggc` can compile its own source to
a working executable, and doing so twice in a row produces the same compiler (the standard
three-stage bootstrap fixpoint, below).

**What stays in Rust:** the runtime — GC, string arena, `List`/`Dict`, the `read`/`json`/`str`/`fs`
host functions, `frog_rt_main`. This is not a concession; it is what self-hosting means everywhere.
A self-hosted compiler compiles itself; the runtime is a library it links, exactly as GHC links its
RTS and rustc links `std`. Rewriting the runtime in froglang is a separate, later, much larger
project (`plans/CONCURRENCY.md`'s I/O model and a frog-level allocator would be prerequisites), and
it is explicitly **out of scope** here.

**What this is not:** it is not the Cranelift-driving FFI sketched in `plans/FFI.md` (if written).
That path leaves a permanent Rust+Cranelift dependency under the "self-hosted" compiler and requires
wrapping Cranelift's entire API across the FFI boundary. Emitting C needs only string building
(done), `write_file` (done), and two small stdlib primitives (G1/G2 below), and it makes the
backend genuinely frog-owned.

## Why C, and not CLIF

Cranelift IR *does* have a textual form — it is what `FROG_DUMP_CLIF` prints, and
`cranelift-reader`/`clif-util` parse and compile it. It is nonetheless the wrong target:

- **It does not shed the dependency.** `clif-util` is not a system-installed toolchain; you would
  ship a Cranelift-version-pinned binary and shell out to *that*, so the "self-hosted" compiler
  still rides on Cranelift. Relative to just linking Cranelift (the FFI path), emitting CLIF text
  buys nothing.
- **CLIF is unstable** across Cranelift versions (the AOT work already ate a 0.113 → 0.135
  migration, `RUNTIME.md` Part 2 "As built").
- **CLIF is SSA and low-level.** The frog backend would have to do SSA construction *and* carry
  the precise-stack-map/GC-root contract itself — i.e. all the hard parts. A C compiler does
  instruction selection, register allocation, and (via our shadow stack, G6) hands us a portable
  root story for free.

C is a stable, universally-installed target, high-level enough that the emitter is a direct
transcription of the existing `codegen/mod.rs`, and it lets `cc -O2` do the optimization the
Cranelift AOT path pays `opt_level=speed` for.

## The pivot: both backends target the same runtime ABI

The reason this is tractable rather than a rewrite is that the emit-C backend and the existing
Cranelift AOT backend aim at the *identical* runtime interface:

- Every runtime entry point is already `#[no_mangle] extern "C" fn frog_*` (`ffi.rs`, `dict.rs`,
  `runtime/host.rs`, `read.rs`, `json/mod.rs`) — the emitted C `#include`s a header declaring them
  and calls them by name; `cc` links them from `libfroglang_core.a` exactly as the AOT linker does
  (`plans/AOT.md`, G7).
- Host functions use the uniform `extern "C" fn(ctx, args, out)` shim, imported by symbol
  (`EMBEDDING.md`, "The uniform shim ABI"). Emitted C calls `__frog_host_<name>` with a stack
  arg-buffer and out-buffer — the same calling convention `compile_host_call` emits into CLIF.
- The value representation is the flattened-i64-leaves model (`struct_fields` order, `Float`/`Bool`
  normalized through `to_i64_repr`). In C, every frog value is one or more `int64_t` locals;
  scalar arithmetic and comparison are plain C on `int64_t`/`double` (with bit-casts for `Float`);
  everything heap goes through a `frog_*` call.

So **`codegen/mod.rs` is the reference spec for the C emitter.** Port it construct by construct,
printing C where it currently builds CLIF. This also gives us the strongest possible test oracle
(G9).

---

## Prerequisites — the compiler-as-a-program plumbing

These block writing `froggc.frog` at all. Both are small, self-contained stdlib additions
(`froglang-core/src/stdlib/`) and can land immediately, independent of everything else.

### G1 — `args()`: command-line arguments
Today `argv` is consumed entirely by the Rust `main.rs`; a `.frog` program cannot see its own
arguments. A compiler needs to know which file to compile and where to write output.
**Add** a host function `args() -> List<Str>` (from `std::env::args`). Small `#[frog_fn]` in a new
`stdlib/env.rs`. Until it lands, `froggc` could read source from stdin (`read_stdin_all`, done) and
write to a fixed path, but that does not scale to multi-file/module input — do G1 first.

### G2 — `exec()`: subprocess execution
No `exec`/`spawn`/`command` host function exists. Without it, an emit-C compiler cannot invoke `cc`
to finish the job. **Add** `exec(cmd: Str, args: List<Str>) -> ExecResult` where
`ExecResult(code: Int, stdout: Str, stderr: Str)` (or a `T | ErrMsg` on spawn failure), wrapping
`std::process::Command`. This is the single most important missing primitive for the whole plan.
Keep it capability-flavored (`plans/CONCURRENCY.md` wants I/O to be injectable authority) but a
plain host function is fine for now.

### G3 — arithmetic and bit primitives
The language has `+ - * /` only — no `%` (modulo) and no bitwise `& | ^ << >>` (verified in
`frontend/tokens.rs`; `Pipe` is union syntax, not bitwise-or). A lexer needs character-class math,
a symbol table needs hashing, and C string-literal escaping needs `%`/`>>`. **Add** `%` and the
bitwise operators. Cheapest form: new infix tokens desugaring to existing integer codegen; a
runtime change is not needed. (Alternative: host functions `mod`/`band`/`shl`/… — uglier at call
sites but zero grammar work. Prefer operators.)

### G4 — lexer ergonomics: `ord`/`chr` and string ordering
`stdlib/str.rs` has `chars` (→ `List<Str>` of one-char strings), `slice`, `index_of`, `split`,
`to_int`. A lexer classifies characters; doing it via single-char-string equality is workable but
slow and clumsy. **Add** `ord(s: Str) -> Int` / `chr(n: Int) -> Str` (first codepoint / from
codepoint) and confirm/enable `<`/`>` on `Str` (lexicographic) so `"a" <= c and c <= "z"` works.
Not strictly blocking — a charset-`index_of` workaround exists — but cheap and it makes the lexer
readable.

**Explicitly not required:** escaping closures (`plans/CLOSURES.md` tier 2). Write `froggc` in
match-dispatch style — a `match` over AST-node unions, not a table of function values — which fits
frog's grain and needs only tier-1 first-class functions (done). Revisit only if the code demands it.

---

## The backend — `froggc.frog`

Written in froglang, using only features the emit-C backend supports (grow the two together; see
G9). Structured as the existing compiler is.

### G5 — the runtime header and value model in C
Write `froglang-core/runtime.h` (shipped beside `libfroglang_core.a`): C declarations for every
`frog_*` runtime function and every `__frog_host_*` shim the stdlib exports, plus the value
representation as C types. Decisions to pin here, all inherited from `RUNTIME.md` Part 1 and
`struct_fields`:
- Every frog value is `int64_t` per leaf. Multi-leaf values (structs, unions) are N adjacent
  `int64_t` — as separate C locals, or a small `int64_t[N]`, in `struct_fields` order.
- Tagged pointers: heap references carry tag bits; the C side never inspects them, only passes them
  to `frog_*`. `Float` is a bit-cast of the `int64_t` (`to_i64_repr`); `Bool` is 0/1.
- Unions are the `(tag+ptr, scalar)` pair `codegen::pack_union_member` builds; `match`/`?`/`!`/
  `catch` lower to a C `switch`/`if` on the tag word, exactly as `fold_match_arm` does.
- String literals become C `static const uint8_t[]` handed to `frog_alloc_str` (mirrors the AOT
  G1 decision to make literals data, not baked pointers).
This header is also independently useful: it is the documented C ABI of the runtime.

### G6 — GC roots for emitted C (**the crux — spike this first**)

> **Superseded (2026-11-10):** the GC was deleted (`plans/NOGC.md` step 3), so there are no roots to
> track and G6 no longer exists. The text below is kept for history; the spike it asks for is moot.

The precise root story (`RUNTIME.md` Part 2: Cranelift user stack maps, keyed by return address,
walked via frame pointers) is **Cranelift-specific and does not apply to C frames** — we do not
control C's stack layout, so we cannot produce those maps. The collector must find roots in
emitted-C code another way. Two options; the runtime is closer to the first than it looks.

**Option A (recommended): reuse the existing explicit-roots API as a shadow stack.** The GC
*already* has `GcHeap::push_root(value, is_ptr)` / `pop_roots(n)` / `roots_len()` and traces them
in `for_each_embedding_root` — that is precisely a shadow stack, built for the embedding API and
for `RuntimeRoots` (host-shim argument rooting). The emitter, which knows every local's type,
emits `frog_push_root(v)` for each heap-typed local as it becomes live and `frog_pop_roots(n)` on
every exit path. Requirements:
- Expose `push_root`/`pop_roots`/`roots_len` as `frog_*` C symbols (thin wrappers if not already
  exported).
- Handle *every* exit correctly: normal `return`, `?` propagation, and `catch` all must restore the
  root-stack depth. Snapshot `roots_len()` at function entry / `catch` entry and truncate back to it
  — the same mark/restore discipline `HostRootScope::scope` (`runtime/host.rs`) already uses.
- It is **precise** (no false retention) and **portable** (no `setjmp`, no register-flush, no
  platform stack-walking).
- The shadow stack was *retired* from the Cranelift path (`MUTABILITY.md` Stage 6a, `RUNTIME.md`
  Part 2 "Why") because it duplicated Cranelift's own liveness and cost ~30% on `orders.frog`.
  Neither objection applies here: there is no Cranelift liveness to delegate to in a C target, and
  a ~30% hit on a batch compiler is irrelevant. The retirement removed it as a *competitor* to
  stack maps, not as a technique.

**Option B (fallback): conservative stack scanning.** Because the heap is **non-moving mark-sweep**
(`RUNTIME.md` Part 2: "non-moving collectors are the supported case"), a misidentified integer only
*retains* an object, never corrupts one — so scanning the C stack conservatively is *safe*, unlike
under a moving GC. Needs, in the runtime: (1) capture the stack base at `frog_rt_main` entry and the
current SP at collection; (2) a `setjmp`-style register flush so in-register roots hit the stack;
(3) an object-validity predicate ("does this tag-masked word point at a live heap object?"), which
needs an object-start index the mark-sweep heap does not currently maintain. Lower *emitter* effort
(the C is root-agnostic) at the cost of runtime work and some floating garbage. Acceptable for a
bounded compile.

**Recommendation:** Option A. The root API exists, it is precise, and the emitter already computes
the type of every binding. **Build a throwaway spike of G6 before committing to the rest** — hand-
write the emitted C for one small allocating, recursing, `catch`-ing program, run it under
`FROG_GC_STRESS=1`, and confirm no root is lost. This de-risks the whole project; everything else is
comparatively mechanical.

### G7 — the C emitter
A pass over the typed, lowered AST (the same AST reached after
`monomorphize_generics` → `lower_function_values` → `desugar_notation`, per `main.rs::build`) that
prints C. Port `codegen/mod.rs` construct by construct:
- Each frog `func` → a C function; parameters and returns are the flattened i64 leaves.
- Scalars → C operators; heap ops → `frog_*` calls; host calls → `__frog_host_*` with arg/out
  buffers; `match`/unions → tag `switch`; loops/`for` → C loops; `?`/`catch` → the tag-test-and-
  branch `fold_match_arm` emits, plus the G6 root restore.
- A generated C `main` that calls `frog_rt_main` (runs for side effects, discards the final value —
  the AOT semantics from `plans/AOT.md`, decision (a), and README "Ahead-of-time compilation").
Emit readable, `#line`-annotated C so a miscompile is debuggable; correctness first, not clever C.

### G8 — driving `cc`
`froggc` writes `out.c`, then `exec("cc", ["out.c", "-I<hdr>", "<runtime>/libfroglang_core.a",
"-o", out])` (G2). Reuse `main.rs::build`'s runtime-lib discovery (next to the compiler exe,
overridable via `FROG_RUNTIME_LIB`) and its up-front existence check. `cc -O2` for release. This
mirrors the existing AOT link step exactly, only the object comes from `cc` instead of
`ObjectModule::finish()`.

---

## G9 — Testing: a third differential backend

The AOT work built a whole-corpus JIT-vs-Cranelift-AOT differential, green under `FROG_GC_STRESS=1`
(`plans/AOT.md`, `tests/test_build_cli.rs`). **Extend it to a third column: emit-C.** Every corpus
program must produce identical output under `frog run` (JIT), `frog build` (Cranelift AOT), and
`froggc` (emit-C), including under GC stress. This is the emitter's oracle and the reason porting
from `codegen/mod.rs` is safe: any divergence is a located bug. Define a **"compiler subset"** — the
language features `froggc.frog` itself uses — and gate bootstrap on that subset passing the
differential before attempting to compile the whole compiler.

## The bootstrap

Standard three stages. Note the front-end (lexer/parser/type-checker in frog) is pure computation
and buildable **today** — start it in parallel with G1–G8; it needs nothing new.

1. **Stage 0 → 1.** The Rust compiler (Cranelift AOT, `frog build`) compiles `froggc.frog` →
   `froggc-1`, a native binary whose *backend is emit-C*.
2. **Stage 1 → 2.** `froggc-1` compiles `froggc.frog` → `froggc-2` (this binary is built by the
   emit-C toolchain: C from `froggc-1`, then `cc`).
3. **Stage 2 → 3.** `froggc-2` compiles `froggc.frog` → `froggc-3`.

**Fixpoint:** `froggc-2` and `froggc-3` are both produced by the emit-C path and must be identical
(bit-for-bit if `cc` is deterministic, behaviorally otherwise). `froggc-1` differs — it was code-
generated by Cranelift, not `cc` — so it is *not* part of the equality check; it only needs to
behave correctly. Reaching a `froggc-2 == froggc-3` fixpoint is the definition of done.

## Suggested order

1. **G1, G2** (args, exec) — unblock writing any compiler-as-a-program. Small.
2. **G6 spike** — hand-written C for one GC-stressed program. De-risks everything.
3. **Front-end in frog** (lexer → parser → type-checker), in parallel — no new capabilities needed.
4. **G3, G4** (bit ops, `ord`/`chr`) as the front-end demands them.
5. **G5, G7, G8** (header, emitter, link) + **G9** differential, growing the compiler subset.
6. **Bootstrap** to the fixpoint.

## Open questions

- **Modules across the C boundary.** froglang modules are resolved before codegen (`resolve_file`),
  so `froggc` sees one merged AST — no per-module C compilation units needed for v1. Separate
  compilation (one `.o` per module, incremental) is a later optimization, not a blocker.
- **`repr`/`print` of user types** already lower to typed-AST desugars over `struct_fields`
  (`DATA.md` Stage 5) — the C emitter should inherit them for free, but confirm the desugar runs
  before G7 sees the AST.
- **Debug info.** `#line` into the emitted C points at the C, not the `.frog`. Mapping back to frog
  source is deferred; emit a comment with the frog span per statement for now.
- **Which `cc`.** Assume a system `cc` (clang/gcc). No `clang`-specific flags in v1.
