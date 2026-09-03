//! In-process benchmarks for froglang's own compile and run phases.
//!
//! `run_comparisons.sh` answers a different question — "how does froglang
//! compare to Rust/Go/Lua/Python on the same program?" — by timing whole
//! processes. That's the right shape for a cross-language table and the
//! wrong shape for spotting a regression between two froglang commits: one
//! wall-clock number per language per run, no baseline, no statistics, and
//! process startup and JIT compilation folded into the same figure as the
//! program's actual execution.
//!
//! These fill that gap. Criterion keeps a saved baseline, so
//! `cargo bench -p froglang-core` reports each measurement against the last
//! run and calls out a change as an improvement or a regression. The
//! `orders` regression that doubled its wall time (182ms -> 441ms) when the
//! hot path was wrapped in `?`/`catch` would have shown up here the moment
//! it landed.
//!
//! Each workload is measured twice:
//!
//!   * `compile/*` — parse, resolve modules, type-check and lower. The front
//!     end only; no Cranelift, no execution. This is what a `frog check`
//!     costs, and it's where a front-end complexity regression shows up as a
//!     large multiplier (`tests/test_compile_scaling.rs` guards the
//!     catastrophic end of that; this measures the everyday end).
//!   * `end_to_end/*` — the same source through `compile_and_run`: front
//!     end, Cranelift codegen, and execution. Subtract the `compile` figure
//!     to attribute a change to the back end or the runtime rather than the
//!     front end.
//!
//! Sizes are tuned so each workload lands in the milliseconds — big enough
//! to dominate measurement noise, small enough that a full run finishes in
//! a reasonable time. `orders` in particular runs a fraction of the
//! iterations `benches/orders.frog` does, so the two numbers are not
//! comparable to each other; only to their own history.
//!
//! The workloads are chosen to isolate *different* subsystems, so a
//! regression points at a culprit rather than just "something got slower".
//! Between them they cover: raw call/arithmetic throughput with no heap at
//! all (`fib`), unboxed struct field access and copying (`structs`), the
//! allocator and collector under pure churn (`alloc`) and under a large
//! *retained* live set that every collection must re-mark (`gc_pressure`),
//! string building and comparison (`strings`), list allocation, growth and
//! indexing (`lists`), recursive boxed unions and `match` dispatch (`tree`),
//! and everything at once (`orders`).
//!
//! `fallible`/`infallible` are a deliberate pair: the same arithmetic
//! pipeline, once written plainly and once threaded through `?`/`catch`
//! over an `Int | Bad` union. Their *ratio* is the cost of froglang's error
//! propagation, and it is the number to watch — wrapping `orders`'s hot
//! path in `?`/`catch` once doubled its wall time even though the error
//! branch was never taken, and nothing in the suite would have localised
//! that to error handling rather than to structs, lists or the GC.
//!
//! `structs`/`structs_mut_param` and `lists`/`list_mut_index` are the same
//! pairing applied to MUTABILITY.md: each pair runs the identical
//! computation once the old return-and-rebind way and once through the
//! `mut`-parameter / place-assignment machinery that stage added. Neither
//! pair should show a *regression* — `mut` write-back is currently
//! copy-in/copy-out at the ABI boundary, no cheaper than returning a fresh
//! value — but their ratio is the baseline to compare against once
//! move-on-last-use or in-place `mut` writes (MUTABILITY.md §4) land: it
//! should shrink toward 1 as copies the compiler can prove are unobserved
//! stop happening.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

use froglang_core::codegen::compile_and_run;
use froglang_core::frontend::expression::Expression;
use froglang_core::frontend::modules;
use froglang_core::frontend::tokens::{Span, Spanned};
use froglang_core::frontend::typeck::TypeChecker;

/// Parse, resolve and type-check `src`, without generating any code.
/// Panics on failure — a benchmark that silently measures the error path
/// would report a meaningless (and suspiciously fast) number.
fn compile_only(src: &str) {
    let base = std::env::temp_dir().join("<bench>");
    let stmts = modules::resolve_source(src, &base).expect("benchmark program must parse");
    let span = match (stmts.first(), stmts.last()) {
        (Some(f), Some(l)) => f.span.merge(l.span),
        _ => Span::new((0, 0), (0, 0)),
    };
    let ast = Spanned::from(Expression::Block(stmts), span);
    let typed = TypeChecker::new()
        .check_and_lower(ast)
        .expect("benchmark program must type-check");
    black_box(typed);
}

// ── workloads ────────────────────────────────────────────────────────────────

/// Naive recursion, zero allocation: isolates raw call and arithmetic
/// throughput from anything the GC does.
const FIB: &str = "\
func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)
fib(24)
";

/// Structs, nominal unions with `match`, list comprehensions, and `?`/`catch`
/// error propagation — the same program shape as `benches/orders.frog`, at
/// 1/40th the iteration count so it fits a criterion sample.
const ORDERS: &str = "\
data Category is Food | Book | Electronics | Toy
data Item(sku: Int, category: Category, qty: Int, unit_price: Int)
data Discount is NoDiscount | Percent(pct: Int) | Flat(amount: Int) | BulkOver(min_qty: Int, pct: Int)
data PricingError(msg: Str) provides Error

func modn(x: Int, n: Int): Int = x - (x / n) * n
func hash(i: Int): Int = modn(i * 2654435761 + 1013904223, 2147483647)

func category_of(h: Int): Category = {
    let k = modn(h, 4)
    if k == 0 then Food else if k == 1 then Book else if k == 2 then Electronics else Toy
}

func discount_for(it: Item): Discount = match it.category {
    is Food then if it.qty >= 6 then BulkOver(min_qty=6, pct=5) else NoDiscount
    is Book then Percent(pct=10)
    is Electronics then if it.unit_price > 3000 then Flat(amount=250) else Percent(pct=3)
    is Toy then BulkOver(min_qty=3, pct=15)
}

func apply(d: Discount, gross: Int, qty: Int): Int = match d {
    is NoDiscount then gross
    is Percent(pct) then gross - gross * pct / 100
    is Flat(amount) then if gross > amount then gross - amount else 0
    is BulkOver(min_qty, pct) then if qty >= min_qty then gross - gross * pct / 100 else gross
}

func checked_gross(gross: Int): Int | PricingError =
    if gross < 0 then PricingError(msg=\"negative gross\") else gross

func price_item(it: Item): Int | PricingError = {
    let gross = checked_gross(it.qty * it.unit_price)?
    apply(discount_for(it), gross, it.qty)
}

let items = [for i in 0..400 do Item(sku=i, category=category_of(hash(i)), qty=1 + modn(hash(i + 7), 9), unit_price=100 + modn(hash(i + 13), 5000))]
mut total = 0
for round in 0..100 do {
    let batch = [for it in items if modn(it.sku + round, 3) != 0 do it]
    for it in batch do { total = total + (price_item(it) catch 0) }
}
total
";

/// Allocation-dominated: every iteration builds a fresh string and a fresh
/// list, so this is really a measurement of `frog_alloc_*` plus the
/// mark-sweep collector and the shadow-stack rooting around them. The
/// counterpart to `fib`, which never touches the heap at all.
const ALLOC: &str = "\
func label(i: Int): Str = \"item-\" + \"x\"
mut sink = 0
for i in 0..20000 do {
    let s = label(i)
    let xs = [i, i + 1, i + 2]
    sink = sink + xs[2]
}
sink
";

/// Unboxed structs and nothing else: nested `data` values passed by value
/// into and out of functions, rebound field by field, with no allocation
/// anywhere. Struct values are flattened into one SSA value per leaf field
/// rather than boxed (`struct_fields` in codegen), so this should track
/// `fib` closely — if it drifts away from `fib`, struct passing has stopped
/// being free.
const STRUCTS: &str = "\
data Vec3(x: Int, y: Int, z: Int)
data Body(pos: Vec3, vel: Vec3, mass: Int)
func advance(p: Vec3, v: Vec3): Vec3 = Vec3(x=p.x + v.x, y=p.y + v.y, z=p.z + v.z)
func step(b: Body): Body = {
    mut r = b
    r.pos = advance(r.pos, r.vel)
    r
}
func energy(b: Body): Int = b.mass * (b.vel.x * b.vel.x + b.vel.y * b.vel.y + b.vel.z * b.vel.z)
mut b = Body(pos=Vec3(x=0, y=0, z=0), vel=Vec3(x=1, y=2, z=3), mass=7)
mut total = 0
for i in 0..500000 do {
    b = step(b)
    total = total + energy(b)
}
total + b.pos.x
";

/// `STRUCTS`'s copy-elision baseline pair (MUTABILITY.md stage 4): the
/// identical `Body`-advancing computation, but `step` writes into the
/// caller's `b` through a `mut` parameter instead of returning a fresh
/// `Body` for the caller to rebind. `advance` is inlined into `step` rather
/// than called separately, because a `mut` argument must be a plain
/// binding — `b.pos` isn't one, so there's no way to hand the nested field
/// to a second `mut`-taking function. Semantically this differs from
/// `STRUCTS` only in *how* the write happens; today `mut` parameters are
/// copy-in/copy-out at the ABI boundary (`compile_call`'s "copy-out" pass in
/// codegen/mod.rs), so this should currently track `STRUCTS` closely. The
/// gap between them is exactly what move-on-last-use / in-place `mut`
/// writes (MUTABILITY.md §4, tier 2) should close — watch this ratio move
/// toward 1 as that lands.
const STRUCTS_MUT_PARAM: &str = "\
data Vec3(x: Int, y: Int, z: Int)
data Body(pos: Vec3, vel: Vec3, mass: Int)
func step(mut b: Body): None = {
    b.pos.x = b.pos.x + b.vel.x
    b.pos.y = b.pos.y + b.vel.y
    b.pos.z = b.pos.z + b.vel.z
}
func energy(b: Body): Int = b.mass * (b.vel.x * b.vel.x + b.vel.y * b.vel.y + b.vel.z * b.vel.z)
mut b = Body(pos=Vec3(x=0, y=0, z=0), vel=Vec3(x=1, y=2, z=3), mass=7)
mut total = 0
for i in 0..500000 do {
    step(mut b)
    total = total + energy(b)
}
total + b.pos.x
";

/// String building and comparison: every iteration concatenates several
/// short strings into a fresh one and compares it against a literal. Each
/// `+` is a `frog_str_concat` — a GC allocation plus a copy of both
/// operands — so this is the workload that notices a change to string
/// representation or to concat's own buffering.
const STRINGS: &str = "\
func modn(x: Int, n: Int): Int = x - (x / n) * n
func digit(d: Int): Str =
    if d == 0 then \"0\" else if d == 1 then \"1\" else if d == 2 then \"2\"
    else if d == 3 then \"3\" else if d == 4 then \"4\" else if d == 5 then \"5\"
    else if d == 6 then \"6\" else if d == 7 then \"7\" else if d == 8 then \"8\" else \"9\"
func render(n: Int): Str = digit(modn(n / 100, 10)) + digit(modn(n / 10, 10)) + digit(modn(n, 10))
mut hits = 0
for i in 0..8000 do {
    let key = \"id-\" + render(i) + \"/\" + render(i + 1)
    if key == \"id-000/001\" then hits = hits + 1 else hits = hits
}
hits
";

/// Lists specifically: a fresh 1500-element comprehension per round (which
/// grows its backing store by repeated doubling from a capacity of one),
/// then a scattered read pass over it. Separates list allocation, growth
/// and bounds-checked indexing from the struct/union work `orders` mixes
/// them with.
const LISTS: &str = "\
func modn(x: Int, n: Int): Int = x - (x / n) * n
mut total = 0
for round in 0..500 do {
    let xs = [for i in 0..1500 do i * 3 + round]
    mut s = 0
    for j in 0..1500 do { s = s + xs[modn(j * 7 + round, 1500)] }
    total = total + modn(s, 1000003)
}
total
";

/// `LISTS`'s mutability counterpart: one list allocated once, then mutated
/// in place through index assignment (`xs[j] = ...`) every round instead of
/// rebuilt fresh via a comprehension. List index assignment (`PlaceAssign`
/// with one `[index]` step, MUTABILITY.md stage 3) didn't exist before this
/// round of changes — "no user-facing list mutation exists" per that doc's
/// table — so there's no pre-mutability baseline to compare against; this
/// establishes one for the write path itself (`frog_list_set`, called once
/// per leaf field per write) ahead of any future inlining of it the way
/// list *reads* were already inlined (see README, "Inline heap access").
/// Any gap against `LISTS`'s per-element cost that isn't explained by
/// skipping the comprehension's own allocation is that path's overhead.
const LIST_MUT_INDEX: &str = "\
func modn(x: Int, n: Int): Int = x - (x / n) * n
mut xs = [for i in 0..1500 do i]
mut total = 0
for round in 0..500 do {
    for j in 0..1500 do { xs[j] = modn(xs[j] + round * 7 + 1, 1000003) }
    total = total + xs[modn(round * 13, 1500)]
}
total
";

/// `LISTS`'s `push` counterpart: the same 1500-element-per-round shape, but
/// built by `push`ing into a `mut` binding one element at a time instead of
/// a comprehension. `push`'s own receiver is always `Move`-classified
/// (`liveness.rs`'s `Call`/`mut_args` handling — see codegen's `Ctx::liveness`
/// doc comment), so this should cost about what `LISTS`' comprehension build
/// costs, not that plus a per-`push` clone: MUTABILITY.md stage 6's elision
/// claim, made measurable.
const LIST_PUSH_LOOP: &str = "\
func modn(x: Int, n: Int): Int = x - (x / n) * n
mut total = 0
for round in 0..500 do {
    mut xs = []
    for i in 0..1500 do push(mut xs, i * 3 + round)
    mut s = 0
    for j in 0..1500 do { s = s + xs[modn(j * 7 + round, 1500)] }
    total = total + modn(s, 1000003)
}
total
";

/// `LIST_PUSH_LOOP` with one genuine alias per round: `ys` binds the fully
/// built list, `xs` is read again afterward, so that bind must clone —
/// exactly the correctness half of the same story. Should show one
/// measurable, once-per-round clone cost on top of `LIST_PUSH_LOOP`, not a
/// per-element one; if it doesn't show up at all, the clone is being wrongly
/// elided, not correctly avoided.
const LIST_PUSH_ALIASED: &str = "\
func modn(x: Int, n: Int): Int = x - (x / n) * n
mut total = 0
for round in 0..500 do {
    mut xs = []
    for i in 0..1500 do push(mut xs, i * 3 + round)
    mut ys = xs
    push(mut ys, 999999)
    mut s = 0
    for j in 0..1500 do { s = s + xs[modn(j * 7 + round, 1500)] }
    total = total + modn(s + ys[1500], 1000003)
}
total
";

/// Recursive boxed unions: build a complete binary `Tree` and walk it with
/// `match`. `Tree` is self-referential, so it is one of the two shapes
/// `codegen::union_is_inline` refuses to flatten and it keeps the heap-boxed
/// `FrogVariant` representation — which is the point: this is the *boxed*
/// half of the union story, and measures variant allocation, tag dispatch and
/// destructuring on a deep object graph — and, because each tree stays
/// wholly reachable while it is summed, a mark phase that has to chase
/// pointers rather than scan a flat list.
const TREE: &str = "\
data Tree is Leaf | Node(v: Int, l: Tree, r: Tree)
func build(depth: Int, v: Int): Tree =
    if depth == 0 then Leaf else Node(v=v, l=build(depth - 1, v * 2), r=build(depth - 1, v * 2 + 1))
func sum(t: Tree): Int = match t {
    is Leaf then 0
    is Node(v, l, r) then v + sum(l) + sum(r)
}
mut total = 0
for i in 0..40 do { total = total + sum(build(12, i)) }
total
";

/// Collector scaling against a large *retained* live set. `live` is a
/// 4000-cell chain that stays reachable for the whole run, so every
/// collection triggered by the garbage in the loop below must re-mark all
/// of it. `alloc` measures allocation with almost nothing live; this
/// measures the mark phase, which is the half that grows with the heap
/// rather than with the allocation rate — a non-generational collector's
/// characteristic cliff.
const GC_PRESSURE: &str = "\
data Node is Empty | Cell(v: Int, rest: Node)
func chain(n: Int, acc: Node): Node = if n == 0 then acc else chain(n - 1, Cell(v=n, rest=acc))
func total(t: Node): Int = match t {
    is Empty then 0
    is Cell(v, rest) then v + total(rest)
}
let live = chain(4000, Empty)
mut sink = 0
for i in 0..400 do {
    let garbage = chain(200, Empty)
    sink = sink + total(garbage)
}
sink + total(live)
";

/// The control half of the `?`/`catch` pair: a plain arithmetic pipeline of
/// ordinary `Int`-returning calls. Identical in shape and result to
/// `FALLIBLE`; the only difference is that nothing here is fallible.
const INFALLIBLE: &str = "\
func modn(x: Int, n: Int): Int = x - (x / n) * n
func scale(x: Int): Int = if x < 0 then 0 else x * 3 + 1
func pipeline(x: Int): Int = scale(scale(x))
mut total = 0
for i in 0..800000 do { total = total + modn(pipeline(i), 1000003) }
total
";

/// The same pipeline with every step returning `Int | Bad`, propagated with
/// `?` and collapsed back to an `Int` with `catch` at the call site. The
/// error branch is never taken, so any gap against `INFALLIBLE` is pure
/// overhead in the mechanism: the union representation, the tag test `?`
/// emits per call, and the `catch` landing pad.
///
/// `Int | Bad` is two members and not self-referential, so it rides in an
/// unboxed `(tag+ptr, scalar)` register pair (`codegen::UnionLayout`) rather
/// than allocating a `FrogVariant` per call, and — being exhaustive with no
/// `else` — costs one tag test per dispatch, not two. As of 2026-09-02 the
/// ratio is ~1.0. A sudden jump in it most likely means some change made
/// that union box again.
const FALLIBLE: &str = "\
data Bad(msg: Str) provides Error
func modn(x: Int, n: Int): Int = x - (x / n) * n
func scale(x: Int): Int | Bad = if x < 0 then Bad(msg=\"negative\") else x * 3 + 1
func pipeline(x: Int): Int | Bad = {
    let a = scale(x)?
    scale(a)
}
mut total = 0
for i in 0..800000 do { total = total + modn(pipeline(i) catch 0, 1000003) }
total
";

/// Front-end-only workload: a wide program with many independent
/// declarations, sized to make parsing and type-checking (rather than
/// execution) the whole cost. Compiled but never run.
fn wide_program() -> String {
    let mut src = String::new();
    for i in 0..200 {
        src.push_str(&format!(
            "data S{i}(a: Int, b: Str)\nfunc f{i}(x: Int): Int = if x > {i} then x - {i} else x + {i}\n"
        ));
    }
    src.push_str("f0(1)\n");
    src
}

/// The other front-end axis: one *deeply* nested expression rather than
/// many shallow declarations. `wide` grows the number of top-level items,
/// which a linear front end handles linearly; this grows the nesting depth
/// of a single expression, which is where a type-checker that re-walks a
/// subtree per level turns quadratic and where the recursion limits bite.
/// Compiled but never run.
fn deep_program() -> String {
    let mut src = String::from("func f(x: Int): Int = x + 1
let y = 1
let z = ");
    for _ in 0..300 {
        src.push_str("f(");
    }
    src.push('y');
    for _ in 0..300 {
        src.push(')');
    }
    src.push_str("
z
");
    src
}

// ── benchmark groups ─────────────────────────────────────────────────────────

/// Every runnable workload, in the order both groups report them.
/// `infallible` sits immediately before `fallible` so their ratio — the
/// cost of `?`/`catch` — is two adjacent lines in the output. Likewise
/// `structs`/`structs_mut_param` and `lists`/`list_mut_index` are adjacent
/// pairs: same computation, return-and-rebind vs `mut`-parameter/place
/// write-back, so their ratio is the copy-elision headroom MUTABILITY.md
/// §4 describes.
fn runnable() -> [(&'static str, &'static str); 14] {
    [
        ("fib", FIB),
        ("structs", STRUCTS),
        ("structs_mut_param", STRUCTS_MUT_PARAM),
        ("alloc", ALLOC),
        ("gc_pressure", GC_PRESSURE),
        ("strings", STRINGS),
        ("lists", LISTS),
        ("list_mut_index", LIST_MUT_INDEX),
        ("list_push_loop", LIST_PUSH_LOOP),
        ("list_push_aliased", LIST_PUSH_ALIASED),
        ("tree", TREE),
        ("infallible", INFALLIBLE),
        ("fallible", FALLIBLE),
        ("orders", ORDERS),
    ]
}

fn bench_compile(c: &mut Criterion) {
    let wide = wide_program();
    let deep = deep_program();
    let mut group = c.benchmark_group("compile");
    // The front end sees the same programs the back end does, plus the two
    // that exist only to stress it.
    let mut cases: Vec<(&str, &str)> = runnable().to_vec();
    cases.push(("wide", wide.as_str()));
    cases.push(("deep", deep.as_str()));
    for (name, src) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), src, |b, src| {
            b.iter(|| compile_only(black_box(src)))
        });
    }
    group.finish();
}

fn bench_end_to_end(c: &mut Criterion) {
    let mut group = c.benchmark_group("end_to_end");
    // `wide` and `deep` are deliberately absent: they exist to stress the
    // front end, and running them would only add a constant.
    for (name, src) in runnable() {
        group.bench_with_input(BenchmarkId::from_parameter(name), src, |b, src| {
            b.iter(|| black_box(compile_and_run(black_box(src))))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compile, bench_end_to_end);
criterion_main!(benches);
