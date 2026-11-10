//! `Abort` (`plans/NOGC.md` §3.8): what `?` does to the values that are live
//! when it fires. On a `?` that returns early, every owned binding whose type
//! provides `Abort` gets `abort(x)` called on it, newest first, before the
//! error is returned. `catch`, `!` and a plain `return` do not.
//!
//! The `abort` bodies here print, which is the only way to observe the order
//! and the absence of a call. Every program runs under the leak oracle.

mod common;
use common::run;

/// A linear `Res` whose abort prints, plus a fallible step that fails when
/// given a negative number.
const PRELUDE: &str = r#"
error Bad(msg: Str)
data Res(id: Int) provides Linear, Abort {
    func abort(r: Res): None = print("abort ${r.id}")
}
func open(id: Int): Res = Res(id=id)
func close(r: Res): None = print("close ${r.id}")
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
"#;

fn prog(body: &str) -> String { format!("{}\n{}", PRELUDE, body) }

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    err.item.msg
}

#[test]
fn a_firing_question_mark_aborts_a_live_linear_value() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    let r = open(1)
    let v = step(n)?
    close(r)
    v
}
match f(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err ${e.msg}")
}
"#);
    assert_eq!(run(&src), "abort 1\nerr neg\n");
}

#[test]
fn a_question_mark_that_does_not_fire_aborts_nothing() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    let r = open(1)
    let v = step(n)?
    close(r)
    v
}
match f(5) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err ${e.msg}")
}
"#);
    assert_eq!(run(&src), "close 1\nok 5\n");
}

#[test]
fn values_are_aborted_newest_first() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    let a = open(1)
    let b = open(2)
    let v = step(n)?
    close(b)
    close(a)
    v
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 2\nabort 1\nerr\n");
}

#[test]
fn a_value_consumed_before_the_edge_is_not_aborted() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    let a = open(1)
    close(a)
    let b = open(2)
    let v = step(n)?
    close(b)
    v
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "close 1\nabort 2\nerr\n");
}

#[test]
fn a_value_opened_after_the_edge_is_not_aborted_by_it() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    let v = step(n)?
    let r = open(7)
    close(r)
    v
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "err\n");
}

#[test]
fn each_edge_aborts_only_what_is_live_at_that_edge() {
    let src = prog(r#"
func f(n: Int, m: Int): Int | Bad = {
    let a = open(1)
    let x = step(n)?
    let b = open(2)
    let y = step(m)?
    close(b)
    close(a)
    x + y
}
match f(1, -1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 2\nabort 1\nerr\n");
}

#[test]
fn an_owned_linear_parameter_is_aborted() {
    let src = prog(r#"
func use_it(r: Res, n: Int): Int | Bad = {
    let v = step(n)?
    close(r)
    v
}
match use_it(open(3), -1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 3\nerr\n");
}

#[test]
fn a_value_consumed_on_the_success_path_of_a_branch_is_still_aborted_on_the_error_edge() {
    let src = prog(r#"
func f(n: Int, flag: Bool): Int | Bad = {
    let r = open(1)
    let v = step(n)?
    if flag then close(r) else close(r)
    v
}
match f(-1, true) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 1\nerr\n");
}

#[test]
fn an_edge_inside_a_branch_sees_the_values_declared_there() {
    let src = prog(r#"
func f(n: Int, flag: Bool): Int | Bad = {
    let a = open(1)
    let v = if flag then {
        let b = open(2)
        let w = step(n)?
        close(b)
        w
    } else 0
    close(a)
    v
}
match f(-1, true) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 2\nabort 1\nerr\n");
}

// ── Edges that do not abort ─────────────────────────────────────────────

#[test]
fn catch_does_not_abort() {
    let src = prog(r#"
func f(n: Int): Int = {
    let r = open(1)
    let v = step(n) catch [e] -> 0
    close(r)
    v
}
print("${f(-1)}")
"#);
    assert_eq!(run(&src), "close 1\n0\n");
}

#[test]
fn a_plain_return_does_not_abort() {
    let src = prog(r#"
func f(n: Int): Int = {
    let r = open(1)
    close(r)
    return n
}
print("${f(4)}")
"#);
    assert_eq!(run(&src), "close 1\n4\n");
}

// ── Non-linear `Abort` ──────────────────────────────────────────────────

#[test]
fn a_plain_abort_type_is_aborted_when_live_and_kept_otherwise() {
    let src = r#"
error Bad(msg: Str)
data Tmp(id: Int) provides Abort {
    func abort(t: Tmp): None = print("undo ${t.id}")
}
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
func f(n: Int): Int | Bad = {
    let t = Tmp(id=9)
    let v = step(n)?
    v + t.id
}
match f(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
match f(1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#;
    assert_eq!(run(src), "undo 9\nerr\nok 10\n");
}

#[test]
fn a_type_without_abort_is_unaffected_by_question_mark() {
    let src = r#"
error Bad(msg: Str)
data Plain(id: Int)
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
func f(n: Int): Int | Bad = {
    let p = Plain(id=1)
    let v = step(n)?
    v + p.id
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#;
    assert_eq!(run(src), "err\n");
}

#[test]
fn an_abort_value_survives_a_loop_of_fallible_steps() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    mut total = 0
    for i in 0..3 do {
        let r = open(i)
        total = total + step(n)?
        close(r)
    }
    total
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 0\nerr\n");
}

// ── Declaration rules ───────────────────────────────────────────────────

#[test]
fn linear_requires_abort() {
    let err = type_error("data Buf(x: Int) provides Linear\n1");
    assert!(err.contains("must also provide Abort"), "{err}");
}

#[test]
fn abort_is_a_declared_trait_and_cannot_be_redeclared() {
    let err = type_error("trait Abort { func abort(x: Self) }\n1");
    assert!(err.contains("Abort"), "{err}");
}

#[test]
fn abort_must_be_implemented_when_provided() {
    let err = type_error("data T(x: Int) provides Abort\n1");
    assert!(err.contains("abort"), "{err}");
}

// ── Exits that leave a Linear value unconsumed ─────────────────────────

fn eval_error(src: &str) -> String {
    froglang_core::state::FrogState::new().eval(src).unwrap_err().to_string()
}

#[test]
fn a_plain_return_with_a_live_linear_value_is_rejected() {
    let err = eval_error(&prog(r#"
func f(bad: Bool): Int = {
    let r = open(1)
    if bad then { return 0 } else none
    close(r)
    1
}
f(true)
"#));
    assert!(err.contains("'r' is Linear and is not consumed before this return"), "{err}");
}

#[test]
fn a_plain_return_opposite_a_consuming_branch_is_rejected() {
    let err = eval_error(&prog(r#"
func f(bad: Bool): Int = {
    let r = open(1)
    if bad then { return 0 } else { close(r) }
    1
}
f(true)
"#));
    assert!(err.contains("'r' is Linear and is not consumed before this return"), "{err}");
}

#[test]
fn a_return_after_consuming_is_accepted() {
    let src = prog(r#"
func f(bad: Bool): Int = {
    let r = open(1)
    if bad then { close(r); return 0 } else { close(r) }
    1
}
print("${f(true)}")
"#);
    assert_eq!(run(&src), "close 1\n0\n");
}

#[test]
fn returning_the_linear_value_itself_consumes_it() {
    let src = prog(r#"
func f(bad: Bool): Res = {
    let r = open(1)
    if bad then { return r } else none
    r
}
close(f(true))
"#);
    assert_eq!(run(&src), "close 1\n");
}

#[test]
fn a_question_mark_that_cannot_reach_a_shadowed_linear_value_is_rejected() {
    let err = eval_error(&prog(r#"
func f(n: Int): Int | Bad = {
    let r = open(1)
    let r = 5
    let v = step(n)?
    v + r
}
1
"#));
    assert!(err.contains("'r' is Linear and is not consumed before this return"), "{err}");
    assert!(err.contains("shadows it"), "{err}");
}

// ── Shadowing inside a nested scope ─────────────────────────────────────

#[test]
fn an_outer_value_shadowed_only_inside_a_branch_is_still_aborted() {
    let src = prog(r#"
func f(n: Int, flag: Bool): Int | Bad = {
    let r = open(1)
    if flag then { let r = 5; print("${r}") } else none
    let v = step(n)?
    close(r)
    v
}
match f(-1, true) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "5\nabort 1\nerr\n");
}

#[test]
fn an_outer_value_shadowed_only_inside_a_loop_is_still_aborted() {
    let src = prog(r#"
func f(n: Int): Int | Bad = {
    let r = open(1)
    for r in 0..1 do print("${r}")
    let v = step(n)?
    close(r)
    v
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "0\nabort 1\nerr\n");
}

#[test]
fn an_outer_value_consumed_before_an_inner_rebinding_is_not_aborted() {
    let src = prog(r#"
func f(n: Int, flag: Bool): Int | Bad = {
    let r = open(1)
    close(r)
    if flag then { let r = open(2); close(r) } else none
    let v = step(n)?
    v
}
match f(-1, true) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "close 1\nclose 2\nerr\n");
}

// ── Parameters the callee only borrows ──────────────────────────────────

/// `t` is only read in `use_tmp`, so it is borrowed: still the caller's, and
/// the caller's own `?` is what aborts it. The callee used to abort it too,
/// which also made `t` consuming, so the caller deep-copied it for the call
/// and the one failure aborted it twice.
const TMP: &str = r#"
error Bad(msg: Str)
data Tmp(id: Int) provides Abort {
    func abort(t: Tmp): None = print("undo ${t.id}")
}
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
func use_tmp(t: Tmp, n: Int): Int | Bad = {
    let v = step(n)?
    v + t.id
}
"#;

#[test]
fn a_borrowed_parameter_is_aborted_once_by_its_owner() {
    let src = format!("{TMP}{}", r#"
func g(n: Int): Int | Bad = {
    let t = Tmp(id=9)
    let v = use_tmp(t, n)?
    v + t.id
}
match g(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "undo 9\nerr\n");
}

#[test]
fn a_value_lent_to_a_borrowing_callee_at_its_last_use_is_still_aborted() {
    let src = format!("{TMP}{}", r#"
func g(n: Int): Int | Bad = {
    let t = Tmp(id=9)
    let v = use_tmp(t, n)?
    v
}
match g(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "undo 9\nerr\n");
}

#[test]
fn a_consumed_parameter_is_aborted_by_the_callee() {
    let src = format!("{TMP}{}", r#"
func keep(t: Tmp, n: Int): Tmp | Bad = {
    let v = step(n)?
    t
}
func g(n: Int): Int | Bad = {
    let t = Tmp(id=9)
    let k = keep(t, n)?
    k.id
}
match g(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "undo 9\nerr\n");
}

// ── However `Linear` is granted, it needs `Abort` ───────────────────────

#[test]
fn a_standalone_linear_grant_without_abort_is_rejected() {
    let err = eval_error("data T(x: Int)\nprovides Linear for T { }\n1");
    assert!(err.contains("'T' provides Linear, so it must also provide Abort"), "{err}");
}

#[test]
fn a_linear_primitive_without_abort_is_rejected() {
    let err = eval_error("provides Linear for Int { }\n1");
    assert!(err.contains("'Int' provides Linear, so it must also provide Abort"), "{err}");
}

#[test]
fn linear_and_abort_may_be_granted_separately() {
    let src = r#"
error Bad(msg: Str)
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
data T(x: Int) provides Linear
provides Abort for T { func abort(t: T): None = print("abort ${t.x}") }
func f(n: Int): Int | Bad = {
    let t = T(x=4)
    let v = step(n)?
    t.x + v
}
match f(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#;
    assert_eq!(run(src), "abort 4\nerr\n");
}

/// A nominal union's `abort` is indexed under its variants, so `?` used to
/// find no impl for the union and could not abort it.
#[test]
fn a_linear_nominal_union_is_aborted() {
    let src = r#"
error Bad(msg: Str)
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
data R is A(x: Int) | B(y: Int) provides Linear, Abort {
    func abort(r: R): None = print("abort")
}
func close(r: R): Int = match r {
    is A(x) then x
    is B(y) then y
}
func f(n: Int): Int | Bad = {
    let r: R = A(x=1)
    let v = step(n)?
    close(r) + v - 1
}
match f(-1) {
    is Int(v) then print("ok")
    is Bad(e) then print("err")
}
match f(1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#;
    assert_eq!(run(src), "abort\nerr\nok 1\n");
}

const UNION: &str = r#"
error Bad(msg: Str)
func step(n: Int): Int | Bad = if n < 0 then Bad(msg="neg") else n
func close(r: R): Int = match r {
    is A(x) then x
    is B(y) then y
}
data R is A(x: Int) | B(y: Int) provides Linear, Abort {
    func abort(r: R): None = print("abort ${close(r)}")
}
"#;

/// Inside an arm that narrows `r`, the narrowed `r` is what `?` aborts —
/// widened back into the union for the union's `abort` — and the match's
/// own subject temporary is not aborted as well.
#[test]
fn a_question_mark_inside_a_narrowed_arm_aborts_the_narrowed_value_once() {
    let src = format!("{}{}", UNION, r#"
func f(n: Int): Int | Bad = {
    let r: R = A(x=1)
    match r {
        is A then { let v = step(n)?; close(r) + v }
        is B then close(r)
    }
}
match f(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
match f(2) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err")
}
"#);
    assert_eq!(run(&src), "abort 1\nerr\nok 3\n");
}

#[test]
fn abort_can_be_called_directly_on_a_linear_union() {
    let src = format!("{}{}", UNION, r#"
func f(): None = {
    let r: R = B(y=7)
    Abort.abort(r)
}
f()
"#);
    assert_eq!(run(&src), "abort 7\n");
}

/// `Keep` is Linear only because it holds a `Res`; an `Abort` impl alone is
/// what lets `?` abort it.
#[test]
fn a_type_linear_by_containment_is_aborted_through_its_own_impl() {
    let src = prog(r#"
data Keep(r: Res) provides Abort {
    func abort(k: Keep): None = Abort.abort(k.r)
}
func f(n: Int): Int | Bad = {
    let k = Keep(r=open(1))
    let v = step(n)?
    close(k.r)
    v
}
match f(-1) {
    is Int(v) then print("ok ${v}")
    is Bad(e) then print("err ${e.msg}")
}
"#);
    assert_eq!(run(&src), "abort 1\nerr neg\n");
}
