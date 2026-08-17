//! Regression tests for `and`/`or` short-circuiting in codegen.
//!
//! Codegen used to lower both to eager `band`/`bor`, which compute the *same
//! boolean value* as short-circuit evaluation always does — so a value-only
//! assertion can't tell short-circuiting apart from the eager bug. These
//! tests instead observe whether the right-hand side actually *ran*, via
//! `print`'s side effect on stdout, by running the compiled binary as a
//! subprocess (in-process `compile_and_run` has no stdout to capture).

use std::process::Command;

/// Run `src` through the `froglang-core` binary and return its stdout.
/// Writes `src` to a scratch `.frog` file, avoiding a dependency on the
/// `tempfile` crate; the file is removed again once the process exits.
fn run(src: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "frog_test_{}_{}.frog",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::write(&path, src).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_froglang-core"))
        .args(["run", path.to_str().unwrap()])
        .output()
        .expect("failed to run froglang-core binary");
    let _ = std::fs::remove_file(&path);

    assert!(output.status.success(), "program failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

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
