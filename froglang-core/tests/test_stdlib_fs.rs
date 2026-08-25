//! File and stdio stdlib functions — `plans/STDLIB.md` Phase 4
//! (`froglang-core/src/stdlib/fs.rs`).
//!
//! File-touching functions are tested in-process (`FrogState::with_stdlib`)
//! against a scratch file, same convention as `test_host_fns.rs`.
//! `read_line`/`read_stdin_all` need to control the test *process*'s own
//! stdin, which an in-process `FrogState` can't do (it shares the test
//! binary's real stdin) — those two go through a subprocess with piped
//! stdin instead, mirroring `tests/common`'s `run`/`run_raw` convention but
//! adding stdin, which that module's helpers don't support.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use froglang_core::state::{FrogState, FrogValue};

fn int(v: &FrogValue) -> i64 {
    match v {
        FrogValue::Int(n) => *n,
        other => panic!("expected Int, got {:?}", other),
    }
}
fn string(v: &FrogValue) -> String {
    match v {
        FrogValue::Str(s) => s.clone(),
        other => panic!("expected Str, got {:?}", other),
    }
}
fn boolean(v: &FrogValue) -> bool {
    match v {
        FrogValue::Bool(b) => *b,
        other => panic!("expected Bool, got {:?}", other),
    }
}

static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

/// A scratch file (distinct from `tests/common`'s `ScratchFile`, which
/// holds a frog *program*'s source, not arbitrary file content under test)
/// that deletes itself on drop, including on a mid-assert panic.
struct ScratchPath {
    path: std::path::PathBuf,
}
impl ScratchPath {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "frog_fs_test_{}_{}.txt",
            std::process::id(),
            SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        ScratchPath { path }
    }
    fn as_str(&self) -> &str {
        self.path.to_str().expect("scratch path is valid UTF-8")
    }
}
impl Drop for ScratchPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn write_then_read_round_trips() {
    let scratch = ScratchPath::new();
    let mut s = FrogState::with_stdlib().unwrap();
    let src = format!(
        "let n = write_file(\"{p}\", \"hello\")\n\
         let content = read_file(\"{p}\")\n\
         if content is Str then content else \"read-err\"",
        p = scratch.as_str(),
    );
    let (v, _) = s.eval(&src).unwrap();
    assert_eq!(string(&v), "hello");
}

#[test]
fn write_file_returns_bytes_written() {
    let scratch = ScratchPath::new();
    let mut s = FrogState::with_stdlib().unwrap();
    let src = format!(
        "let n = write_file(\"{p}\", \"hello\")\nif n is Int then n else -1",
        p = scratch.as_str(),
    );
    let (v, _) = s.eval(&src).unwrap();
    assert_eq!(int(&v), 5);
}

#[test]
fn write_file_truncates_a_pre_existing_file() {
    let scratch = ScratchPath::new();
    std::fs::write(&scratch.path, "a much longer previous line of content").unwrap();
    let mut s = FrogState::with_stdlib().unwrap();
    let src = format!(
        "let n = write_file(\"{p}\", \"hi\")\n\
         let content = read_file(\"{p}\")\n\
         if content is Str then content else \"read-err\"",
        p = scratch.as_str(),
    );
    let (v, _) = s.eval(&src).unwrap();
    assert_eq!(string(&v), "hi");
}

#[test]
fn append_file_appends_and_creates_if_missing() {
    let scratch = ScratchPath::new();
    let mut s = FrogState::with_stdlib().unwrap();
    let src = format!(
        "let a = append_file(\"{p}\", \"hello\")\n\
         let b = append_file(\"{p}\", \" world\")\n\
         let content = read_file(\"{p}\")\n\
         if content is Str then content else \"read-err\"",
        p = scratch.as_str(),
    );
    let (v, _) = s.eval(&src).unwrap();
    assert_eq!(string(&v), "hello world");
}

#[test]
fn file_exists_true_and_false() {
    let scratch = ScratchPath::new();
    std::fs::write(&scratch.path, "x").unwrap();
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval(&format!("file_exists(\"{}\")", scratch.as_str())).unwrap();
    assert!(boolean(&v));
    let (v, _) = s.eval(&format!("file_exists(\"{}.missing\")", scratch.as_str())).unwrap();
    assert!(!boolean(&v));
}

/// A missing file is a caught `ErrMsg`, not a Rust panic or process abort —
/// `std::fs::read_to_string`'s `Err` is mapped, never `.unwrap()`-ed, inside
/// the shim.
#[test]
fn read_file_missing_path_is_a_catchable_error() {
    let scratch = ScratchPath::new();
    let mut s = FrogState::with_stdlib().unwrap();
    let src = format!(
        "read_file(\"{p}.missing\") catch [e] -> e.msg",
        p = scratch.as_str(),
    );
    let (v, _) = s.eval(&src).unwrap();
    assert!(string(&v).contains("missing"), "{:?}", v);
}

#[test]
fn write_stdout_and_write_stderr_return_bytes_written() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval("let n = write_stdout(\"hi\")\nif n is Int then n else -1").unwrap();
    assert_eq!(int(&v), 2);
    let (v, _) = s.eval("let n = write_stderr(\"hello\")\nif n is Int then n else -1").unwrap();
    assert_eq!(int(&v), 5);
}

#[test]
fn flush_stdout_succeeds() {
    let mut s = FrogState::with_stdlib().unwrap();
    let (v, _) = s.eval("flush_stdout()").unwrap();
    assert!(boolean(&v));
}

/// `FROG_GC_STRESS=1` across a file round trip — `read_file`'s `Ok(String)`
/// allocates inside the shim's `FrogCtx::scope()` exactly like every other
/// allocating shim already covered elsewhere; this is the file-IO-specific
/// instance of that same guarantee.
#[test]
fn file_round_trip_survives_gc_stress() {
    let scratch = ScratchPath::new();
    std::env::set_var("FROG_GC_STRESS", "1");
    let mut s = FrogState::with_stdlib().unwrap();
    let src = format!(
        "let n = write_file(\"{p}\", \"stress test content\")\n\
         let content = read_file(\"{p}\")\n\
         if content is Str then content else \"read-err\"",
        p = scratch.as_str(),
    );
    let (v, _) = s.eval(&src).unwrap();
    std::env::remove_var("FROG_GC_STRESS");
    assert_eq!(string(&v), "stress test content");
}

// ── stdin (subprocess — needs to control the process's own stdin) ─────────

fn run_with_stdin(src: &str, stdin_data: &str) -> (String, String, Option<i32>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_froglang-core"))
        .args(["run", src])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn froglang-core binary");
    child.stdin.take().unwrap().write_all(stdin_data.as_bytes()).unwrap();
    let output = child.wait_with_output().expect("failed to wait on froglang-core binary");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

#[test]
fn read_line_reads_sequential_lines() {
    let (stdout, stderr, status) = run_with_stdin(
        "let a = read_line()\n\
         let b = read_line()\n\
         let av = if a is Str then a else \"ERRA\"\n\
         let bv = if b is Str then b else \"ERRB\"\n\
         av + \"|\" + bv",
        "first\nsecond\n",
    );
    assert_eq!(status, Some(0), "stderr: {}", stderr);
    assert_eq!(stdout.trim(), "first|second");
}

#[test]
fn read_line_at_eof_is_a_catchable_error() {
    let (stdout, stderr, status) = run_with_stdin(
        "read_line() catch [e] -> e.msg",
        "",
    );
    assert_eq!(status, Some(0), "stderr: {}", stderr);
    assert_eq!(stdout.trim(), "EOF");
}

#[test]
fn read_stdin_all_reads_everything_to_eof() {
    let (stdout, stderr, status) = run_with_stdin(
        "let a = read_stdin_all()\nif a is Str then a else \"ERR\"",
        "line1\nline2\n",
    );
    assert_eq!(status, Some(0), "stderr: {}", stderr);
    // `main.rs`'s `run()` prints a `Str` result via `println!`, adding its
    // own trailing newline on top of the two already read from stdin.
    assert_eq!(stdout, "line1\nline2\n\n");
}
