# froglang runtime & codegen model

Status: **Parts 1 and 2 implemented** (2026-08-22). Part 3 is still a proposal. Written after
two GC-rooting use-after-frees traced to the same structural cause; the measurements below are
real.

Where the implementation diverges from what was proposed, this document says so inline under
"As built" — the proposal text is left standing rather than rewritten, so the reasoning that
produced it stays legible next to what it actually cost.

This covers three coupled decisions — how union values are represented, how the GC finds its
roots, and what codegen compiles *from*. They are written up together because each one's
cheapest solution is blocked by the others: precise GC roots want Cranelift's stack maps,
stack maps require every root slot to be statically a pointer, and that requires changing the
union representation. The IR sits underneath all of it.

Read `MUTABILITY.md`'s stage 6 first for the history — this document is what that section's
post-mortem points at.

## The problem, stated once

Three symptoms, one cause.

**Symptom 1: GC roots are assigned by a cursor that cannot be made sound.** Shadow-stack slots
are handed out by `ctx.heap_cursor`, bumped in `compile_expr_multi`'s evaluation order and
predicted independently by `max_heap_slots`. Reuse of a slot is only legal where its current
occupant is dead, and nothing checks that. Two bugs so far: the reverted `Conditional`
branch-sharing, and the still-open loop back-edge hole (`tests/test_gc_roots.rs`). Both are
"an analysis's model of control flow diverged from what the code does".

**Symptom 2: a union's payload slot is only sometimes a pointer.** `is_two_slot_union` gives a
scalar-carrying union an unboxed `(tag, payload)` pair where `payload` is a pointer exactly
when `tag` names a boxed member. Everything that scans memory then needs the tag alongside the
word: `cond_mask`/`boxed_tags` in `gc_masks`, `root_two_slot_payload`'s `select` dance,
`heap_roots_in_leaves`' backwards peek at `vals[i-1]`. It also costs expressiveness —
`gc_masks` supports **at most one distinct two-slot-union shape per aggregate**, a restriction
`check_scalar_union_consistency` exists solely to enforce.

**Symptom 3: nominal unions with payloads always box.** `data Discount is NoDiscount |
Percent(pct: Int) | ...` allocates a `FrogVariant` per value — roughly 4M mallocs in
`benches/orders.frog`, the top item on `roadmap.md`'s perf list.

The common cause: **pointer-ness is a runtime fact rather than a static property of a slot.**
Struct flattening works precisely because every leaf's pointer-ness is static; `ptr_mask` works
for the same reason. Unions are the one place that invariant was given up, and every piece of
special-case machinery above is the interest payment.

## Measurements

Two experiments, run 2026-08-22.

**In-tree.** Flattened structs of width 1/2/3 threaded through `fallible`'s exact control flow,
so slot count is the only variable. A list-resident variant separates register pressure from
memory traffic. (`l2`/`l3` do identical work; only the stride differs.)

| | 1 slot | 2 slots | 3 slots |
|---|---|---|---|
| registers / ABI, 800k iters | 4.87 ms | 5.19 ms | 5.20 ms |
| list-resident, 200k elems | 0.88 ms | 1.50 ms | 2.42 ms |

Widening is **free in registers** (2→3 slots costs +0.4%, noise) and **expensive in memory**
(+61%).

**Standalone microbenchmark**, four candidate representations, ns/element over 4M elements:

| | construct+test+extract | store+load in array | GC scan |
|---|---|---|---|
| two-slot `(tag, payload)` — today | 0.76 | 2.59 (16 B) | 0.48 |
| three-slot `(tag, scalar, ptr)` | 0.91 | 3.03 (24 B) | 0.34 |
| **tagged-pointer `(scalar, ptr\|tag)`** | **0.70** | 2.61 (16 B) | **0.24** |
| NaN-boxed (one word) | 0.75 | **0.86** (8 B) | 0.24 |

Note the microbenchmark puts 2→3 slots in an array at +17% where the real compiler shows +61%.
froglang's list path costs 7.5 ns/elem against Rust's 2.6, so per-element overhead
(comprehension machinery, realloc copying, GC scanning the growing list) scales with stride and
amplifies it. Treat +17% as a floor, not an estimate — and treat the gap itself as a separate
optimization target.

### Why not NaN-boxing

It wins decisively on memory and its encode/decode is free. It was rejected anyway, for two
reasons that are about representable programs rather than speed:

- **One word holds one payload.** `data Tree is Leaf | Node(v: Int, l: Tree, r: Tree)` needs
  three; `BulkOver(min_qty: Int, pct: Int)` needs two. Those keep boxing — so NaN-boxing
  optimizes the case that is *already* unboxed and does nothing for symptom 3, which is the
  expensive one.
- **The Int range cut has to be global.** A 3-bit tag leaves i48 (i45 for 64 members). If that
  applied only inside unions, `let y: Int | Str = x` would silently truncate a large `x` with
  no diagnostic. Avoiding the cliff means Int is i48 language-wide, contradicting DESIGN.md's
  `int = isize`. Additionally only one member per union can be "the raw double", and computed
  NaN floats collide with the tag space unless every float result is canonicalized.

Shortening Int would be an acceptable price for a big win. It is not an acceptable price for a
win that leaves the main allocation problem untouched.

## Part 1 — Union representation: tagged pointers

### The word encoding

One encoding for every GC-visible word in the system, so the collector needs no per-slot
metadata and no type information. `alloc_bytes` uses `Layout::from_size_align(words * 8, 8)`,
so every heap object address has its low 3 bits clear. Those 3 bits carry the tag.

| low 3 bits | meaning |
|---|---|
| `000` | a plain pointer (or `0` for null/absent) — `Str`, `List`, a boxed union |
| `001`–`110` | a union's pointer slot: member tag in the low bits, pointer in `w & !7` |
| `111` | not a pointer; the upper 61 bits are data (a payload-less variant's tag) |

The collector's rule, applied uniformly to every root and every scanned slot:

```
fn is_heap_ptr(w: i64) -> bool { w & 7 != 7 && (w & !7) != 0 }
fn heap_ptr(w: i64) -> *mut GcHeader { (w & !7) as *mut _ }
```

Two ALU ops, no branch on slot kind. A scalar-carrying member leaves the pointer part zero, so
its word is just the small tag `1..=6`, which masks to `0` and is correctly not followed.

**Member tags are `1..=6`.** `0` is reserved so a plain non-union pointer is indistinguishable
from an untagged one, and `7` is reserved for immediates. Six inline members; a union with
seven or more falls back to today's boxed one-slot representation. That is an acceptable
ceiling — large sums want boxing anyway, and every union in the tree today has at most four
members. Raising `alloc_bytes` to 16-byte alignment would buy 4 bits and 14 members at the cost
of some fragmentation; not proposed, but the knob exists.

Tags are assigned by position in the **normalized** member list (`Type::normalize` flattens,
dedups and sorts), so a tag is stable for a given type regardless of how it was spelled.

### The encoding collision, explicitly

This is the part most likely to reintroduce a use-after-free, so it gets stated as an invariant
rather than an argument.

Today `gc::immediate_variant` encodes a payload-less variant as `(tag << 1) | 1` and
`is_heap_ptr` is `v != 0 && v & 1 == 0` — "even means pointer". That scheme is **incompatible**
with the one above, and not subtly: under a 3-bit mask, `immediate_variant(4) == 9`, and
`9 & !7 == 8`, a non-zero value that the collector would happily dereference as the address `8`.
Any partial migration that leaves both encodings live produces exactly that.

The resolution is that the immediate scheme is **subsumed**, not coexisted with:

- A payload-less variant of a union that fits inline is no longer an immediate at all. It is
  its tag in the pointer slot: the word `t` for `t` in `1..=6`. No separate encoding.
- A payload-less variant of a **boxed** union (seven or more members) keeps an immediate, but
  re-encoded as `(tag << 3) | 7` so it lands in the reserved `111` class. `immediate_variant`
  and `immediate_variant_tag` change together.
- `Type::None` currently compiles to the immediate `1`. As a standalone unit value it becomes
  `7` (the `111` class, tag data zero). As a *union member* it is just its member tag, like any
  other payload-less member.

The migration invariant: **no word reaches the collector under one encoding while another is in
effect.** Since `is_heap_ptr` is the single choke point every consumer already routes through
(mark roots, list element slots, variant payload slots, `heap_roots_in_leaves`), changing the
encoding means changing that function and every producer in the same commit. There is no
intermediate state where half the producers have migrated; a staged rollout of this specific
change is not available and should not be attempted.

Worth asserting in debug builds at the point of production, not just hoping: every word written
into a GC-visible slot should satisfy "masks to zero, or masks to an address this heap
allocated".

### Layout

For a union whose members fit inline, partition every member's fields by static pointer-ness,
overlay the members, and host the tag in the first pointer slot:

```
width = max over members(#scalar leaves) + max over members(#pointer leaves)
```

If a union has no pointer leaves in any member, there is no slot to host the tag, so it gets a
dedicated leading tag slot — which is never GC-visible, so it costs nothing but width.

Worked examples against types in the tree today:

| type | today | proposed | note |
|---|---|---|---|
| `Int \| PricingError` | 2 slots, `cond_mask` | 1 scalar + 1 ptr = **2 slots** | same width, statically scannable |
| `Str \| List(Int)` | 1 slot, boxed, one malloc per value | 1 ptr = **1 slot**, no allocation | tags 1 and 2 distinguish them |
| `Discount` | 1 slot, boxed, one malloc per value | tag + 2 scalars = **3 slots**, no allocation | symptom 3, solved |
| `Tree is Leaf \| Node(v, l, r)` | 1 slot, boxed | 1 scalar + 2 ptrs = **3 slots** | node unboxes; children stay boxed, being self-referential |
| `Category` (4 payload-less) | immediate | 1 slot, tag only | |

### What this deletes

`cond_mask`, `boxed_tags`, `root_two_slot_payload`, `assert_no_two_slot_union_leaf`,
`is_two_slot_union`'s special casing in `struct_fields`/`gc_masks`/`heap_roots_in_leaves`, and
`TypeChecker::check_scalar_union_consistency` along with the one-scalar-union-shape-per-aggregate
restriction it enforces. `gc_masks` collapses back to a plain `ptr_mask`.

### As built

Three deviations, all conservative.

**Self-referential unions box entirely, rather than unboxing the node and boxing its
children.** The proposal's `Tree` row wants a value's representation to depend on *where* it
sits — inline as a local, boxed as a `Node`'s field — which needs a box/unbox conversion at
every field read and write. Representation is instead a function of the type alone
(`union_is_inline`): a union reachable from itself, directly or through a struct field, keeps
today's one-slot boxed form. That is exactly its current behaviour, so it is a missed win
rather than a regression, and it is what makes `union_layout`'s recursion terminate without a
visited set threaded through every caller. `Tree` and a struct-mediated cycle are both tested
(`tests/test_union_repr.rs`).

**The tag sometimes needs a column of its own** — the second open question below, answered.
Overlaying the tag on a member's first pointer leaf is only safe when that word has three spare
low bits, which a `Str`/`List` pointer does and a *union* leaf does not: an inline union's slot
0 already carries its own tag, and a boxed union's word may be an immediate. `UnionLayout`
detects that case (`overlay_safe`) and gives the tag a dedicated leading column, one slot wider
and still statically scannable. Every union in the tree today overlays; the dedicated case is
reached only by a union nested directly inside a union member.

**The tag is always slot 0**, whether it shares that word with a pointer or owns it. Reading it
is `w & 7` either way, with no per-union branch at the read site, and the scannable columns stay
a contiguous prefix.

Measured on `benches/orders.frog`: **90 ms → 60 ms** (same result value), from `Discount`,
`Category` and `Int | PricingError` no longer allocating.

## Part 2 — GC roots: Cranelift stack maps

### Why

The shadow stack is a hand-written liveness analysis competing with the one Cranelift already
runs for register allocation. Ours has produced two use-after-frees; theirs is load-bearing for
every Cranelift user. Delegating removes the bug class rather than fixing instances of it, and
removes the runtime cost with it — `project_gc_shadow_stack_perf` measured shadow-frame
maintenance at ~30% of `orders.frog` and 67% of the `?`/`catch` penalty.

Confirmed available in the pinned cranelift 0.113:

- `FunctionBuilder::declare_value_needs_stack_map(val)` (`cranelift-frontend/src/frontend.rs:563`)
- spilling and liveness in `frontend/safepoints.rs`
- `CompiledCode::buffer.user_stack_maps() -> &[(CodeOffset, u32, UserStackMap)]`, keyed by
  return address
- values are spilled but not reloaded, i.e. **non-moving collectors are the supported case** —
  which is what we have

### The constraint that dictates Part 1

Cranelift's stack-map liveness is a **use-driven backward analysis**: uses mark a value live,
defs kill it, fixpoint over real predecessors. A value with no uses is dead immediately after
its def and appears in no stack map.

That rules out the obvious port of `root_two_slot_payload`. You cannot compute a sanitized
`select(is_boxed, payload, 0)` copy purely for the collector's benefit and declare *that* — the
mutator keeps using the raw payload, so the sanitized value has no uses and is never recorded.
The shadow stack tolerated the trick because we controlled the store; stack maps derive liveness
from the program and there is no way to fake a use.

**Hence the rule: the value you declare must be one the mutator genuinely uses, and statically
a pointer.** The tagged-pointer word satisfies both — it is masked to get the pointer and read
in the low bits to get the tag, so every narrow, every call passing the union along, every store
into an aggregate, every return is a real use. And when it genuinely has no uses (a union only
ever tested with `is`, never narrowed) its absence from the map is correct, not a hole.

### Mechanics

1. Cranelift emits stack maps keyed by return address. `preserve_frame_pointers` is enabled on
   the JIT side and `-C force-frame-pointers=yes` on the Rust side (`.cargo/config.toml`), and the
   walk starts inside the runtime FFI function the mutator called into — `caller_frame_pointer!`
   reads that function's own frame pointer, and `JitFrameGuard` (`gc.rs`) publishes it for the
   duration of the call, restoring the previous one on the way out so re-entrant calls (printing
   a union runs JIT-compiled formatting) nest correctly.
2. `gc::JitCode` is a sorted table of `(start, len, maps)` across all JIT'd functions, built once
   each function's address is known (`finalize_definitions`) and resolving a return address to its
   map in `O(log n)`.
3. Roots come out as SP-relative byte offsets. Read each word, apply the uniform `is_heap_ptr`/
   `heap_ptr` rule from Part 1. No per-root type metadata is needed, which is the whole payoff of
   a single encoding.

### As built

The plan above is what's implemented, with two additions the plan didn't anticipate.

**A prerequisite: cranelift-frontend 0.113 → 0.135.** `declare_var_needs_stack_map` only
propagated through `use_var`/`def_var` in 0.113, not through block parameters the SSA builder
inserts purely to route a variable's value between blocks that never mention it. A value could
sit in exactly such a parameter across a safepoint and be silently absent from that safepoint's
map — the same missed-root class stack maps were meant to remove, just relocated. Confirmed with
a standalone probe before touching any froglang code; `tests/test_cranelift_stack_maps.rs` pins
the property against a regression. 0.135 moved the tracking into the SSA builder itself, which is
the fix, plus an unrelated but required migration (`MemFlags` is now an interned entity index,
block arguments are `BlockArg` not `Value`, `stack_load`/`store` take a pointer type, `*_imm`
builders want an explicit sign/zero-extend variant).

**A residual conservative region: `gc::RuntimeRoots` and `gc::push_scanned_span`.** Stack maps are
precise for JIT frames, but two places still need help:

- A runtime FFI function (`ffi.rs`) that receives a GC pointer as an argument and can itself
  trigger a collection, but still needs that argument afterward, is not otherwise rooted — from
  the JIT caller's point of view the argument died *at* the call, so it is correctly absent from
  the caller's own map. `RuntimeRoots::hold` pushes explicit roots for the duration of such a
  function's body; the rule is stated once as "any runtime function meeting both conditions holds
  its pointer arguments for its whole body" rather than reasoned about per call site, since that
  reasoning is exactly what produces a use-after-free the next time such a function is edited.
- `__frog_main`'s `out_ptr` buffer (and the one-shot `compile_and_run` path's equivalent) is
  filled with top-level bindings as they're created, but `FrogState::eval` only turns it into
  explicit roots *after* the call returns. A binding whose last JIT-side use has already passed —
  built early, read back only through `out_ptr` after `eval` returns — is reachable only through
  that buffer in between. Found by a real crash (`struct_cart_total.frog`, reduced to
  `tests/test_gc_roots.rs::top_level_binding_survives_a_collection_triggered_before_eval_returns`):
  the shadow stack hid this by keeping every root alive for the whole function; precise roots
  correctly stopped doing that and nothing scanned the buffer instead. `push_scanned_span`
  registers the buffer's GC-scannable slots (from `gc_slots_of_bindings`) for the duration of the
  call, conservatively scanning that one buffer rather than relying on a JIT-side root for a value
  the JIT side is, correctly, already done with.

Both are narrow, deliberate exceptions to "every root comes from a Cranelift stack map" — confined
to the two places precise roots cannot reach by construction (a value crossing into Rust code with
no map of its own; a value crossing out through a buffer the caller owns), not a reversion to
conservative scanning generally.

Measured on `benches/orders.frog`: unchanged at 60 ms — Part 2 is about correctness and about
retiring hand-written machinery, not about this benchmark's time, which Part 1 already fixed.
`FROG_GC_STRESS=1` (a collection on every allocation) passes across the whole suite, including the
previously-`#[ignore]`d `test_gc_roots::loop_body_producer_does_not_clobber_an_escaping_binding` —
un-ignored, since binding-owned roots were exactly what it needed and Cranelift's live-range
analysis gives every `Variable` its own.

`gc::ShadowFrame`, `ShadowTop`, `setup_shadow_frame`, `teardown_shadow_frame`,
`root_heap_value`, `root_flat_leaves`, `max_heap_slots`, `Ctx.heap_slot/heap_cursor/heap_max`
are gone. So is `MUTABILITY.md`'s proposed stage 6a slot allocator — it was never built; this
subsumed it before it needed to be.

If a moving collector is ever wanted, spilled slots would need rewriting with the tag preserved.
Noted, not designed.

## Part 3 — A mid-level IR

The lower-priority half, and the one to defer until a feature forces it.

Codegen walks the typed AST directly; there is no CFG between the type checker and Cranelift. So
every analysis needing control flow re-derives it as a structured fold mirroring
`compile_expr_multi` arm for arm. `max_heap_slots` does this, `liveness.rs` does it again with
its own hand-rolled fixpoint, and the codebase's own comments call the resulting coupling a
hazard (`Ctx.heap_max`, `max_heap_slots`). Both GC bugs were instances of it, and the frontend
has the same disease in a different key — `all_roots` indexed positionally against a list it
isn't parallel to, `func_mut_params` keyed by bare name with no scoping.

Parts 1 and 2 remove the *GC* consumer of that machinery, which is the acute problem. The IR is
for the ones coming: structured concurrency puts suspension points everywhere, effects and
capabilities are a dataflow lattice, and refinement types with pre/post-conditions need path
sensitivity. Those are all CFG analyses, and writing a fourth hand-rolled traversal is the point
where the cost stops being recoverable.

Shape: flat basic blocks, explicit locals, real predecessor edges. It goes *underneath* the type
checker's existing lowering — desugaring `match`/`?`/`!`/`catch` to `Conditional`, union
unification, narrowing all stay exactly as they are and simply target blocks instead of nested
expressions. Nothing in the surface language or type system changes.

What it buys beyond hygiene: **escape analysis becomes mechanical**, and that is where the
remaining performance actually is. `orders.frog` at 182 ms against 31 ms for the same program in
Rust is dominated by allocation volume, and stack-allocating the non-escaping majority is the
general lever — Go's whole performance story on a GC'd value-semantics language. Unboxing union
members one shape at a time is a series of special cases chasing the same goal. Move-on-last-use
and COW's `shared` bit also fall out, both being CFG notions.

Cost: `liveness.rs` is superseded. It is uncommitted, so this is the cheapest moment it will
ever be.

## Sequencing

1. ~~**Tagged-pointer union representation.**~~ Done — see Part 1's "As built".
2. ~~**Stack maps.**~~ Done — see Part 2's "As built". Needed an unplanned Cranelift upgrade and
   an unplanned conservative region (`RuntimeRoots`/`push_scanned_span`) for the two places
   precise roots cannot reach by construction.
3. ~~**Fix the open frontend findings**~~ (`all_roots` indexing, `func_mut_params` scoping,
   `state.rs:324`'s zero-leaf slice) — done, in the commit between Parts 1 and 2.
4. **The IR**, when effects, concurrency, or refinement types arrive. Not speculatively.

Stage 6a of `MUTABILITY.md` is retired by step 2 and should not be implemented.

## Open questions

- ~~**Where does the tag live when a union has both pointer and scalar leaves in different
  members?**~~ Answered: the first pointer column, whose remaining bits stay zero for a member
  with no pointer fields. No reader consults a scalar column it did not write —
  `member_slot_map` drives both `pack_union_member` and `unpack_union_member` from the same
  partition.
- ~~**Nested unions.**~~ Answered, and it cost a slot: see "As built". A union leaf is not
  overlay-safe, so a union nested inside a union member forces `dedicated_tag`. Recursion
  terminates because a self-referential union boxes.
- ~~**Does the 6-member ceiling bite anywhere real?**~~ Nothing in the tree exceeds four, so
  the ceiling does not bite — but the fallback is now reachable and tested rather than merely
  present (`a_seven_member_union_falls_back_to_boxing`).
- ~~**Is `preserve_frame_pointers` enough on aarch64**~~ Answered: yes, paired with
  `-C force-frame-pointers=yes` on the Rust side, which the JIT-only flag doesn't cover. No
  explicit frame list needed.
- **The list-stride overhead gap** (7.5 ns/elem against Rust's 2.6) is unexplained and larger
  than any representation choice here. Worth profiling before optimizing width further.
