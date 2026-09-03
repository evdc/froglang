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
    assert_eq!(frog_list_get(bits, 1, 0),20);
}

#[test]
fn test_list_get_first_last() {
    let bits = compile_and_run("[100, 200, 300]");
    assert_eq!(frog_list_get(bits, 0, 0),100);
    assert_eq!(frog_list_get(bits, 2, 0),300);
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
    let get = |i: i64| f64::from_bits(frog_list_get(bits, i, 0) as u64);
    assert_eq!(get(0), 1.0);
    assert_eq!(get(1), 2.5);
    assert_eq!(get(2), -3.25);
}

#[test]
fn test_bool_list_roundtrips() {
    let bits = compile_and_run("[true, false, true]");
    assert_eq!(frog_list_len(bits), 3);
    assert!(frog_list_get(bits, 0, 0) != 0);
    assert!(!(frog_list_get(bits, 1, 0) != 0));
    assert!(frog_list_get(bits, 2, 0) != 0);
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
    (0..n).map(|i| frog_list_get(bits, i, 0)).collect()
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
    // `1..5` no longer materializes a `List` on its own — `to_list` is the
    // explicit opt-in conversion now (see the comment below).
    let bits = compile_and_run("to_list(1..5)");
    assert_eq!(list_elems(bits), vec![1, 2, 3, 4]);
}

// `Range` is now a real, distinct nominal type (`Type::Named{"Range",[T]}`),
// not a materializing `List<Int>` in disguise — see `plans/RANGES.md`.
// `to_list(range)` is the explicit, opt-in conversion for tests (and
// programs) that want `List` semantics.

#[test]
fn test_range_reversed_is_empty() {
    let bits = compile_and_run("to_list(5..1)");
    assert_eq!(list_elems(bits), Vec::<i64>::new());
}

#[test]
fn test_range_index_is_a_type_error() {
    let ast = froglang_core::frontend::parser::Parser::parse("(1..10)[3]").expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("indexing a Range should be a type error");
    assert!(format!("{:?}", err).contains("Can't index into Range"), "unexpected error: {:?}", err);
}

#[test]
fn test_range_in_let() {
    assert_eq!(compile_and_run("let r = 1..5\nr.start"), 1);
    assert_eq!(compile_and_run("let r = 1..5\nr.end"), 5);
    let bits = compile_and_run("let r = 1..5\nto_list(r)");
    assert_eq!(list_elems(bits), vec![1, 2, 3, 4]);
}

#[test]
fn test_slice_str_list() {
    let bits = compile_and_run(r#"["a", "b", "c", "d"][1..3]"#);
    assert_eq!(frog_list_len(bits), 2);
    let s0 = unsafe { frog_str_as_str(frog_list_get(bits, 0, 0) as *const FrogStr) };
    let s1 = unsafe { frog_str_as_str(frog_list_get(bits, 1, 0) as *const FrogStr) };
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
    let s0 = unsafe { frog_str_as_str(frog_list_get(bits, 0, 0) as *const FrogStr) };
    let s2 = unsafe { frog_str_as_str(frog_list_get(bits, 2, 0) as *const FrogStr) };
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
        "mut total = 0\nfor x in [1, 2, 3, 4, 5] do { total = total + x }\ntotal"
    );
    assert_eq!(result, 15);
}

/// Same underlying bug, but via `if`/`else` rather than a loop body — a
/// reassignment inside either branch used to crash the same way.
#[test]
fn test_reassignment_inside_if_else_branches() {
    let result = compile_and_run(
        "mut x = 1\nif true then { x = 99 } else { x = 0 }\nx"
    );
    assert_eq!(result, 99);
}

/// Regression test for GC rooting under deep recursion: a string bound
/// early must survive many subsequent string allocations (and the GC
/// collections they trigger) deep inside recursive calls. Before rooting
/// existed at all, `keep` had no root visible to the collector while
/// `pad`'s recursive calls were executing, so the collector could free it
/// out from under a still-live JIT register — this used to trip a
/// `ptr::copy_nonoverlapping` UB check (or silently corrupt memory in
/// release builds).
#[test]
fn test_gc_survives_recursive_allocation() {
    let src = r#"func pad(n: Int): Str = if n <= 0 then "" else pad(n - 1) + "0123456789012345678901234567890123456789"
let keep = "SENTINEL"
let big = pad(3000)
keep"#;
    let bits = compile_and_run(src);
    let s = unsafe { frog_str_as_str(bits as *const FrogStr) };
    assert_eq!(s, "SENTINEL");
}

// ── structs ────────────────────────────────────────────────────────────────

#[test]
fn test_struct_construct_and_field_access() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet alice = Person(name=\"Alice\", age=42)\nalice.age"
    ), 42);
}

#[test]
fn test_struct_field_order_independent_at_construction() {
    // Fields passed out of declaration order still land correctly.
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet alice = Person(age=42, name=\"Alice\")\nalice.age"
    ), 42);
}

#[test]
fn test_struct_declaration_order_independent() {
    // `Company` references `Person`, declared *after* it in the source.
    assert_eq!(compile_and_run(
        "data Company(ceo: Person)\ndata Person(name: Str, age: Int)\nlet c = Company(ceo=Person(name=\"Alice\", age=42))\nc.ceo.age"
    ), 42);
}

/// The crux of the whole unboxed-value-semantics design: aliasing a struct
/// binding and then "mutating" the alias via the rebind-sugar must NOT
/// affect the original binding, proving copy-on-assign actually holds under
/// codegen (not just on paper) — structs are flattened `Variable`s, not a
/// shared boxed pointer.
#[test]
fn test_struct_value_semantics_alias_not_affected_by_rebind() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet a = Person(name=\"Alice\", age=42)\nmut b = a\nb.age = 99\na.age"
    ), 42);
}

#[test]
fn test_struct_rebind_sugar_updates_only_target_field() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nmut a = Person(name=\"Alice\", age=42)\na.age = 99\na.age"
    ), 99);
}

#[test]
fn test_struct_equality_true() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet a = Person(name=\"Alice\", age=42)\nlet b = Person(name=\"Alice\", age=42)\nif a == b then 1 else 0"
    ), 1);
}

#[test]
fn test_struct_equality_false_on_differing_field() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet a = Person(name=\"Alice\", age=42)\nlet b = Person(name=\"Alice\", age=43)\nif a == b then 1 else 0"
    ), 0);
}

#[test]
fn test_struct_not_equal_operator() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet a = Person(name=\"Alice\", age=42)\nlet b = Person(name=\"Alice\", age=43)\nif a != b then 1 else 0"
    ), 1);
}

#[test]
fn test_nested_struct_field_access() {
    assert_eq!(compile_and_run(
        "data Address(city: Str, zip: Int)\ndata Person(name: Str, address: Address)\nlet a = Person(name=\"Alice\", address=Address(city=\"Springfield\", zip=12345))\na.address.zip"
    ), 12345);
}

#[test]
fn test_nested_struct_equality() {
    assert_eq!(compile_and_run(
        "data Address(city: Str, zip: Int)\ndata Person(name: Str, address: Address)\nlet a = Person(name=\"Alice\", address=Address(city=\"X\", zip=1))\nlet b = Person(name=\"Alice\", address=Address(city=\"X\", zip=1))\nif a == b then 1 else 0"
    ), 1);
}

#[test]
fn test_struct_typed_function_param_and_return() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nfunc birthday(p: Person): Person = Person(name=p.name, age=p.age + 1)\nlet a = Person(name=\"Alice\", age=42)\nlet b = birthday(a)\nb.age"
    ), 43);
}

#[test]
fn test_struct_typed_function_does_not_mutate_caller_arg() {
    // `p` (the parameter) is immutable until `mut` parameters exist
    // (MUTABILITY.md stage 4); `mut q = p` takes an explicit local mutable
    // copy, and mutating it still never affects the caller's argument.
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nfunc birthday(p: Person): Person = { mut q = p\nq.age = q.age + 1\nq }\nlet a = Person(name=\"Alice\", age=42)\nlet b = birthday(a)\na.age"
    ), 42);
}

#[test]
fn test_list_of_structs_index() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet people = [Person(name=\"Alice\", age=42), Person(name=\"Bob\", age=34)]\npeople[1].age"
    ), 34);
}

#[test]
fn test_list_of_structs_iteration_sum() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet people = [Person(name=\"A\", age=1), Person(name=\"B\", age=2), Person(name=\"C\", age=3)]\nmut total = 0\nfor p in people do { total = total + p.age }\ntotal"
    ), 6);
}

#[test]
fn test_comprehension_of_structs() {
    assert_eq!(compile_and_run(
        "data Sq(n: Int, sq: Int)\nlet sqs = [for i in 1..4 do Sq(n=i, sq=i * i)]\nsqs[2].sq"
    ), 9);
}

/// GC stress: many struct allocations, each holding a `Str` (heap-pointer)
/// field, forcing multiple collections — regression test for the list
/// stride/pointer-mask generalization in `GcHeap::mark` (runtime/gc.rs).
#[test]
fn test_list_of_structs_with_str_field_survives_gc() {
    assert_eq!(compile_and_run(
        "data Person(name: Str, age: Int)\nlet people = [for i in 1..2000 do Person(name=\"p\", age=i)]\npeople[1998].age"
    ), 1999);
}

#[test]
fn test_struct_self_reference_rejected() {
    let ast = froglang_core::frontend::parser::Parser::parse(
        "data Tree(value: Int, left: Tree)\n1"
    ).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("self-referential struct should be a type error");
    assert!(format!("{:?}", err).contains("contains itself"), "unexpected error: {:?}", err);
}

#[test]
fn test_struct_missing_field_rejected() {
    let ast = froglang_core::frontend::parser::Parser::parse(
        "data Person(name: Str, age: Int)\nPerson(name=\"Alice\")\n1"
    ).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("missing field should be a type error");
    assert!(format!("{:?}", err).contains("Missing field"), "unexpected error: {:?}", err);
}

#[test]
fn test_struct_nominal_typing_rejects_cross_type_equality() {
    let ast = froglang_core::frontend::parser::Parser::parse(
        "data Point(x: Int, y: Int)\ndata Size(x: Int, y: Int)\nlet p = Point(x=1, y=2)\nlet s = Size(x=1, y=2)\np == s"
    ).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("different struct names should never unify");
    assert!(format!("{:?}", err).contains("incompatible types"), "unexpected error: {:?}", err);
}

// ── struct programs from tests/programs/ (structs + for-loops/comprehensions
// combined, more realistic than the single-feature tests above) ─────────────

/// Shopping cart: nested structs (`LineItem` holds a `Product`), list
/// indexing into a separately-built catalog, a function taking a struct
/// param, a plain for-loop accumulator, and a filtering comprehension.
/// subtotal = 450*2 + 500*1 = 1400; bulk discount = 900/10 = 90 -> 1310.
#[test]
fn test_struct_cart_total() {
    assert_eq!(compile_and_run(&prog("struct_cart_total.frog")), 1310);
}

/// Grade stats: a struct-returning function that mutates its (copied)
/// struct param via the field-rebind sugar, a comprehension filtering
/// structs (not just scalars) by a field predicate, and a for-loop folding
/// the filtered results through the function. Passing scores 92/77/88,
/// average = 257/3 = 85 (integer division).
#[test]
fn test_struct_grade_stats() {
    assert_eq!(compile_and_run(&prog("struct_grade_stats.frog")), 85);
}

/// Shapes-by-origin: nested structs (`Shape` holds a `Point`), struct
/// equality used directly as a comprehension filter predicate, and a
/// function taking a struct param. Shapes A and C are centered at the
/// origin (radius 2 and 1); total_area = 2*2*3 + 1*1*3 = 15.
#[test]
fn test_struct_shapes_by_origin() {
    assert_eq!(compile_and_run(&prog("struct_shapes_by_origin.frog")), 15);
}

// ── mutability programs from tests/programs/ (MUTABILITY.md stages 3-4: ────
// places and `mut` parameters — each of these was a parse error or a type
// error before that work landed) ────────────────────────────────────────────

/// Same computation as `struct_grade_stats.frog`, but `record` writes back
/// into the caller's `stats` through a `mut` parameter instead of returning
/// a fresh copy for the caller to rebind. average = 257/3 = 85.
#[test]
fn test_mut_accumulator_via_param() {
    assert_eq!(compile_and_run(&prog("mut_accumulator_via_param.frog")), 85);
}

/// Assignment through a path three struct levels deep (`scene.bg.origin.x`)
/// — `o.i.v = 5` was a parse error before places existed.
#[test]
fn test_mut_nested_field_path() {
    assert_eq!(compile_and_run(&prog("mut_nested_field_path.frog")), 178);
}

/// List index assignment, both on a scalar list and on a list of structs
/// through a nested field (`cells[i].age = ...`) — no user-facing list
/// mutation existed before places.
#[test]
fn test_mut_list_index_assign() {
    assert_eq!(compile_and_run(&prog("mut_list_index_assign.frog")), 197);
}

/// Two `mut` parameters writing back into two distinct caller bindings from
/// the same call — callee mutation of a caller's argument didn't exist at
/// all before `mut` parameters. x and y swap: (9, 3) -> 9*10+3 = 93.
#[test]
fn test_mut_swap_via_params() {
    assert_eq!(compile_and_run(&prog("mut_swap_via_params.frog")), 93);
}

/// The exclusivity rule: a `mut`-marked root can't also appear as another
/// argument in the same call.
#[test]
fn test_mut_exclusivity_rejects_aliased_root() {
    let ast = froglang_core::frontend::parser::Parser::parse(
        "func f(mut a: Int, b: Int): None = { a = b\nnone }\nmut x = 1\nf(mut x, x)\n1"
    ).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("aliased mut root should be rejected");
    assert!(format!("{:?}", err).contains("can't be passed 'mut'"), "unexpected error: {:?}", err);
}

// ── `else`-less `if` in statement position ───────────────────────────────────
//
// `if cond then side_effect()` is the reason `if` needs to work as a
// statement even though it is an expression. Its value is `T | None`
// (implicit `else none`), so these assert through a `mut` binding rather
// than on the `if`'s own result. See `test_parser.rs` for the parse the
// separator after it used to break.

#[test]
fn else_less_if_runs_its_branch_in_statement_position() {
    assert_eq!(compile_and_run("mut n = 0\nif 1 > 0 then n = 5\nn"), 5);
}

#[test]
fn else_less_if_skips_its_branch_when_the_condition_is_false() {
    assert_eq!(compile_and_run("mut n = 0\nif 1 > 2 then n = 5\nn"), 0);
}

#[test]
fn else_less_if_in_a_block_body_leaves_the_following_statements_alone() {
    assert_eq!(compile_and_run(
        "func classify(x: Int): Int = {\n\
           mut n = 1\n\
           if x > 0 then n = n * 10\n\
           if x > 100 then n = n * 10\n\
           n\n\
         }\n\
         classify(5) + classify(500) + classify(0 - 1)"
    ), 10 + 100 + 1);
}
