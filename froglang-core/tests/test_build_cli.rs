//! `frog build` end to end: compile, link against the runtime staticlib, run
//! the binary. Each program is also run through `frog run` and the two must
//! agree — the AOT path's specification is "behaves like the JIT".

use std::path::PathBuf;
use std::process::{Command, Output};

const FROG: &str = env!("CARGO_BIN_EXE_froglang-core");

/// `frog build` links `libfroglang_core.a` from next to the compiler binary,
/// but `cargo test` only rebuilds the rlib — so without this every test here
/// would silently exercise whatever runtime the last `cargo build` produced.
/// Rebuilt once per test binary; a no-op when it's already fresh.
fn ensure_runtime_lib() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--lib", "-p", "froglang-core"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status().unwrap();
        assert!(status.success(), "building the runtime staticlib failed");
    });
}

/// A per-test scratch dir: named by test, so parallel tests never share one.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("frog_build_cli_{}_{}", std::process::id(), test));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build `src` and run the binary (optionally under `FROG_GC_STRESS`).
fn build_and_run(test: &str, src: &str, gc_stress: bool) -> Output {
    ensure_runtime_lib();
    let dir = scratch(test);
    let out = dir.join("prog");
    let build = Command::new(FROG).arg("build").arg(src).arg("-o").arg(&out)
        .output().unwrap();
    assert!(build.status.success(), "build failed:\n{}{}",
        String::from_utf8_lossy(&build.stdout), String::from_utf8_lossy(&build.stderr));
    let mut cmd = Command::new(&out);
    if gc_stress {
        cmd.env("FROG_GC_STRESS", "1");
    }
    let run = cmd.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    run
}

/// `src` must produce the same stdout, stderr and exit code built as run.
/// Programs here end in a `None`-valued expression, so `frog run` prints no
/// trailing result that a built binary (which discards it) wouldn't.
fn assert_aot_matches_jit(test: &str, src: &str) -> String {
    let jit = Command::new(FROG).arg("run").arg(src).output().unwrap();
    for stress in [false, true] {
        let aot = build_and_run(test, src, stress);
        assert_eq!(String::from_utf8_lossy(&aot.stdout), String::from_utf8_lossy(&jit.stdout),
            "stdout differs (gc_stress={})", stress);
        assert_eq!(String::from_utf8_lossy(&aot.stderr), String::from_utf8_lossy(&jit.stderr),
            "stderr differs (gc_stress={})", stress);
        assert_eq!(aot.status.code(), jit.status.code(), "exit code differs (gc_stress={})", stress);
    }
    String::from_utf8(jit.stdout).unwrap()
}

/// `build` type-checks against the stdlib prelude (`IndexError`), not a bare
/// `TypeChecker::new()`.
#[test]
fn build_accepts_stdlib_prelude_types() {
    let out = assert_aot_matches_jit("prelude",
        "let xs = [1, 2, 3]\nlet r = xs.get(1)\nprint(if r is Int then \"ok\" else \"err\")");
    assert_eq!(out, "ok\n");
}

#[test]
fn check_accepts_stdlib_prelude_types() {
    let check = Command::new(FROG).arg("check").arg("let xs = [1, 2, 3]\nxs.get(1)")
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(check.status.success(), "check failed: {}", stdout);
    assert!(stdout.contains("IndexError"), "unexpected type: {}", stdout);
}

/// A stdlib host function links (its shim is exported under the name the
/// object imports) and runs with the AOT `FrogCtx`.
#[test]
fn build_calls_stdlib_host_function() {
    let out = assert_aot_matches_jit("slice_ok", "print(\"hello\".slice(1, 3)!)");
    assert_eq!(out, "el\n");
}

/// The error half of a host `Result` marshals into a catchable `ErrMsg`.
#[test]
fn build_catches_stdlib_host_error() {
    let out = assert_aot_matches_jit("slice_err",
        "print(slice(\"hello\", 3, 1) catch [e] -> e.msg)");
    assert!(out.contains("out of range"), "unexpected output: {}", out);
}

/// Host calls interleaved with allocation over many frames, under GC stress.
#[test]
fn build_host_calls_survive_collections() {
    let out = assert_aot_matches_jit("slice_loop",
        "func go(i: Int, acc: Str): Str = if i >= 2000 then acc else go(i + 1, slice(to_upper(acc + \"xy\"), 0, 4)!)\nprint(go(0, \"ab\"))");
    assert_eq!(out, "ABXY\n");
}

/// A built binary's `main` isn't Rust's, so nothing flushes stdout at exit
/// unless `frog_rt_main` does — output after the last newline would be lost.
#[test]
fn build_flushes_stdout_without_trailing_newline() {
    let run = build_and_run("flush", "write_stdout(\"no newline\")!", false);
    assert_eq!(String::from_utf8_lossy(&run.stdout), "no newline");
}
