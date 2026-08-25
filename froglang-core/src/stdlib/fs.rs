//! File and stdio functions — `plans/STDLIB.md` Phase 4.
//!
//! Blocking, thin wrappers over `std::fs`/`std::io` — `plans/CONCURRENCY.md`
//! makes I/O a swappable value in the ambient context *eventually*, but
//! there's no scheduler for it to swap into yet, so today's contract is
//! just "call, block, get an answer." Keeping each function a direct,
//! isolated wrapper (no shared buffering state, no partial reads spread
//! across calls beyond what the OS/`std::io::Stdin`'s own process-global
//! lock already gives sequential `read_line` calls) is what keeps that
//! future swap a one-file change rather than a rewrite.
//!
//! Every fallible function here returns `Result<_, ErrMsg>` — `stdlib`'s
//! shared error type (`mod.rs`) — never a bare `Result<_, String>`
//! (`host.rs`'s `ToFrog for Result<T, E>` doc comment, precondition 5) and
//! never `Result<(), ErrMsg>` (precondition 4: `()` contributes zero union
//! leaves, breaking this impl's one-leaf-per-member assumption). That's why
//! `write_file`/`append_file` return the byte count written instead of
//! `None` on success — a real, useful `Ok` value rather than a placeholder
//! chosen to route around a marshalling limitation.

use std::io::{Read, Write};

use crate::frog_fn;
use crate::state::FrogStateBuilder;
use crate::stdlib::ErrMsg;

pub(super) fn install(builder: FrogStateBuilder) -> FrogStateBuilder {
    builder
        .func(read_file_host())
        .func(write_file_host())
        .func(append_file_host())
        .func(file_exists_host())
        .func(write_stdout_host())
        .func(write_stderr_host())
        .func(flush_stdout_host())
        .func(read_line_host())
        .func(read_stdin_all_host())
}

#[frog_fn]
fn read_file(path: String) -> Result<String, ErrMsg> {
    std::fs::read_to_string(&path).map_err(|e| ErrMsg(format!("{}: {}", path, e)))
}

/// Truncates and creates as needed (`std::fs::write`'s own semantics), like
/// most languages' `write_file`/`writeFile`. Returns bytes written on
/// success, not the number of bytes in `contents` sight-unseen — the two
/// only differ if the write is somehow short, which `std::fs::write`
/// doesn't expose, but keeping the value honestly "what actually happened"
/// costs nothing.
#[frog_fn]
fn write_file(path: String, contents: String) -> Result<i64, ErrMsg> {
    std::fs::write(&path, contents.as_bytes())
        .map(|_| contents.len() as i64)
        .map_err(|e| ErrMsg(format!("{}: {}", path, e)))
}

#[frog_fn]
fn append_file(path: String, contents: String) -> Result<i64, ErrMsg> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(contents.as_bytes()).map(|_| contents.len() as i64))
        .map_err(|e| ErrMsg(format!("{}: {}", path, e)))
}

#[frog_fn]
fn file_exists(path: String) -> bool {
    std::path::Path::new(&path).exists()
}

/// Raw bytes, no trailing newline and no debug-quoting — unlike `print`,
/// which always adds a newline and (for non-`Str` types) a `Debug`-style
/// rendering. This is `print`'s lower-level sibling for anything that wants
/// to control its own formatting/line-endings — a progress bar, a TUI
/// escape sequence, a partial line built up across several calls.
#[frog_fn]
fn write_stdout(s: String) -> Result<i64, ErrMsg> {
    std::io::stdout().write_all(s.as_bytes())
        .map(|_| s.len() as i64)
        .map_err(|e| ErrMsg(e.to_string()))
}

#[frog_fn]
fn write_stderr(s: String) -> Result<i64, ErrMsg> {
    std::io::stderr().write_all(s.as_bytes())
        .map(|_| s.len() as i64)
        .map_err(|e| ErrMsg(e.to_string()))
}

/// Terminals and most files are line-buffered or unbuffered already, but a
/// TUI mid-frame or a progress indicator without a trailing newline needs
/// this to actually appear. Infallible (`bool`, not `Result`) rather than
/// `Result<(), ErrMsg>` — deliberately, per this module's doc comment — and
/// a flush failure is rare enough in practice that collapsing it to `false`
/// costs little.
#[frog_fn]
fn flush_stdout() -> bool {
    std::io::stdout().flush().is_ok()
}

/// One line from stdin, newline stripped (`\n` or `\r\n`). `Err("EOF")` at
/// end of input — not `Str?` (`Str | None`): seeing EOF as a legitimate,
/// non-error outcome would be nicer, but `Result<T, ()>`/`Option`-shaped
/// unions aren't supported yet (`host.rs`, precondition 4). A caller that
/// wants a clean end-of-input loop matches `e.msg == "EOF"` in the `catch`
/// handler and treats anything else as a real error.
#[frog_fn]
fn read_line() -> Result<String, ErrMsg> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => Err(ErrMsg("EOF".to_string())),
        Ok(_) => {
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') { buf.pop(); }
            }
            Ok(buf)
        }
        Err(e) => Err(ErrMsg(e.to_string())),
    }
}

/// The rest of stdin, to EOF — piped/redirected input, not interactive
/// line-at-a-time reading (`read_line` above).
#[frog_fn]
fn read_stdin_all() -> Result<String, ErrMsg> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)
        .map(|_| buf)
        .map_err(|e| ErrMsg(e.to_string()))
}
