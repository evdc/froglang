//! Regression tests for how a froglang program *dies*.
//!
//! Every runtime fault a program can trigger must report what went wrong and
//! exit cleanly with status 1. Three of them used not to: `panic(msg)` and
//! `!`'s unwrap failure emitted a Cranelift `trap` after printing (SIGILL,
//! exit 132), and integer division by zero emitted no check at all, so
//! `sdiv` itself trapped — killing the process with *no* diagnostic
//! whatsoever. Only the list-bounds path did the right thing, and these
//! tests pin all four to it.
//!
//! These have to run the compiled binary as a subprocess: an exit status
//! isn't observable from an in-process `compile_and_run`, and the whole
//! point of the fix is the exit status.

mod common;
use common::run_raw;

/// Assert `src` aborts with status 1 and a stderr message containing `needle`.
fn assert_aborts_with(src: &str, needle: &str) {
    let out = run_raw(src);
    assert_eq!(
        out.status,
        Some(1),
        "expected a clean exit(1) abort, got {:?} (132 means it died on a trap/SIGILL)\
         \n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status, out.stdout, out.stderr,
    );
    assert!(
        out.stderr.contains(needle),
        "expected stderr to mention {:?}, got:\n{}",
        needle, out.stderr,
    );
}

#[test]
fn divide_by_zero_literal_aborts_cleanly() {
    assert_aborts_with("print(1 / 0)", "division by zero");
}

#[test]
fn divide_by_zero_through_a_variable_aborts_cleanly() {
    // The literal case could in principle be caught by constant folding;
    // this one can only be caught by the emitted guard.
    assert_aborts_with("let d = 0\nprint(10 / d)", "division by zero");
}

#[test]
fn int_min_divided_by_negative_one_aborts_cleanly() {
    // The other input `sdiv` faults on: the true quotient is one past
    // Int.MAX. Written as an expression because the lexer has no
    // Int.MIN literal (`-9223372036854775808` overflows while lexing the
    // magnitude).
    assert_aborts_with(
        "let lo = 0 - 9223372036854775807 - 1\nprint(lo / -1)",
        "division overflow",
    );
}

#[test]
fn float_division_by_zero_is_infinity_not_an_abort() {
    // IEEE division never faults, so the guard must be on the integer path
    // only — emitting it for floats would turn a legal program into an abort.
    assert_eq!(run_raw("print(1.0 / 0.0)").status, Some(0));
}

#[test]
fn ordinary_division_still_works() {
    let out = run_raw("print(7 / 2)\nprint(-7 / 2)");
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "3\n-3\n");
}

#[test]
fn explicit_panic_aborts_cleanly() {
    assert_aborts_with(r#"panic("boom")"#, "boom");
}

#[test]
fn unwrap_of_an_error_value_aborts_cleanly() {
    assert_aborts_with(
        r#"
data E(msg: Str) provides Error
func f(): Int | E = E(msg="nope")
let x = f()!
print(x)
"#,
        "unwrapped an error value",
    );
}

#[test]
fn list_index_out_of_bounds_aborts_cleanly() {
    assert_aborts_with("let xs = [1]\nprint(xs[5])", "out of bounds");
}
