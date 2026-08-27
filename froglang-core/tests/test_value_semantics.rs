//! Value semantics for `List<T>`, and `push` — MUTABILITY.md stage 6.
//!
//! Distinct from `test_gc_roots.rs`: that file guards against a *missing
//! root* (a value the collector should have kept alive but didn't). This
//! file guards against a *missing clone* — an alias becoming observable
//! because two live bindings ended up sharing one heap object when value
//! semantics say they must not. Both bug classes can be forced into the
//! open by `FROG_GC_STRESS=1` (a wrongly-shared buffer that gets mutated
//! and then collected/recycled corrupts a second binding's contents), so
//! every behavioral test here runs through both `run` and `run_gc_stress`,
//! same discipline as `test_gc_roots.rs`.
//!
//! See codegen/mod.rs's `Ctx::liveness` and the `TypedExprKind::Var` arm of
//! `compile_expr_multi` for the mechanism: a `Copy`-classified read of a
//! GC-pointer-bearing binding is deep-cloned (`runtime::gc::GcHeap::clone_obj`
//! / `ffi::frog_clone`) before use; a `Move`-classified read (this is the
//! name's last use) is not. `push`'s own receiver is always `Move` by
//! construction (`liveness.rs`'s `Call`/`mut_args` handling), which is the
//! elision this whole design exists to deliver.

mod common;
use common::{run, run_gc_stress};

/// The canonical case: aliasing a `mut` list and mutating the alias must
/// not be observable through the original binding. If `mut b = a`'s `Var`
/// read of `a` were wrongly classified `Move` (or the clone were skipped
/// entirely), `push(mut b, 4)` would mutate the same buffer `a` names.
#[test]
fn pushing_through_an_alias_does_not_affect_the_original() {
    let src = r#"
mut a = [1, 2, 3]
mut b = a
push(mut b, 4)
print(a)
print(b)
"#;
    assert_eq!(run(src), "[1, 2, 3]\n[1, 2, 3, 4]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 3]\n[1, 2, 3, 4]\n");
}

/// Struct-element variant: exercises `stride > 1` in `frog_clone`'s `List`
/// case (multiple leaves per element, one of them a `Str` — a real GC
/// pointer column, not just a scalar).
#[test]
fn pushing_through_an_alias_of_a_struct_list_does_not_affect_the_original() {
    let src = r#"
data Point(x: Int, name: Str)
mut a = [Point(x=1, name="a"), Point(x=2, name="b")]
mut b = a
push(mut b, Point(x=3, name="c"))
print(a[0].x)
print(b[2].x)
print(b[2].name)
"#;
    assert_eq!(run(src), "1\n3\nc\n");
    assert_eq!(run_gc_stress(src), "1\n3\nc\n");
}

/// Loop-carried alias — the highest-risk shape, since it exercises
/// `liveness.rs`'s loop-back-edge fixpoint (`transfer_loop`) rather than the
/// `Call`/`mut_args` carve-out `push`'s own receiver always takes. `a` is
/// read (`mut c = a`) inside a loop body whose binding gets pushed into,
/// while `a` itself is used again after the loop — a wrong `Move`
/// classification here would let some iteration's `push` corrupt `a`.
/// `c` should be `[1,2,3,i]` each iteration, so `last` after the loop is the
/// final iteration's appended element (`2`), and `a` must still be `[1,2,3]`.
#[test]
fn a_list_read_inside_a_loop_body_and_pushed_into_does_not_alias_the_outer_binding() {
    let src = r#"
func go(): Int = {
  mut a = [1, 2, 3]
  mut last = 0
  for i in 0..3 do {
    mut c = a
    push(mut c, i)
    last = c[3]
  }
  a[2] * 1000 + last
}
print(go())
"#;
    assert_eq!(run(src), "3002\n");
    assert_eq!(run_gc_stress(src), "3002\n");
}

/// Pure correctness of `push` in a loop with no aliasing at all. This is
/// *not* a test that elision actually happens (a correctness test can't
/// distinguish "no clone was needed" from "a clone happened and didn't
/// matter") — see `benches/pipeline.rs` for the elision claim.
#[test]
fn push_in_a_loop_with_no_aliasing_builds_the_expected_list() {
    let src = r#"
mut xs = []
for i in 0..5 do push(mut xs, i)
print(xs)
"#;
    assert_eq!(run(src), "[0, 1, 2, 3, 4]\n");
    assert_eq!(run_gc_stress(src), "[0, 1, 2, 3, 4]\n");
}

/// Composition with a user `func`'s own `mut` parameter: `push` inside a
/// `mut`-param function, called with an aliased argument at the call site.
/// Confirms the two `mut` mechanisms (a user `func`'s call-site `mut` arg,
/// and `push`'s own internal `mut` arg) don't double-clone or under-clone.
#[test]
fn push_inside_a_mut_param_function_does_not_alias_the_callers_other_binding() {
    let src = r#"
func addOne(mut xs: List<Int>): None = { push(mut xs, 99) }
mut a = [1, 2, 3]
mut b = a
addOne(mut b)
print(a)
print(b)
"#;
    assert_eq!(run(src), "[1, 2, 3]\n[1, 2, 3, 99]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 3]\n[1, 2, 3, 99]\n");
}

// ── typeck rejections ─────────────────────────────────────────────────────

fn expect_push_type_error(src: &str, needle: &str) {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    assert!(
        format!("{:?}", err).contains(needle),
        "unexpected error: {:?} (expected to contain {:?})", err, needle
    );
}

#[test]
fn push_without_mut_on_the_receiver_is_rejected() {
    expect_push_type_error(
        "mut xs = [1, 2, 3]\npush(xs, 4)\n1",
        "must be marked 'mut'",
    );
}

#[test]
fn push_on_a_let_bound_list_is_rejected() {
    expect_push_type_error(
        "let xs = [1, 2, 3]\npush(mut xs, 4)\n1",
        "is not mutable",
    );
}

#[test]
fn push_on_a_non_list_is_rejected() {
    expect_push_type_error(
        "mut x = 1\npush(mut x, 4)\n1",
        "must be a List",
    );
}

#[test]
fn push_with_the_same_root_as_both_arguments_is_rejected() {
    expect_push_type_error(
        "mut xs = [1, 2, 3]\npush(mut xs, xs)\n1",
        "can't be passed 'mut'",
    );
}
