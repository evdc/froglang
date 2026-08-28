//! Frog notation for the scalar leaves — the printed forms that have to read
//! back as source (plans/DATA.md stage 2 and the law `read(repr(x)) == x`).
//!
//! This module is the counterpart of the lexer: `escape_str` may emit only
//! escapes `Lexer::read_string` accepts, and `float_repr` only forms
//! `Lexer::read_number` and the `inf`/`nan` keywords accept. The two were
//! previously allowed to disagree, by both borrowing Rust's `{:?}`, and they
//! did — `escape_debug` spells a control character `\u{7}`, which froglang's
//! lexer read back as the four characters `u{7}`: wrong, silently. Anything
//! added on one side belongs on the other in the same change.
//!
//! It lives at the crate root because both consumers of a printed value need
//! it — `runtime::ffi` (what a compiled `print` calls) and `state`
//! (what the embedding API renders) — and stage 5's `repr` will be a third.

/// A float in frog notation. Rust's `{:?}` is already shortest-roundtrip,
/// which is the hard half, and `1.0` keeps its `.0` so `Float` and `Int`
/// stay distinguishable in notation. The three non-finite forms are spelled
/// as the lowercase literals the lexer accepts, not Rust's `NaN`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        "nan".to_string()
    } else if f.is_infinite() {
        if f < 0.0 { "-inf".to_string() } else { "inf".to_string() }
    } else {
        format!("{:?}", f)
    }
}

/// A string as a quoted frog literal, escaped so it reads back exactly.
///
/// Only the characters that *must* be escaped are: the two that would end or
/// continue the literal, and the control characters, which have no printable
/// spelling. Everything else — including non-ASCII text — is emitted as
/// itself, since the lexer takes a string literal a `char` at a time and has
/// no opinion about anything but `"` and `\`.
pub fn escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            '"'  => out.push_str("\\\""),
            '\0' => out.push_str("\\0"),
            // Every other control character goes through `\u{...}`, which is
            // why the lexer had to learn that escape: without it these are
            // unspellable, and `repr` would have to reject the string.
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::lexer::Lexer;
    use crate::frontend::tokens::Token;

    /// Lex a single token, asserting there were no errors.
    fn lex_one(src: &str) -> Token {
        let (tokens, errors) = Lexer::new(src).collect_errors();
        assert!(errors.is_empty(), "{}: {:?}", src, errors);
        assert_eq!(tokens.len(), 1, "{}: {:?}", src, tokens);
        tokens.into_iter().next().unwrap().item
    }

    /// The point of the module: what it prints is what the lexer reads.
    /// Written as a round-trip rather than as expected strings so the two
    /// sides cannot drift without a failure here.
    #[test]
    fn floats_round_trip_through_the_lexer() {
        for f in [0.0, 1.0, 0.1 + 0.2, -2.5, 1e100, 1e-300, f64::MAX, f64::MIN_POSITIVE] {
            // A negative literal is unary minus applied to a positive one,
            // so it is two tokens; check the magnitude and the sign apart.
            let text = float_repr(f);
            let text = text.strip_prefix('-').unwrap_or(&text);
            match lex_one(text) {
                Token::Float(g) => assert_eq!(f.abs(), g, "{}", text),
                other => panic!("{}: {:?}", text, other),
            }
        }
    }

    #[test]
    fn non_finite_floats_are_lowercase_literals() {
        assert_eq!(float_repr(f64::INFINITY), "inf");
        assert_eq!(float_repr(f64::NEG_INFINITY), "-inf");
        assert_eq!(float_repr(f64::NAN), "nan");
        assert_eq!(lex_one("inf"), Token::Inf);
        assert_eq!(lex_one("nan"), Token::Nan);
    }

    #[test]
    fn strings_round_trip_through_the_lexer() {
        // The control characters are the cases that were silently wrong
        // before: `\u{7}` read back as `u{7}`.
        for s in ["", "hi", "a\"b", "a\\b", "tab\there", "nl\n", "\u{7}", "\u{1b}[0m", "é 🐸", "\u{7f}"] {
            match lex_one(&escape_str(s)) {
                Token::String(t) => assert_eq!(s, t, "{:?}", escape_str(s)),
                other => panic!("{:?}: {:?}", escape_str(s), other),
            }
        }
    }
}
