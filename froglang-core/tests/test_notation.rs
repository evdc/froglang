//! plans/DATA.md stage 2 — the printed form of a float or a string has to be
//! something the lexer reads back as the same value.
//!
//! These are round-trip tests wherever they can be: the program prints a
//! value, and a second program written in the notation the first emitted
//! prints the same thing. Asserting on the literal text alone would let the
//! printer and the lexer drift apart in exactly the way that made `\u{7}`
//! silently wrong. `crate::notation`'s unit tests cover the formatter
//! against the lexer directly; these cover the compiled path.

mod common;
use common::{run, run_raw};

// ── floats ───────────────────────────────────────────────────────────────────

#[test]
fn exponent_literals_are_readable() {
    assert_eq!(run("print(1e100)\nprint(1.5e-3)\nprint(2E5)"), "1e100\n0.0015\n200000.0\n");
}

/// The round trip that motivates the stage: `1e100` used to print in a form
/// the lexer rejected, so its own output was not a valid program.
#[test]
fn a_printed_float_reads_back() {
    for src in ["1e100", "0.1 + 0.2", "1.0", "1.0 / 3.0", "1e-300"] {
        let printed = run(&format!("print({})", src));
        let reprinted = run(&format!("print({})", printed.trim()));
        assert_eq!(printed, reprinted, "{}", src);
    }
}

#[test]
fn non_finite_floats_print_as_their_literals() {
    assert_eq!(
        run("print(1.0/0.0)\nprint(-1.0/0.0)\nprint(0.0/0.0)"),
        "inf\n-inf\nnan\n",
    );
    assert_eq!(run("print(inf)\nprint(-inf)\nprint(nan)"), "inf\n-inf\nnan\n");
}

/// `inf` and `nan` are ordinary `Float` values, not a separate type — they
/// compare, arithmetic works on them, and `nan != nan` is the carve-out the
/// notation law is stated modulo.
#[test]
fn inf_and_nan_are_floats() {
    assert_eq!(run("print(inf > 1.0)\nprint(inf + 1.0)"), "true\ninf\n");
    assert_eq!(run("print(nan == nan)\nprint(nan != nan)"), "false\ntrue\n");
    assert_eq!(run("print(inf == inf)"), "true\n");
}

/// The exponent scan must not eat the `e` of a following name — `e` is a
/// name character, and this is the failure mode the lookahead exists for.
#[test]
fn a_name_after_a_number_survives() {
    assert_eq!(run("print(if 1 == 1 then 2 else 3)"), "2\n");
    assert_eq!(run("let e = 5\nprint(e)"), "5\n");
}

// ── strings ──────────────────────────────────────────────────────────────────

/// Inside a composite a `Str` prints as a literal, so its escaping is
/// notation. Before this stage it borrowed Rust's `escape_debug`, which
/// emits `\u{7}` — an escape the lexer didn't have, and which silently read
/// back as the four characters `u{7}`.
#[test]
fn control_characters_print_as_readable_escapes() {
    assert_eq!(run("print([\"a\\u{7}b\"])"), "[\"a\\u{7}b\"]\n");
    assert_eq!(run("print([\"tab\\there\", \"nl\\n\"])"), "[\"tab\\there\", \"nl\\n\"]\n");
}

#[test]
fn a_printed_string_reads_back() {
    // The printed form is pasted back in as source, so whatever the escaper
    // emitted has to lex — and to the same string.
    let printed = run("print([\"a\\u{7}b\\tc\\\"d\\\\e\"])");
    let reprinted = run(&format!("print({})", printed.trim()));
    assert_eq!(printed, reprinted);
}

#[test]
fn non_ascii_text_is_not_escaped() {
    // Nothing in a string literal but `"` and `\` is special to the lexer,
    // so escaping the rest would only make the notation unreadable.
    assert_eq!(run("print([\"é 🐸\"])"), "[\"é 🐸\"]\n");
}

#[test]
fn unicode_escapes_denote_the_character() {
    assert_eq!(run("print(\"\\u{1F438}\")\nprint(\"\\u{41}\")"), "🐸\nA\n");
}

// ── the escapes that are now errors ──────────────────────────────────────────

/// An unknown escape used to keep the escaped character, so `"\q"` was
/// `"q"` — the rule that let a wrong escaper be unobservable. A small
/// breaking change, and the point of the language.
#[test]
fn an_unknown_escape_is_a_lex_error() {
    let out = run_raw("print(\"a\\qb\")");
    let text = format!("{}{}", out.stdout, out.stderr);
    assert!(text.contains("escape"), "unexpected output: {}", text);
    assert!(!text.contains("aqb"), "escape was silently dropped: {}", text);
}

#[test]
fn a_malformed_unicode_escape_is_a_lex_error() {
    for src in ["print(\"\\u{}\")", "print(\"\\u{zz}\")", "print(\"\\u{D800}\")"] {
        let out = run_raw(src);
        let text = format!("{}{}", out.stdout, out.stderr);
        assert!(text.contains("unicode") || text.contains("escape"), "{}: {}", src, text);
    }
}
