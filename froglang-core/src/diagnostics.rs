//! rustc-style span rendering — `plans/DATA.md` Stage 3.
//!
//! `Span`/`Position` (`frontend/tokens.rs`) carry no filename, and nothing
//! upstream threads the original source text down to error-formatting time
//! either — `FrogState::eval_with_base` used to discard both, converting a
//! `Spanned<TypeError>` straight to `e.to_string()` (`Span`'s own
//! `(line:col .. line:col)` form plus the message, no source line, no
//! caret). This is the renderer that fixes that, and `FrogState`'s
//! `entry_sources` (`state.rs`) is what supplies the filename/source pair it
//! needs — one entry per `eval`/`eval_file` call.

use crate::frontend::tokens::Span;

/// Render a single-message rustc-style diagnostic:
///
/// ```text
/// error: <msg>
///  --> <filename>:<line>:<col>
///   |
/// 3 | let x = y +
///   |             ^
/// ```
///
/// `span.start.line`/`.col` are 0-indexed (`Lexer::current_line`/`current_col`'s
/// convention — see `lexer.rs`); the gutter and `-->` position printed here
/// are the usual 1-indexed editor convention, so both get `+ 1` on the way
/// out. A line past the end of `source` (an out-of-sync span, or a synthetic
/// zero-span) renders an empty source line rather than panicking —
/// diagnostics must never be the thing that crashes over a bad span.
pub fn render_span(filename: &str, source: &str, span: Span, msg: &str) -> String {
    let line_no = span.start.line as usize + 1;
    let col_no = span.start.col as usize + 1;
    let line_text = source.lines().nth(line_no - 1).unwrap_or("");

    let caret_len = if span.end.line == span.start.line && span.end.col > span.start.col {
        (span.end.col - span.start.col) as usize
    } else {
        1
    };

    let gutter = line_no.to_string();
    let pad = " ".repeat(gutter.len());
    let caret_pad = " ".repeat(col_no - 1);
    let caret = "^".repeat(caret_len);

    format!(
        "error: {msg}\n{pad} --> {filename}:{line_no}:{col_no}\n{pad} |\n{gutter} | {line_text}\n{pad} | {caret_pad}{caret}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::tokens::{Position, Span};

    fn span(sl: u32, sc: u32, el: u32, ec: u32) -> Span {
        Span { start: Position { line: sl, col: sc }, end: Position { line: el, col: ec } }
    }

    #[test]
    fn renders_the_offending_line_with_a_caret_under_the_span() {
        // 0-indexed input (the lexer's own convention): line 1 = source's
        // second line, col 12 = 'z''s position in "let y = x + z".
        let src = "let x = 1\nlet y = x + z\n";
        let out = render_span("<repl>", src, span(1, 12, 1, 13), "unknown name 'z'");
        assert_eq!(
            out,
            "error: unknown name 'z'\n  --> <repl>:2:13\n  |\n2 | let y = x + z\n  |             ^"
        );
    }

    #[test]
    fn multi_column_span_gets_a_matching_caret_width() {
        let src = "foo + bar\n";
        let out = render_span("f.frog", src, span(0, 0, 0, 3), "type mismatch");
        assert!(out.contains("1 | foo + bar"), "{out}");
        assert!(out.ends_with("^^^"), "{out}");
    }

    #[test]
    fn a_span_past_the_end_of_source_does_not_panic() {
        let out = render_span("<repl>", "", span(4, 0, 4, 1), "oops");
        assert!(out.contains("5 | "), "{out}");
    }
}
