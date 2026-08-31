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

/// Reading a list for its length creates no alias, so it must not mark the
/// list shared — otherwise the next `push` copies it, and a loop that
/// queries `len` as it builds is quadratic.
///
/// This is a behavioural test of an elision, so it asserts the *output*
/// (which never depended on the bug) and relies on the size to make a
/// regression obvious: at 20000 elements the marked version copied the whole
/// list on every iteration and took ~4x as long. `benches/pipeline.rs` is
/// where the timing lives; this is here to pin the semantics that make the
/// elision legal.
#[test]
fn reading_len_in_a_push_loop_does_not_share_the_list() {
    let src = r#"
mut xs = []
mut seen = 0
for i in 0..2000 do {
    seen = seen + xs.len()
    push(mut xs, i)
}
print(xs.len())
print(seen)
"#;
    // seen is 0+1+...+1999 = 1999000 only if every `len` observed the list
    // the pushes are actually growing — i.e. no copy diverted them.
    assert_eq!(run(src), "2000\n1999000\n");
    assert_eq!(run_cow_verify(src), "2000\n1999000\n");
}

/// The same for `print`, which walks a list and stores nothing.
///
/// Annotated rather than inferred from `[]` only to dodge an unrelated,
/// pre-existing bug: an unannotated `mut xs = []` leaves the element type an
/// unresolved type variable on the `Var` node inside the loop, and `print`'s
/// type-directed codegen emits `<?>` placeholders for it. Nothing to do with
/// aliasing — it reproduces identically before Stage 7.
#[test]
fn printing_a_list_in_a_push_loop_does_not_share_it() {
    let src = r#"
mut xs: List<Int> = []
for i in 0..3 do {
    print(xs)
    push(mut xs, i)
}
print(xs)
"#;
    assert_eq!(run(src), "[]\n[0]\n[0, 1]\n[0, 1, 2]\n");
    assert_eq!(run_cow_verify(src), "[]\n[0]\n[0, 1]\n[0, 1, 2]\n");
}

// ── Stage 8: mutable places ─────────────────────────────────────────────────
//
// Before Stage 8 a mutation's target had to be a bare `mut` binding for
// `push` but could be a whole field/index path for assignment, so
// `b.items[0] = v` was accepted while `push(mut b.items, v)` was not, and
// `rows[y][x] = v` was rejected outright. Both now take the same `Place`,
// and `codegen::emit_place_container` walks it, unsharing every list on the
// way down and writing each private copy back into its parent slot.

/// The inconsistency Stage 8 closed: pushing through a struct field.
#[test]
fn pushing_through_a_struct_field_works() {
    let src = r#"
data Box(items: List<Int>)
mut b = Box(items=[1, 2])
push(mut b.items, 3)
print(b.items)
"#;
    assert_eq!(run(src), "[1, 2, 3]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 3]\n");
    assert_eq!(run_cow_verify(src), "[1, 2, 3]\n");
}

/// Copying a struct aliases the lists hanging off it, so mutating one
/// through the original must not be visible through the copy.
///
/// This was a real soundness hole, and it predates Stage 8 — the
/// index-assign half of it (`b.items[0] = 99`) has always been legal.
/// `mark_shared_if_aliased` only marked a value whose *own* type was
/// `List`, so copying the enclosing struct marked nothing; see its doc
/// comment.
#[test]
fn copying_a_struct_does_not_share_its_list_field() {
    let src = r#"
data Box(items: List<Int>)
mut b = Box(items=[1, 2])
let c = b
b.items[0] = 99
push(mut b.items, 3)
print(c.items)
print(b.items)
"#;
    assert_eq!(run(src), "[1, 2]\n[99, 2, 3]\n");
    assert_eq!(run_gc_stress(src), "[1, 2]\n[99, 2, 3]\n");
    assert_eq!(run_cow_verify(src), "[1, 2]\n[99, 2, 3]\n");
}

/// Assignment through nested list indices — the Game-of-Life shape, and the
/// one `lower_place_assign` used to reject with "assignment through more
/// than one list index isn't supported yet".
#[test]
fn assigning_through_nested_indices_works() {
    let src = r#"
mut rows = [[1, 2], [3, 4]]
rows[1][0] = 99
print(rows)
"#;
    assert_eq!(run(src), "[[1, 2], [99, 4]]\n");
    assert_eq!(run_gc_stress(src), "[[1, 2], [99, 4]]\n");
    assert_eq!(run_cow_verify(src), "[[1, 2], [99, 4]]\n");
}

/// ...and pushing to an inner list reached by index.
#[test]
fn pushing_through_a_list_index_works() {
    let src = r#"
mut rows = [[1, 2], [3, 4]]
push(mut rows[0], 7)
print(rows)
"#;
    assert_eq!(run(src), "[[1, 2, 7], [3, 4]]\n");
    assert_eq!(run_gc_stress(src), "[[1, 2, 7], [3, 4]]\n");
    assert_eq!(run_cow_verify(src), "[[1, 2, 7], [3, 4]]\n");
}

/// A path that alternates field and index steps, so the walk has to add a
/// within-element field offset between two `[index]` steps rather than
/// descending straight down.
#[test]
fn a_mixed_field_and_index_path_works() {
    let src = r#"
data Grid(cells: List<List<Int>>)
mut g = Grid(cells=[[1, 2], [3, 4]])
g.cells[1][1] = 42
push(mut g.cells[0], 9)
print(g.cells)
"#;
    assert_eq!(run(src), "[[1, 2, 9], [3, 42]]\n");
    assert_eq!(run_gc_stress(src), "[[1, 2, 9], [3, 42]]\n");
    assert_eq!(run_cow_verify(src), "[[1, 2, 9], [3, 42]]\n");
}

/// Value semantics through a nested write: an alias of the *outer* list must
/// not observe a mutation of an inner one. This is what forces
/// `emit_place_container` to unshare every list on the path, not just the
/// innermost — writing into `rows[0]`'s buffer would otherwise be visible
/// through `copy`, which reaches the same inner list.
#[test]
fn a_nested_write_does_not_leak_through_an_outer_alias() {
    let src = r#"
mut rows = [[1, 2], [3, 4]]
let copy = rows
rows[0][0] = 99
push(mut rows[1], 7)
print(copy)
print(rows)
"#;
    assert_eq!(run(src), "[[1, 2], [3, 4]]\n[[99, 2], [3, 4, 7]]\n");
    assert_eq!(run_gc_stress(src), "[[1, 2], [3, 4]]\n[[99, 2], [3, 4, 7]]\n");
    assert_eq!(run_cow_verify(src), "[[1, 2], [3, 4]]\n[[99, 2], [3, 4, 7]]\n");
}

/// The other direction: a binding extracted *out* of the outer list must not
/// see a later nested write, and must not have its own pushes leak back in.
#[test]
fn an_extracted_inner_list_is_independent_of_a_nested_write() {
    let src = r#"
mut rows = [[1, 2], [3, 4]]
mut inner = rows[0]
rows[0][0] = 99
push(mut inner, 7)
print(inner)
print(rows)
"#;
    assert_eq!(run(src), "[1, 2, 7]\n[[99, 2], [3, 4]]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 7]\n[[99, 2], [3, 4]]\n");
    assert_eq!(run_cow_verify(src), "[1, 2, 7]\n[[99, 2], [3, 4]]\n");
}

/// One list stored twice in an outer list is still one object, so pushing
/// through one slot must not be visible through the other, nor through the
/// binding it came from.
#[test]
fn pushing_through_one_slot_does_not_affect_a_twin_slot() {
    let src = r#"
mut xs = [1, 2]
mut rows = [xs, xs]
push(mut rows[0], 99)
print(xs)
print(rows)
"#;
    assert_eq!(run(src), "[1, 2]\n[[1, 2, 99], [1, 2]]\n");
    assert_eq!(run_gc_stress(src), "[1, 2]\n[[1, 2, 99], [1, 2]]\n");
    assert_eq!(run_cow_verify(src), "[1, 2]\n[[1, 2, 99], [1, 2]]\n");
}

/// A `mut` argument still has to be a place, not an arbitrary expression.
/// `Grammar::mut_prefix` now parses a whole `.field`/`[index]` path instead
/// of a bare identifier, so this is caught in the parser (where an
/// assignment target with a non-identifier root is caught) rather than in
/// typeck.
#[test]
fn push_into_a_non_place_is_rejected() {
    let err = froglang_core::frontend::parser::Parser::parse("mut rows = [[1, 2]]\npush(mut rows.len(), 4)\n1")
        .expect_err("expected a parse error");
    assert!(
        format!("{:?}", err).contains("InvalidAssignmentTarget"),
        "unexpected error: {:?}", err
    );
}

/// The half a dedicated `push` node could never deliver: a *user* function's
/// `mut` parameter accepts a place too, because a `mut` argument is a
/// `typed_ast::Arg::Mut` carrying a `Place` rather than a bare identifier.
#[test]
fn a_user_functions_mut_param_accepts_a_place() {
    let src = r#"
data Box(items: List<Int>)
func add(mut xs: List<Int>): None = { push(mut xs, 9) }
mut b = Box(items=[1, 2])
add(mut b.items)
mut rows = [[1], [2]]
add(mut rows[1])
print(b.items)
print(rows)
"#;
    assert_eq!(run(src), "[1, 2, 9]\n[[1], [2, 9]]\n");
    assert_eq!(run_gc_stress(src), "[1, 2, 9]\n[[1], [2, 9]]\n");
    assert_eq!(run_cow_verify(src), "[1, 2, 9]\n[[1], [2, 9]]\n");
}

/// ...and it stays value-semantic: an alias taken before the call must not
/// observe what the callee writes.
///
/// This is the shape that caught a real bug in the `Arg::Mut` refactor. The
/// old code removed a `mut` argument's root from `live_out`, which was only
/// correct because the argument was a `Var` node whose own transfer added it
/// straight back. A place has no such read, so the removal made `let c = b`
/// a last use, nothing marked the list, and the callee's push landed where
/// `c` could see it. `liveness.rs` now treats the root as read-modify-write.
#[test]
fn a_place_mut_argument_does_not_leak_through_an_earlier_alias() {
    let src = r#"
data Box(items: List<Int>)
func add(mut xs: List<Int>): None = { push(mut xs, 9) }
mut b = Box(items=[1, 2])
let c = b
add(mut b.items)
print(c.items)
print(b.items)
"#;
    assert_eq!(run(src), "[1, 2]\n[1, 2, 9]\n");
    assert_eq!(run_gc_stress(src), "[1, 2]\n[1, 2, 9]\n");
    assert_eq!(run_cow_verify(src), "[1, 2]\n[1, 2, 9]\n");
}
