//! UFCS — `TRAITS.md` Part 1, Stage 0: `x.f(args)` resolves to a field
//! access if `f` names a field of `typeof(x)`, else rewrites to `f(x, args)`
//! for a global `func f` whose first parameter accepts `typeof(x)`. Trait
//! members (resolution step 2) don't exist yet — that's Stage 5.
//!
//! Also covers Part 2: the receiver of a dot-form call is exempt from the
//! `mut` call-site marker (`xs.push(x)`, not `mut xs.push(x)`), while the
//! declaration-site mutability check still applies.

mod common;
use common::run;

#[test]
fn ufcs_rewrites_a_free_function_call_with_the_receiver_as_first_arg() {
    let src = r#"
func double(x: Int): Int = x * 2
print(5.double())
"#;
    assert_eq!(run(src), "10\n");
}

#[test]
fn ufcs_chains_across_multiple_free_functions() {
    let src = r#"
func inc(xs: List(Int)): List(Int) = xs
func double_all(xs: List(Int)): List(Int) = xs
print([1, 2, 3].inc().double_all())
"#;
    assert_eq!(run(src), "[1, 2, 3]\n");
}

#[test]
fn a_field_takes_priority_over_a_same_named_free_function() {
    // If `p` has a field `x`, `p.x` must mean the field, per resolution
    // step 1 — even if a free function `x` also exists (unlikely in
    // practice, but the field must win unconditionally).
    let src = r#"
data Point(x: Int, y: Int)
let p = Point(x=1, y=2)
print(p.x)
"#;
    assert_eq!(run(src), "1\n");
}

#[test]
fn push_via_dot_form_needs_no_mut_marker_on_the_receiver() {
    let src = r#"
mut xs = [1, 2, 3]
xs.push(4)
print(xs)
"#;
    assert_eq!(run(src), "[1, 2, 3, 4]\n");
}

#[test]
fn a_mut_param_free_function_is_callable_via_dot_form_with_no_marker() {
    let src = r#"
func addOne(mut x: Int): Int = x + 1
mut n = 5
print(n.addOne())
"#;
    assert_eq!(run(src), "6\n");
}

// ── typeck rejections ─────────────────────────────────────────────────────

fn expect_ufcs_type_error(src: &str, needle: &str) {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    assert!(
        format!("{:?}", err).contains(needle),
        "unexpected error: {:?} (expected to contain {:?})", err, needle
    );
}

#[test]
fn dot_push_on_a_let_bound_list_is_rejected() {
    expect_ufcs_type_error(
        "let xs = [1, 2, 3]\nxs.push(4)\n1",
        "is not mutable",
    );
}

#[test]
fn dot_call_of_a_mut_param_function_on_a_let_binding_is_rejected() {
    expect_ufcs_type_error(
        "func addOne(mut x: Int): Int = x + 1\nlet n = 5\nn.addOne()\n1",
        "is not mutable",
    );
}

#[test]
fn no_such_field_and_no_such_function_is_a_clear_error() {
    expect_ufcs_type_error(
        "data Point(x: Int, y: Int)\nlet p = Point(x=1, y=2)\np.bogus()\n1",
        "no field 'bogus'",
    );
}
