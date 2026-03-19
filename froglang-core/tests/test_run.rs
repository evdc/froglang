use froglang_core::codegen::compile_and_run;

// ── helper: load a .frog program from tests/programs/ ────────────────────────

fn prog(name: &str) -> String {
    let path = format!("{}/tests/programs/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {}: {}", path, e))
}

// ── fib (existing) ────────────────────────────────────────────────────────────

const FIB: &str =
    "func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)";

fn fib_src(call: &str) -> String { format!("{}\n{}", FIB, call) }

#[test] fn test_fib_base_case_0()  { assert_eq!(compile_and_run(&fib_src("fib(0)")),  0); }
#[test] fn test_fib_base_case_1()  { assert_eq!(compile_and_run(&fib_src("fib(1)")),  1); }
#[test] fn test_fib_correctness()  { assert_eq!(compile_and_run(&fib_src("fib(10)")), 55); }

// ── inline basics ────────────────────────────────────────────────────────────

#[test] fn test_int_literal()  { assert_eq!(compile_and_run("42"),        42); }
#[test] fn test_arithmetic()   { assert_eq!(compile_and_run("3 + 4 * 2"), 11); }

// ── programs from tests/programs/ ────────────────────────────────────────────

/// Single recursive function, multiplication.
/// fact(10) = 10! = 3628800
#[test]
fn test_factorial() {
    assert_eq!(compile_and_run(&prog("factorial.frog")), 3_628_800);
}

/// Simple linear recursion / accumulation.
/// sum(100) = 1 + 2 + … + 100 = 5050
#[test]
fn test_sum_to_n() {
    assert_eq!(compile_and_run(&prog("sum_to_n.frog")), 5050);
}

/// Two-parameter recursion.
/// pow(2, 20) = 2^20 = 1048576
#[test]
fn test_power() {
    assert_eq!(compile_and_run(&prog("power.frog")), 1_048_576);
}

/// Euclidean GCD using integer division to compute a mod b.
/// gcd(1071, 462) = 21
#[test]
fn test_gcd() {
    assert_eq!(compile_and_run(&prog("gcd.frog")), 21);
}

/// Nested if-else, even/odd check via integer division.
/// collatz(27) visits 111 steps before reaching 1.
#[test]
fn test_collatz() {
    assert_eq!(compile_and_run(&prog("collatz.frog")), 111);
}

/// Functions calling other named functions; 3-argument function.
/// clamp(150, 0, 100) = 100
#[test]
fn test_multi_func() {
    assert_eq!(compile_and_run(&prog("multi_func.frog")), 100);
}

/// Ackermann function: a call expression used directly as a call argument.
/// ack(3, 4) = 2^7 - 3 = 125
#[test]
fn test_ackermann() {
    assert_eq!(compile_and_run(&prog("ackermann.frog")), 125);
}

/// Let bindings only — no functions.
/// a=3, b=4, c = a²+b² = 25  (Pythagorean triple check)
#[test]
fn test_let_bindings() {
    assert_eq!(compile_and_run(&prog("let_bindings.frog")), 25);
}

/// Named functions alongside top-level let bindings in the same block.
/// cube(7) = 7³ = 343
#[test]
fn test_func_and_let() {
    assert_eq!(compile_and_run(&prog("func_and_let.frog")), 343);
}

/// Multi-line function body and multi-line if-then-else.
/// fib(10) = 55
#[test]
fn test_fib_multiline() {
    assert_eq!(compile_and_run(&prog("fib_multiline.frog")), 55);
}

/// Block syntax: multi-statement body with newline separators.
/// hypotenuse(3, 4) = 3² + 4² = 25
#[test]
fn test_block_func() {
    assert_eq!(compile_and_run(&prog("block_func.frog")), 25);
}

/// Block syntax: single-line block with semicolons.
/// clamp_pos(-5) = 0
#[test]
fn test_block_inline() {
    assert_eq!(compile_and_run(&prog("block_inline.frog")), 0);
}
