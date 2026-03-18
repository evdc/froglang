use froglang_core::codegen::compile_and_run;

const FIB: &str =
    "func fib(n: Int): Int = if n <= 1 then n else fib(n - 1) + fib(n - 2)";

fn fib_src(call: &str) -> String {
    format!("{}\n{}", FIB, call)
}

#[test]
fn test_fib_base_case_0() {
    assert_eq!(compile_and_run(&fib_src("fib(0)")), 0);
}

#[test]
fn test_fib_base_case_1() {
    assert_eq!(compile_and_run(&fib_src("fib(1)")), 1);
}

#[test]
fn test_fib_correctness() {
    assert_eq!(compile_and_run(&fib_src("fib(10)")), 55);
}

#[test]
fn test_int_literal() {
    assert_eq!(compile_and_run("42"), 42);
}

#[test]
fn test_arithmetic() {
    assert_eq!(compile_and_run("3 + 4 * 2"), 11);
}
