//! Regression tests for `print`'s scalar-to-string coercions.

use std::process::Command;

fn run(src: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "frog_print_test_{}_{}.frog",
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

#[test]
fn print_coerces_scalar_values_to_text() {
    assert_eq!(run("print(3)\nprint(1.5)\nprint(true)\nprint(\"ok\")\nprint([1, 2])"), "3\n1.5\ntrue\nok\n[1, 2]\n");
}
