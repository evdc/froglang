//! Regression tests for `and`/`or` short-circuiting in codegen.
//!
//! Codegen used to lower both to eager `band`/`bor`, which compute the *same
//! boolean value* as short-circuit evaluation always does — so a value-only
//! assertion can't tell short-circuiting apart from the eager bug. These
//! tests instead observe whether the right-hand side actually *ran*, via
//! `print`'s side effect on stdout, by running the compiled binary as a
//! subprocess (in-process `compile_and_run` has no stdout to capture).

mod common;
use common::run;

// These rely on the CLI's own auto-print of the program's trailing expression
// value (which handles `Bool` directly) to
// surface the `and`/`or` result — `noisy()`'s `print("evaluated")` call is
// what surfaces whether the right-hand side actually ran.

#[test]
fn test_and_short_circuits_on_false_left() {
    let out = run(r#"
func noisy(): Bool = { print("evaluated"); true }
false and noisy()
"#);
    assert_eq!(out, "false\n", "right-hand side of `and` must not run when the left side is false");
}

#[test]
fn test_or_short_circuits_on_true_left() {
    let out = run(r#"
func noisy(): Bool = { print("evaluated"); false }
true or noisy()
"#);
    assert_eq!(out, "true\n", "right-hand side of `or` must not run when the left side is true");
}

#[test]
fn test_and_evaluates_right_when_needed() {
    let out = run(r#"
func noisy(): Bool = { print("evaluated"); true }
true and noisy()
"#);
    assert_eq!(out, "evaluated\ntrue\n");
}

#[test]
fn test_or_evaluates_right_when_needed() {
    let out = run(r#"
func noisy(): Bool = { print("evaluated"); false }
false or noisy()
"#);
    assert_eq!(out, "evaluated\nfalse\n");
}
