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
