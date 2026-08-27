//! Type annotations flowing *down* into the expression they annotate, and
//! the numeric promotions that happen when a value lands in a wider slot.
//!
//! Both used to be broken in ways that only showed up together:
//!
//!   - An annotation was checked bottom-up: infer the expression, then
//!     compare. That can't type a list literal at all — `[]` infers
//!     `List<~t0>`, which no subtype check can bind to `List<Str>` — so
//!     `let xs: List<Str> = []` was rejected outright.
//!   - `widens_to` (Int -> Float) was accepted for struct fields and call
//!     arguments but not for annotations or declared return types, so
//!     `let x: Float = 1` failed while `g(1)` for `func g(w: Float)`
//!     succeeded. Worse, the struct-field path that *did* accept it never
//!     lowered the promotion, so `data P(w: Float)` + `P(w=1)` type-checked
//!     and then crashed the Cranelift verifier — an `i64` written into an
//!     `f64` slot.
//!
//! `check`/`lower_expected` now push the expectation down, and
//! `lower_widen` inserts a `TypedExprKind::Coerce` on every path.

mod common;
use common::{run, run_raw};

use froglang_core::codegen::compile_and_run;

// ── annotations push down into list literals ─────────────────────────────────

#[test]
fn an_empty_list_takes_its_element_type_from_the_annotation() {
    assert_eq!(run("let xs: List<Str> = []\nprint(xs)"), "[]\n");
}

#[test]
fn a_declared_return_type_types_an_empty_list_body() {
    assert_eq!(run("func f(): List<Str> = []\nprint(f())"), "[]\n");
}

#[test]
fn an_annotated_list_literal_still_type_checks_its_elements() {
    assert_ne!(run_raw("let xs: List<Str> = [1]\n1").status, Some(0));
}

#[test]
fn nested_list_annotations_push_down_recursively() {
    assert_eq!(run("let xs: List<List<Str>> = [[], []]\nprint(xs[0])"), "[]\n");
}

#[test]
fn an_unannotated_list_literal_still_infers_bottom_up() {
    // The push-down must not disturb the ordinary path.
    assert_eq!(compile_and_run("let xs = [4, 5, 6]\nxs[2]"), 6);
    assert_ne!(run_raw("let xs = [1, \"a\"]\n1").status, Some(0));
}

// ── numeric promotion into a wider slot ──────────────────────────────────────

#[test]
fn an_int_promotes_to_an_annotated_float() {
    assert_eq!(run("let x: Float = 1\nprint(x)"), "1.0\n");
}

#[test]
fn an_int_promotes_to_a_declared_float_return_type() {
    assert_eq!(run("func f(): Float = 1\nprint(f())"), "1.0\n");
}

#[test]
fn an_int_promotes_into_a_float_struct_field() {
    // This is the case that used to reach codegen mistyped and fail the
    // Cranelift verifier.
    assert_eq!(run("data P(w: Float)\nprint(P(w=1))"), "P(w=1.0)\n");
}

#[test]
fn an_int_promotes_into_a_float_list_element() {
    assert_eq!(run("let xs: List<Float> = [1, 2]\nprint(xs)"), "[1.0, 2.0]\n");
}

#[test]
fn a_promoted_value_really_is_a_float_at_runtime() {
    // Printing alone could be masking an untouched i64; force actual
    // floating-point arithmetic on the promoted value.
    assert_eq!(run("let xs: List<Float> = [1, 2]\nprint(xs[0] + 0.5)"), "1.5\n");
}

#[test]
fn an_int_promotes_into_a_float_function_parameter() {
    // The one path that already worked, pinned so it stays consistent with
    // the others.
    assert_eq!(run("func g(w: Float): Float = w\nprint(g(1))"), "1.0\n");
}

#[test]
fn promotion_is_one_directional() {
    // Float -> Int is lossy and must stay rejected everywhere.
    assert_ne!(run_raw("let x: Int = 1.5\n1").status, Some(0));
    assert_ne!(run_raw("func f(): Int = 1.5\n1").status, Some(0));
    assert_ne!(run_raw("data P(w: Int)\nlet p = P(w=1.5)\n1").status, Some(0));
}

// ── float rendering ──────────────────────────────────────────────────────────

#[test]
fn floats_render_the_same_inside_a_list_as_outside_one() {
    // `frog_list_print` used Rust's `Display` for f64, which renders 1.0 as
    // "1" — so a `List<Float>` printed identically to a `List<Int>` while
    // the scalar path (`{:?}`) printed "1.0". Same value, two spellings.
    assert_eq!(run("print(1.0)\nprint([1.0, 2.5])"), "1.0\n[1.0, 2.5]\n");
}
