//! Regression/integration tests for struct/nominal-union marshalling in the
//! embedding API (`#[derive(FrogData)]`/`#[derive(FrogUnion)]`,
//! `FrogStateBuilder::data::<T>()`) — `plans/EMBEDDING.md`.
//!
//! `FrogValue` still collapses struct/union results to `None` (Stage 5,
//! not yet built), so these tests observe a struct/union crossing the
//! boundary indirectly: a host function that accepts one and returns a
//! plain scalar derived from its fields, or one that returns one and lets
//! *frog* source destructure it — which also exercises real frog-side
//! `struct_fields`/`union_layout`, not just that the Rust side compiles.

use froglang_core::{frog_fn, host::ToFrog};
use froglang_core::state::FrogState;
use froglang_macros::{FrogData, FrogUnion};

fn int(v: &froglang_core::state::FrogValue) -> i64 {
    match v {
        froglang_core::state::FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}

// ── Structs ──────────────────────────────────────────────────────────────

#[derive(FrogData, Clone)]
struct Point { x: i64, y: i64 }

#[frog_fn]
fn point_sum(p: Point) -> i64 { p.x + p.y }

#[frog_fn]
fn make_point(x: i64, y: i64) -> Point { Point { x, y } }

#[test]
fn struct_argument_round_trips() {
    let mut s = FrogState::builder()
        .data::<Point>()
        .func(point_sum_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("point_sum(Point(x=3, y=4))").unwrap();
    assert_eq!(int(&v), 7);
}

#[test]
fn struct_return_is_readable_from_frog() {
    let mut s = FrogState::builder()
        .data::<Point>()
        .func(make_point_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("let p = make_point(3, 4)\np.x + p.y").unwrap();
    assert_eq!(int(&v), 7);
}

// A nested struct field: proves the flattened multi-leaf path, not just a
// one-field wrapper like `ErrMsg`.
#[derive(FrogData, Clone)]
struct Line { a: Point, b: Point }

#[frog_fn]
fn line_len_sq(l: Line) -> i64 {
    let dx = l.b.x - l.a.x;
    let dy = l.b.y - l.a.y;
    dx * dx + dy * dy
}

#[test]
fn nested_struct_field_flattens_correctly() {
    let mut s = FrogState::builder()
        .data::<Point>()
        .data::<Line>()
        .func(line_len_sq_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("line_len_sq(Line(a=Point(x=0, y=0), b=Point(x=3, y=4)))").unwrap();
    assert_eq!(int(&v), 25);
}

#[frog_fn]
fn sum_points(pts: Vec<Point>) -> i64 {
    pts.into_iter().map(|p| p.x + p.y).sum()
}

#[test]
fn vec_of_structs_round_trips_with_correct_stride_and_mask() {
    let mut s = FrogState::builder()
        .data::<Point>()
        .func(sum_points_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("sum_points([Point(x=1, y=2), Point(x=3, y=4)])").unwrap();
    assert_eq!(int(&v), 10);
    // Force a collection between allocating the list/points and the host
    // call reading them back, to prove the per-leaf pointer mask actually
    // roots every heap-typed leaf (there are none here — Point is all
    // `Int` — but this also stands in as the "doesn't crash" smoke test
    // for the stride math itself).
    s.heap.force_collect();
}

// A positional ("tuple struct") field list — frog's `Lit(5)` construction
// syntax. Frog source can construct one but can't read `.0` back
// (`test_positional_fields.rs`'s documented gap), so this only exercises
// the argument direction, where the host side reads the field in Rust.
#[derive(FrogData, Clone)]
struct Lit(i64);

#[frog_fn]
fn lit_value(l: Lit) -> i64 { l.0 }

#[test]
fn positional_struct_argument_round_trips() {
    let mut s = FrogState::builder()
        .data::<Lit>()
        .func(lit_value_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("lit_value(Lit(5))").unwrap();
    assert_eq!(int(&v), 5);
}

// ── Nominal unions ───────────────────────────────────────────────────────

#[derive(FrogUnion, Clone)]
enum Shape {
    Circle { r: i64 },
    Rect { w: i64, h: i64 },
}

#[frog_fn]
fn area(s: Shape) -> i64 {
    match s {
        Shape::Circle { r } => r * r,
        Shape::Rect { w, h } => w * h,
    }
}

#[frog_fn]
fn classify(n: i64) -> Shape {
    if n > 0 { Shape::Circle { r: n } } else { Shape::Rect { w: 1, h: 1 } }
}

#[test]
fn union_argument_round_trips_through_both_variants() {
    let mut s = FrogState::builder()
        .data::<Shape>()
        .func(area_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("area(Shape.Circle(r=3))").unwrap();
    assert_eq!(int(&v), 9);
    let (v, _) = s.eval("area(Shape.Rect(w=4, h=5))").unwrap();
    assert_eq!(int(&v), 20);
}

#[test]
fn union_return_is_matchable_from_frog() {
    let mut s = FrogState::builder()
        .data::<Shape>()
        .func(classify_host())
        .build()
        .unwrap();
    let src = "match classify(5) {\nis Shape.Circle(r) then r\nis Shape.Rect(w, h) then w * h\n}";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(int(&v), 5);
    let src = "match classify(-1) {\nis Shape.Circle(r) then r\nis Shape.Rect(w, h) then w * h\n}";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(int(&v), 1);
}

// ── Option<T> / Result<T, E> ─────────────────────────────────────────────

#[frog_fn]
fn maybe_positive(n: i64) -> Option<i64> {
    if n > 0 { Some(n) } else { None }
}

#[test]
fn option_round_trips_both_arms() {
    let mut s = FrogState::builder().func(maybe_positive_host()).build().unwrap();
    let src = "match maybe_positive(5) {\nis Int(n) then n\nis None then -1\n}";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(int(&v), 5);
    let src = "match maybe_positive(-5) {\nis Int(n) then n\nis None then -1\n}";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(int(&v), -1);
}

#[derive(FrogData, Clone)]
#[frog(error)]
struct OutOfRange { requested: i64 }

#[frog_fn]
fn checked_double(n: i64) -> Result<i64, OutOfRange> {
    if n > 1000000 {
        Err(OutOfRange { requested: n })
    } else {
        Ok(n * 2)
    }
}

#[test]
fn result_with_a_derived_error_type_round_trips() {
    let mut s = FrogState::builder()
        .data::<OutOfRange>()
        .func(checked_double_host())
        .build()
        .unwrap();
    let (v, _) = s.eval("checked_double(21)!").unwrap();
    assert_eq!(int(&v), 42);
    let (v, _) = s.eval("checked_double(2000000) catch [e] -> e.requested").unwrap();
    assert_eq!(int(&v), 2000000);
}

// ── `build()`'s layout audit ─────────────────────────────────────────────

/// A `ToFrog`/`FrogDecl` impl whose `leaves()` deliberately disagrees with
/// what its own `frog_decl()` would register — the class of bug the derive
/// exists to make impossible, reproduced by hand to prove `build()` catches
/// it rather than corrupting slots silently at the first call.
struct Mismatched;
impl ToFrog for Mismatched {
    fn frog_type() -> froglang_core::frontend::typeck::Type {
        froglang_core::frontend::typeck::Type::strukt("Mismatched")
    }
    fn leaves() -> Vec<froglang_core::frontend::typeck::Type> {
        vec![froglang_core::frontend::typeck::Type::Int, froglang_core::frontend::typeck::Type::Int]
    }
    fn to_frog(self, _ctx: &mut froglang_core::runtime::host::FrogCtx, _out: &mut [i64]) {}
}
impl froglang_core::host::FrogDecl for Mismatched {
    fn frog_decl() -> Option<String> {
        Some("data Mismatched(only_one: Int)".to_string())
    }
}

#[test]
fn build_audit_catches_a_leaves_layout_mismatch() {
    let result = FrogState::builder().data::<Mismatched>().build();
    let msg = match result {
        Ok(_) => panic!("expected build() to reject the mismatched layout"),
        Err(e) => format!("{:?}", e),
    };
    assert!(msg.contains("Mismatched"), "error should name the offending type: {}", msg);
    assert!(msg.contains("disagree"), "error should say what's wrong: {}", msg);
}

/// A multi-variant error union used as `Result`'s `E`. `Type::normalize`
/// flattens it into the surrounding union, so `Result<i64, Denial>` is a
/// *three*-member frog union — a shape `Result`'s impl can't address, and
/// used to mis-marshal silently (wrong column offsets both directions, and
/// a `leaves()` longer than the slot buffer the shim writes into).
#[derive(FrogUnion, Clone)]
#[frog(error)]
enum Denial {
    Denied { code: i64 },
    NotFound { code: i64 },
}

#[frog_fn]
fn lookup(n: i64) -> Result<i64, Denial> {
    if n > 0 { Ok(n * 2) } else { Err(Denial::NotFound { code: n }) }
}

#[test]
fn result_over_a_union_error_is_rejected_not_mis_marshalled() {
    let err = std::panic::catch_unwind(lookup_host)
        .err()
        .expect("expected Result<_, Denial> to be rejected: its E is itself a union");
    let msg = err.downcast_ref::<String>().map(|s| s.as_str()).unwrap_or("");
    assert!(msg.contains("Result<T, E>"), "error should name the impl: {}", msg);
    assert!(msg.contains("normalize"), "error should explain the flattening: {}", msg);
}

/// A hand-written pair whose `leaves()` disagrees with the frog declaration
/// it maps onto, reachable *only* through a host function's signature —
/// never registered via `.data::<T>()`, so only the signature audit can
/// catch it.
struct SigOnly;
impl froglang_core::host::FromFrog for SigOnly {
    fn frog_type() -> froglang_core::frontend::typeck::Type {
        froglang_core::frontend::typeck::Type::strukt("SigOnly")
    }
    fn leaves() -> Vec<froglang_core::frontend::typeck::Type> {
        vec![froglang_core::frontend::typeck::Type::Int, froglang_core::frontend::typeck::Type::Int]
    }
    fn from_frog(_ctx: &froglang_core::runtime::host::FrogCtx, _slots: &[i64]) -> Self { SigOnly }
}

#[frog_fn]
fn sig_only_len(_v: SigOnly) -> i64 { 0 }

#[test]
fn build_audit_catches_a_signature_only_layout_mismatch() {
    let result = FrogState::builder()
        .prelude("data SigOnly(only_one: Int)")
        .func(sig_only_len_host())
        .build();
    let msg = match result {
        Ok(_) => panic!("expected build() to reject the mismatched parameter layout"),
        Err(e) => format!("{:?}", e),
    };
    assert!(msg.contains("sig_only_len"), "error should name the host function: {}", msg);
    assert!(msg.contains("parameter 0"), "error should name the offending parameter: {}", msg);
    assert!(msg.contains("disagree"), "error should say what's wrong: {}", msg);
}
