use froglang_core::codegen::compile_and_run;
use froglang_core::runtime::ffi::{frog_str_len, frog_list_len, frog_list_get};
use froglang_core::runtime::gc::{frog_str_as_str, FrogStr};

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

// ── string / list integration tests ──────────────────────────────────────────

#[test]
fn test_str_concat_len() {
    let bits = compile_and_run(r#""hello" + " " + "world""#);
    assert_eq!(frog_str_len(bits), 11);
}

#[test]
fn test_str_content() {
    let bits = compile_and_run(r#""froglang""#);
    let s = unsafe { frog_str_as_str(bits as *const FrogStr) };
    assert_eq!(s, "froglang");
}

#[test]
fn test_str_eq_true() {
    let bits = compile_and_run(r#""abc" == "abc""#);
    assert_eq!(bits, 1);
}

#[test]
fn test_str_eq_false() {
    let bits = compile_and_run(r#""abc" == "xyz""#);
    assert_eq!(bits, 0);
}

#[test]
fn test_str_neq() {
    let bits = compile_and_run(r#""abc" != "xyz""#);
    assert_eq!(bits, 1);
}

#[test]
fn test_str_lt() {
    assert_eq!(compile_and_run(r#""abc" < "abd""#), 1);
    assert_eq!(compile_and_run(r#""abd" < "abc""#), 0);
}

#[test]
fn test_str_gt_le_ge() {
    assert_eq!(compile_and_run(r#""b" > "a""#), 1);
    assert_eq!(compile_and_run(r#""a" <= "a""#), 1);
    assert_eq!(compile_and_run(r#""b" >= "a""#), 1);
    assert_eq!(compile_and_run(r#""a" >= "b""#), 0);
}

#[test]
fn test_unary_not() {
    assert_eq!(compile_and_run("not true"), 0);
    assert_eq!(compile_and_run("not false"), 1);
}

#[test]
fn test_list_len() {
    let bits = compile_and_run("[1, 2, 3, 4, 5]");
    assert_eq!(frog_list_len(bits), 5);
}

#[test]
fn test_list_get() {
    let bits = compile_and_run("[10, 20, 30]");
    assert_eq!(frog_list_get(bits, 1),20);
}

#[test]
fn test_list_get_first_last() {
    let bits = compile_and_run("[100, 200, 300]");
    assert_eq!(frog_list_get(bits, 0),100);
    assert_eq!(frog_list_get(bits, 2),300);
}

#[test]
fn test_str_in_let() {
    let bits = compile_and_run(r#"let s = "hello" + " world"
s"#);
    assert_eq!(frog_str_len(bits), 11);
}

// ── non-Int list elements (Float/Bool) ───────────────────────────────────────
//
// `frog_list_push` takes a plain i64; Float (F64) and Bool (I8) SSA values
// need the same wire-format conversion applied everywhere else a value
// crosses an i64 FFI boundary (see `to_i64_repr` in codegen). Before this was
// applied to list elements too, `[1.0, 2.0]` and `[true, false]` both hit a
// Cranelift "Verifier errors" panic instead of ever running.

#[test]
fn test_float_list_roundtrips() {
    let bits = compile_and_run("[1.0, 2.5, -3.25]");
    assert_eq!(frog_list_len(bits), 3);
    let get = |i: i64| f64::from_bits(frog_list_get(bits, i) as u64);
    assert_eq!(get(0), 1.0);
    assert_eq!(get(1), 2.5);
    assert_eq!(get(2), -3.25);
}

#[test]
fn test_bool_list_roundtrips() {
    let bits = compile_and_run("[true, false, true]");
    assert_eq!(frog_list_len(bits), 3);
    assert_eq!(frog_list_get(bits, 0) != 0, true);
    assert_eq!(frog_list_get(bits, 1) != 0, false);
    assert_eq!(frog_list_get(bits, 2) != 0, true);
}

// ── list indexing ────────────────────────────────────────────────────────────

#[test]
fn test_index_int_list() {
    assert_eq!(compile_and_run("[10, 20, 30][1]"), 20);
    assert_eq!(compile_and_run("[10, 20, 30][0]"), 10);
    assert_eq!(compile_and_run("[10, 20, 30][2]"), 30);
}

#[test]
fn test_index_computed() {
    let src = "let xs = [1, 2, 3]\nlet i = 1 + 1\nxs[i]";
    assert_eq!(compile_and_run(src), 3);
}

#[test]
fn test_index_float_list() {
    let bits = compile_and_run("[1.5, 2.5, 3.5][2]");
    assert_eq!(f64::from_bits(bits as u64), 3.5);
}

#[test]
fn test_index_bool_list() {
    assert_eq!(compile_and_run("[true, false, true][1]"), 0);
    assert_eq!(compile_and_run("[true, false, true][0]"), 1);
}

#[test]
fn test_index_str_list() {
    let bits = compile_and_run(r#"["a", "bb", "ccc"][1]"#);
    let s = unsafe { frog_str_as_str(bits as *const FrogStr) };
    assert_eq!(s, "bb");
}

#[test]
fn test_index_chained() {
    // A list of lists; index twice.
    assert_eq!(compile_and_run("[[1, 2], [3, 4]][1][0]"), 3);
}

#[test]
fn test_index_negative() {
    assert_eq!(compile_and_run("[10, 20, 30][-1]"), 30);
    assert_eq!(compile_and_run("[10, 20, 30][-3]"), 10);
}

// ── list slicing ─────────────────────────────────────────────────────────────

fn list_elems(bits: i64) -> Vec<i64> {
    let n = frog_list_len(bits);
    (0..n).map(|i| frog_list_get(bits, i)).collect()
}

#[test]
fn test_slice_both_bounds() {
    let bits = compile_and_run("[1, 2, 3, 4, 5][1..3]");
    assert_eq!(list_elems(bits), vec![2, 3]);
}

#[test]
fn test_slice_start_only() {
    let bits = compile_and_run("[1, 2, 3, 4, 5][2..]");
    assert_eq!(list_elems(bits), vec![3, 4, 5]);
}

#[test]
fn test_slice_end_only() {
    let bits = compile_and_run("[1, 2, 3, 4, 5][..2]");
    assert_eq!(list_elems(bits), vec![1, 2]);
}

#[test]
fn test_slice_full() {
    let bits = compile_and_run("[1, 2, 3, 4, 5][..]");
    assert_eq!(list_elems(bits), vec![1, 2, 3, 4, 5]);
}

#[test]
fn test_slice_negative_bounds() {
    let bits = compile_and_run("[1, 2, 3, 4, 5][-2..]");
    assert_eq!(list_elems(bits), vec![4, 5]);
    let bits = compile_and_run("[1, 2, 3, 4, 5][..-2]");
    assert_eq!(list_elems(bits), vec![1, 2, 3]);
}

#[test]
fn test_slice_out_of_range_clamps() {
    let bits = compile_and_run("[1, 2, 3][0..100]");
    assert_eq!(list_elems(bits), vec![1, 2, 3]);
}

#[test]
fn test_slice_inverted_range_is_empty() {
    let bits = compile_and_run("[1, 2, 3][2..0]");
    assert_eq!(list_elems(bits), Vec::<i64>::new());
}

#[test]
fn test_slice_chained_with_index() {
    assert_eq!(compile_and_run("[1, 2, 3, 4, 5][1..4][1]"), 3);
}

// ── standalone ranges ────────────────────────────────────────────────────────

#[test]
fn test_range_materializes_list() {
    let bits = compile_and_run("1..5");
    assert_eq!(list_elems(bits), vec![1, 2, 3, 4]);
}

#[test]
fn test_range_reversed_is_empty() {
    let bits = compile_and_run("5..1");
    assert_eq!(list_elems(bits), Vec::<i64>::new());
}

#[test]
fn test_range_index() {
    assert_eq!(compile_and_run("(1..10)[3]"), 4);
}

#[test]
fn test_range_in_let() {
    let bits = compile_and_run("let r = 1..5\nr");
    assert_eq!(list_elems(bits), vec![1, 2, 3, 4]);
}

#[test]
fn test_slice_str_list() {
    let bits = compile_and_run(r#"["a", "b", "c", "d"][1..3]"#);
    assert_eq!(frog_list_len(bits), 2);
    let s0 = unsafe { frog_str_as_str(frog_list_get(bits, 0) as *const FrogStr) };
    let s1 = unsafe { frog_str_as_str(frog_list_get(bits, 1) as *const FrogStr) };
    assert_eq!(s0, "b");
    assert_eq!(s1, "c");
}

// ── for-loops and comprehensions ────────────────────────────────────────────

#[test]
fn test_for_loop_runs_and_evaluates_to_none() {
    // A bare `for` loop's own value is `Type::None`, wire-represented as 0 —
    // this just confirms the loop actually runs (rather than, say, being
    // optimized away) without crashing.
    assert_eq!(compile_and_run("for x in [1, 2, 3] do x\n0"), 0);
}

#[test]
fn test_for_loop_with_if_filter_runs() {
    assert_eq!(compile_and_run("for x in [1, 2, 3] if x > 1 do x\n0"), 0);
}

#[test]
fn test_comprehension_basic() {
    let bits = compile_and_run("[for x in [1, 2, 3, 4, 5] do x * 2]");
    assert_eq!(list_elems(bits), vec![2, 4, 6, 8, 10]);
}

#[test]
fn test_comprehension_with_filter() {
    let bits = compile_and_run("[for x in [1, 2, 3, 4, 5, 6] if x > 2 do x * 2]");
    assert_eq!(list_elems(bits), vec![6, 8, 10, 12]);
}

#[test]
fn test_comprehension_over_range() {
    let bits = compile_and_run("[for i in 1..5 do i * i]");
    assert_eq!(list_elems(bits), vec![1, 4, 9, 16]);
}

#[test]
fn test_comprehension_over_empty_list() {
    // `[3..3]` slices to an empty list without needing list-type annotation
    // syntax (which doesn't exist yet).
    let bits = compile_and_run("[for x in [1, 2, 3][3..3] do x]");
    assert_eq!(list_elems(bits), Vec::<i64>::new());
}

#[test]
fn test_comprehension_chained_with_index() {
    assert_eq!(compile_and_run("[for x in 1..5 do x][0]"), 1);
}

#[test]
fn test_comprehension_over_strings() {
    let bits = compile_and_run(r#"[for s in ["a", "b", "c"] do s + "!"]"#);
    assert_eq!(frog_list_len(bits), 3);
    let s0 = unsafe { frog_str_as_str(frog_list_get(bits, 0) as *const FrogStr) };
    let s2 = unsafe { frog_str_as_str(frog_list_get(bits, 2) as *const FrogStr) };
    assert_eq!(s0, "a!");
    assert_eq!(s2, "c!");
}

#[test]
fn test_comprehension_in_let() {
    let bits = compile_and_run("let ys = [for x in [1, 2, 3] do x * 10]\nys");
    assert_eq!(list_elems(bits), vec![10, 20, 30]);
}

/// Regression test: reassigning an existing variable inside a loop body (the
/// classic accumulator pattern) used to crash codegen with a Cranelift
/// dominance-verifier error, since `vars` stored raw `Value`s that are only
/// valid in the block that produced them. Fixed by switching to Cranelift
/// `Variable`/`declare_var`/`use_var`/`def_var`, which lets Cranelift insert
/// the phi nodes a loop back-edge needs. See `get_or_declare_var` in
/// `codegen/mod.rs`.
#[test]
fn test_for_loop_accumulator_pattern() {
    let result = compile_and_run(
        "let total = 0\nfor x in [1, 2, 3, 4, 5] do { total = total + x }\ntotal"
    );
    assert_eq!(result, 15);
}

/// Same underlying bug, but via `if`/`else` rather than a loop body — a
/// reassignment inside either branch used to crash the same way.
#[test]
fn test_reassignment_inside_if_else_branches() {
    let result = compile_and_run(
        "let x = 1\nif true then { x = 99 } else { x = 0 }\nx"
    );
    assert_eq!(result, 99);
}

/// Regression test for the GC shadow stack: a string bound early must survive
/// many subsequent string allocations (and the GC collections they trigger)
/// deep inside recursive calls. Before the shadow stack, `keep` had no root
/// visible to the collector while `pad`'s recursive calls were executing, so
/// the collector could free it out from under a still-live JIT register —
/// this used to trip a `ptr::copy_nonoverlapping` UB check (or silently
/// corrupt memory in release builds).
#[test]
fn test_gc_shadow_stack_survives_recursive_allocation() {
    let src = r#"func pad(n: Int): Str = if n <= 0 then "" else pad(n - 1) + "0123456789012345678901234567890123456789"
let keep = "SENTINEL"
let big = pad(3000)
keep"#;
    let bits = compile_and_run(src);
    let s = unsafe { frog_str_as_str(bits as *const FrogStr) };
    assert_eq!(s, "SENTINEL");
}
