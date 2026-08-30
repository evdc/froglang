//! Value semantics for `List<T>`, and `push` — MUTABILITY.md stages 6 and 7.
//!
//! Distinct from `test_gc_roots.rs`: that file guards against a *missing
//! root* (a value the collector should have kept alive but didn't). This
//! file guards against an alias becoming observable — two live bindings
//! sharing one heap object where value semantics say they must not. Both bug
//! classes can be forced into the open by `FROG_GC_STRESS=1` (a wrongly
//! shared buffer that gets mutated and then collected/recycled corrupts a
//! second binding's contents), so every behavioral test here runs through
//! both `run` and `run_gc_stress`, same discipline as `test_gc_roots.rs`.
//!
//! The mechanism is copy-on-write (Stage 7). A `Copy`-classified read of a
//! `List` binding, and any read of one *out of a container*, marks the
//! object `shared` (`codegen::mark_shared_if_aliased` /
//! `mark_shared_extracted`); the two mutations that exist — `push` and index
//! assignment — copy first if that bit is set (`codegen::emit_unshare`) and
//! rebind their root to the copy. A `Move`-classified read is the name's
//! last use, so it marks nothing; `push`'s own receiver is always `Move` by
//! construction (`liveness.rs`'s `Call`/`mut_args` handling), which is the
//! elision this whole design exists to deliver.
//!
//! Stage 6 deep-copied at the read instead. Stage 7's tests below carry
//! `run_cow_verify` in addition, because moving the copy to the write
//! introduces a failure mode the earlier scheme did not have: an aliasing
//! site that forgets to mark.

mod common;
use common::{run, run_cow_verify, run_gc_stress};

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

/// An empty-list literal passed as a `mut` argument, into a parameter whose
/// element type is concrete. `lower_call_arg` unifies the literal's
/// still-unresolved `TypeVar` element type against the parameter, but
/// `lower_widen` used to compare the *unresolved* `TypedExpr::ty` and fall
/// through unchanged — so codegen, reading `ty` directly, saw a `List<t0>`
/// value land in a `List<Str>` slot and panicked ("no widening from
/// List<t0> to List<Str>"). `mut acc = ["seed"]` masked this: only an
/// argument the *element type itself* is never independently fixed by
/// triggers it.
#[test]
fn an_empty_list_literal_widens_through_a_mut_argument() {
    let src = r#"
func fill(mut out: List<Str>): None = { push(mut out, "x") }
mut acc = []
fill(mut acc)
print(acc)
"#;
    assert_eq!(run(src), "[\"x\"]\n");
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

// ── MUTABILITY.md Stage 7: copy-on-write ─────────────────────────────────────
//
// Stage 6 protected an alias by deep-copying it at the *read*. Stage 7 marks
// it and copies at the *write* instead, which is what made the three
// extraction cases below affordable to fix at all — under eager copying,
// closing them meant a clone per element read.
//
// These run under `run_cow_verify` as well as `run`/`run_gc_stress`: the
// failure mode copy-on-write has is a missed `shared` mark, and that check
// catches it at the barrier rather than waiting for a wrong value to
// surface. See `common::run_cow_verify`.

/// Reading a list *out of a container* aliases it — the container keeps its
/// own path to it — so pushing through the extracted binding must not be
/// visible through the container. Before Stage 7 this printed
/// `[[1, 2, 9], [3, 4]]`: extraction is not a `Var` read, so nothing cloned.
#[test]
fn pushing_through_a_list_extracted_from_a_list_does_not_affect_the_container() {
    let src = r#"
mut rows = [[1, 2], [3, 4]]
mut inner = rows[0]
push(mut inner, 9)
print(rows)
print(inner)
"#;
    assert_eq!(run(src), "[[1, 2], [3, 4]]\n[1, 2, 9]\n");
    assert_eq!(run_gc_stress(src), "[[1, 2], [3, 4]]\n[1, 2, 9]\n");
    assert_eq!(run_cow_verify(src), "[[1, 2], [3, 4]]\n[1, 2, 9]\n");
}

/// The same aliasing through a `for`-loop's element binding, which is the
/// shape `benches/life.frog` is built out of. Before Stage 7 the pushes
/// landed in the original rows: `[[1, 2, 7], [3, 4, 7]]`.
#[test]
fn pushing_through_a_for_loop_element_binding_does_not_affect_the_container() {
    let src = r#"
mut rows = [[1, 2], [3, 4]]
for row in rows do {
    mut r = row
    push(mut r, 7)
}
print(rows)
"#;
    assert_eq!(run(src), "[[1, 2], [3, 4]]\n");
    assert_eq!(run_gc_stress(src), "[[1, 2], [3, 4]]\n");
    assert_eq!(run_cow_verify(src), "[[1, 2], [3, 4]]\n");
}

/// And through a struct field. Before Stage 7 both lines printed
/// `[1, 2, 3]` — the struct's own field had been mutated.
#[test]
fn pushing_through_a_list_extracted_from_a_struct_field_does_not_affect_the_struct() {
    let src = r#"
data Box(items: List<Int>)
let b = Box(items=[1, 2])
mut got = b.items
push(mut got, 3)
print(b.items)
print(got)
"#;
    assert_eq!(run(src), "[1, 2]\n[1, 2, 3]\n");
    assert_eq!(run_gc_stress(src), "[1, 2]\n[1, 2, 3]\n");
    assert_eq!(run_cow_verify(src), "[1, 2]\n[1, 2, 3]\n");
}

/// A list bound to an *immutable* name and then aliased into a `mut` one:
/// the read of `a` is `Copy` (it is used again by `print`), so `a` is marked
/// shared and `b`'s push copies. This is the case a "clone only when the
/// source binding is `mut`" rule would get wrong.
#[test]
fn pushing_through_a_mut_alias_of_an_immutable_binding_does_not_affect_it() {
    let src = r#"
let a = [1, 2, 3]
mut b = a
push(mut b, 4)
print(a)
print(b)
"#;
    assert_eq!(run(src), "[1, 2, 3]\n[1, 2, 3, 4]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 3]\n[1, 2, 3, 4]\n");
    assert_eq!(run_cow_verify(src), "[1, 2, 3]\n[1, 2, 3, 4]\n");
}

/// A callee rebinding an immutable parameter to a `mut` local and pushing:
/// the caller's list must be untouched. The protection is the caller-side
/// mark (`f(a)` reads `a` as `Copy`, since `a` is read again afterwards),
/// not anything the callee does.
#[test]
fn a_callee_rebinding_a_param_to_a_mut_local_does_not_affect_the_caller() {
    let src = r#"
func f(xs: List<Int>): Int = {
    mut ys = xs
    push(mut ys, 99)
    ys.len()
}
mut a = [1, 2, 3]
print(f(a))
print(a)
"#;
    assert_eq!(run(src), "4\n[1, 2, 3]\n");
    assert_eq!(run_gc_stress(src), "4\n[1, 2, 3]\n");
    assert_eq!(run_cow_verify(src), "4\n[1, 2, 3]\n");
}

/// Index assignment is the other mutation the write barrier guards, and it
/// needs the same treatment as `push` — including rebinding the root to the
/// copy, without which the write would land on a list nobody reads.
#[test]
fn index_assignment_through_an_alias_does_not_affect_the_original() {
    let src = r#"
mut a = [1, 2, 3]
mut b = a
b[0] = 99
print(a)
print(b)
"#;
    assert_eq!(run(src), "[1, 2, 3]\n[99, 2, 3]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 3]\n[99, 2, 3]\n");
    assert_eq!(run_cow_verify(src), "[1, 2, 3]\n[99, 2, 3]\n");
}

/// The elision that makes this worth doing: a list only ever *read* — passed
/// to a function many times, never mutated — is never copied. This is
/// `benches/life.frog`'s shape reduced to a checkable size; under Stage 6 it
/// deep-copied the whole grid on each of the nine calls per cell.
#[test]
fn passing_a_nested_list_to_a_function_repeatedly_does_not_copy_it() {
    let src = r#"
func at(rows: List<List<Int>>, y: Int, x: Int): Int = rows[y][x]
let grid = [[1, 2], [3, 4]]
mut total = 0
for y in 0..2 do {
    for x in 0..2 do { total = total + at(grid, y, x) }
}
print(total)
print(grid)
"#;
    assert_eq!(run(src), "10\n[[1, 2], [3, 4]]\n");
    assert_eq!(run_gc_stress(src), "10\n[[1, 2], [3, 4]]\n");
    assert_eq!(run_cow_verify(src), "10\n[[1, 2], [3, 4]]\n");
}
