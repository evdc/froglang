//! Shared helpers for the integration tests that have to observe a
//! froglang program's *process* behaviour — its stdout, its stderr, its
//! exit status — rather than just the value `compile_and_run` returns.
//! Short-circuiting, `print`, and the runtime-abort paths all fall in that
//! category, since none of them are visible from an in-process call.
//!
//! `tests/common/` is a module directory, not a test target, so cargo
//! compiles this into each test binary that declares `mod common;` instead
//! of running it as a test of its own.

// Each test binary pulls in the whole module but uses only the helpers it
// needs, so anything the others use looks dead from here.
#![allow(dead_code)]

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Distinguishes concurrently-running scratch files within one test binary.
///
/// The obvious `pid + nanos` naming this replaces was the cause of two
/// long-standing intermittent failures: cargo runs a binary's tests on
/// several threads of a *single* process, so the pid is identical across
/// them, leaving only the timestamp — and two tests that reach this line in
/// the same clock tick then write and delete each other's file. A counter
/// makes the name collision-free by construction. The pid is still in there
/// so separate test binaries running in parallel can't collide either.
static SCRATCH_SEQ: AtomicUsize = AtomicUsize::new(0);

/// A scratch `.frog` file that deletes itself when it goes out of scope —
/// including when a test panics mid-assert, which a bare `remove_file` at
/// the end of the function would skip. Avoids a dependency on `tempfile`.
struct ScratchFile {
    path: std::path::PathBuf,
}

impl ScratchFile {
    fn new(src: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "frog_test_{}_{}.frog",
            std::process::id(),
            SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::write(&path, src).expect("failed to write scratch program");
        ScratchFile { path }
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What `froglang-core run <program>` did: everything a test might want to
/// assert on. `status` is `None` only if the process was killed by a signal.
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: Option<i32>,
}

/// Run `src` through the `froglang-core` binary and return its full output,
/// whether it succeeded or not. Use this for programs expected to fail; use
/// [`run`] for programs expected to succeed.
pub fn run_raw(src: &str) -> RunOutput {
    let scratch = ScratchFile::new(src);
    let output = Command::new(env!("CARGO_BIN_EXE_froglang-core"))
        .args(["run", scratch.path.to_str().expect("scratch path is valid UTF-8")])
        .output()
        .expect("failed to run froglang-core binary");
    RunOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status.code(),
    }
}

/// Run `src` with `FROG_GC_STRESS=1` and return its stdout, asserting it
/// exited successfully.
///
/// `GcHeap::new` reads that variable once per process, so forcing a
/// collection on every allocation is only reachable from a subprocess —
/// which is what makes this a separate helper rather than a flag on an
/// in-process `compile_and_run`. Without it most programs never collect at
/// all (the normal threshold is 1 MB), so a missing or clobbered GC root
/// stays invisible.
pub fn run_gc_stress(src: &str) -> String {
    let scratch = ScratchFile::new(src);
    let output = Command::new(env!("CARGO_BIN_EXE_froglang-core"))
        .args(["run", scratch.path.to_str().expect("scratch path is valid UTF-8")])
        .env("FROG_GC_STRESS", "1")
        .output()
        .expect("failed to run froglang-core binary");
    assert!(
        output.status.success(),
        "program was expected to succeed but exited with {:?}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Run `src` and return its stdout, asserting it exited successfully.
pub fn run(src: &str) -> String {
    let out = run_raw(src);
    assert_eq!(
        out.status,
        Some(0),
        "program was expected to succeed but exited with {:?}\n--- stderr ---\n{}",
        out.status, out.stderr,
    );
    out.stdout
}
