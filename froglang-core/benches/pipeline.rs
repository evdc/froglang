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
let total = 0
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
let sink = 0
for i in 0..20000 do {
    let s = label(i)
    let xs = [i, i + 1, i + 2]
    sink = sink + xs[2]
}
sink
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

// ── benchmark groups ─────────────────────────────────────────────────────────

fn bench_compile(c: &mut Criterion) {
    let wide = wide_program();
    let mut group = c.benchmark_group("compile");
    for (name, src) in [("fib", FIB), ("orders", ORDERS), ("alloc", ALLOC), ("wide", wide.as_str())] {
        group.bench_with_input(BenchmarkId::from_parameter(name), src, |b, src| {
            b.iter(|| compile_only(black_box(src)))
        });
    }
    group.finish();
}

fn bench_end_to_end(c: &mut Criterion) {
    let mut group = c.benchmark_group("end_to_end");
    // `wide` is deliberately absent: it exists to stress the front end, and
    // running it would only add a constant.
    for (name, src) in [("fib", FIB), ("orders", ORDERS), ("alloc", ALLOC)] {
        group.bench_with_input(BenchmarkId::from_parameter(name), src, |b, src| {
            b.iter(|| black_box(compile_and_run(black_box(src))))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compile, bench_end_to_end);
criterion_main!(benches);
