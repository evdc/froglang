//! String interpolation — `"n is ${n}"`.
//!
//! Three things are being pinned here, and they pull in different
//! directions:
//!
//!  - **The lexer split.** `${...}` is found by `Lexer::read_string`, which
//!    hands the parser unparsed source per piece; the awkward cases are all
//!    about where a piece *ends* (a nested string, a brace inside it, an
//!    escaped dollar).
//!  - **The conversion rule.** A `Str` piece goes in raw and everything else
//!    goes through `repr` — so interpolation must agree with `print`
//!    everywhere, which is the same drift guard `test_repr.rs` keeps.
//!  - **The notation law.** Now that `${` means something in source,
//!    `repr`'s output has to escape it (`plans/DATA.md`'s "future
//!    constraint", now present tense) while *data* read back by `read` must
//!    keep interpreting `${` as two ordinary characters.

mod common;
use common::{run, run_raw, run_gc_stress};

use froglang_core::frontend::lexer::Lexer;
use froglang_core::frontend::tokens::Token;

/// The stdout+stderr of a program expected to fail. `frog run` prints
/// diagnostics to stdout, so a test that reads only stderr sees nothing.
fn rejection(src: &str) -> String {
    let out = run_raw(src);
    assert_ne!(out.status, Some(0), "expected failure, got:\n{}", out.stdout);
    format!("{}{}", out.stdout, out.stderr)
}

// ── the basics ───────────────────────────────────────────────────────────────

#[test]
fn interpolates_a_binding() {
    assert_eq!(run(r#"let n = 3
print("n is ${n}")"#), "n is 3\n");
}

#[test]
fn a_str_piece_is_inserted_raw_not_quoted() {
    // The one place interpolation deliberately differs from `repr`: a
    // top-level string is text, exactly as `print("hi")` is text. A string
    // *nested* in a value is still quoted — see below.
    assert_eq!(run(r#"let name = "Bob"
print("hi ${name}!")"#), "hi Bob!\n");
}

#[test]
fn interpolates_every_scalar() {
    assert_eq!(
        run(r#"print("${1} ${2.5} ${true} ${none} ${0..3}")"#),
        "1 2.5 true none 0..3\n"
    );
}

#[test]
fn interpolates_an_arbitrary_expression() {
    // Not just a name: the fragment is parsed by the ordinary expression
    // parser, so precedence inside a `${}` is the language's own.
    assert_eq!(run(r#"let n = 3
print("${n + 4 * 2}")"#), "11\n");
}

#[test]
fn interpolates_a_call_a_field_and_an_index() {
    assert_eq!(run(r#"data P(name: Str, age: Int)
func twice(n: Int): Int = n * 2
let p = P(name="Ann", age=4)
let xs = [10, 20]
print("${twice(3)} ${p.name} ${xs[1]}")"#), "6 Ann 20\n");
}

#[test]
fn adjacent_interpolations_have_no_separator() {
    assert_eq!(run(r#"let a = 1
let b = 2
print("${a}${b}")"#), "12\n");
}

#[test]
fn an_interpolation_may_be_the_whole_literal() {
    assert_eq!(run(r#"let n = 7
print("${n}")"#), "7\n");
}

#[test]
fn interpolates_at_either_end() {
    assert_eq!(run(r#"let n = 7
print("${n} trailing")
print("leading ${n}")"#), "7 trailing\nleading 7\n");
}

#[test]
fn an_empty_string_piece_contributes_nothing() {
    assert_eq!(run(r#"print("[${""}]")"#), "[]\n");
}

// ── where the pieces end ─────────────────────────────────────────────────────

#[test]
fn a_literal_without_interpolation_is_still_a_plain_string_token() {
    // The collapse in `next_token`: `Token::InterpString` exists only where
    // interpolation was actually written, so nothing downstream of the two
    // grammar rules has to know this feature exists.
    let (tokens, errors) = Lexer::new(r#""just text""#).collect_errors();
    assert!(errors.is_empty(), "{:?}", errors);
    assert_eq!(tokens[0].item, Token::String("just text".to_string()));

    let (tokens, errors) = Lexer::new(r#""a ${x} b""#).collect_errors();
    assert!(errors.is_empty(), "{:?}", errors);
    assert!(matches!(tokens[0].item, Token::InterpString(_)));
}

#[test]
fn a_nested_string_may_itself_interpolate() {
    assert_eq!(run(r#"let n = 3
print("outer ${ "inner ${n}" }")"#), "outer inner 3\n");
}

#[test]
fn a_brace_inside_a_nested_string_does_not_close_the_interpolation() {
    // Why `read_interpolation` copies a nested literal verbatim instead of
    // counting braces through it.
    assert_eq!(run(r#"func id(s: Str): Str = s
print("${ id("}") }|")"#), "}|\n");
}

#[test]
fn a_brace_inside_a_block_expression_is_balanced_not_terminal() {
    assert_eq!(run(r#"print("${ { let a = 2; a * 3 } }")"#), "6\n");
}

#[test]
fn a_backslash_dollar_is_a_literal_dollar_brace() {
    assert_eq!(run(r#"let n = 1
print("cost: \${n}")"#), "cost: ${n}\n");
}

#[test]
fn a_dollar_not_followed_by_a_brace_stays_literal() {
    // Only `${` interpolates, so ordinary prose and prices need no escaping
    // — which is what keeps this a small change to existing programs.
    assert_eq!(run(r#"print("$5.00 and 100% $ free")"#), "$5.00 and 100% $ free\n");
}

// ── it is ordinary code inside there ─────────────────────────────────────────

#[test]
fn interpolation_works_inside_a_function_body() {
    assert_eq!(run(r#"func greet(who: Str): Str = "hello ${who}"
print(greet("frog"))"#), "hello frog\n");
}

#[test]
fn interpolation_works_in_a_loop_and_a_comprehension() {
    assert_eq!(run(r#"for i in 0..3 do print("i=${i}")
print([for i in 0..2 do "n${i}"])"#), "i=0\ni=1\ni=2\n[\"n0\", \"n1\"]\n");
}

#[test]
fn interpolation_works_inside_a_generic_function_at_each_instantiation() {
    // `repr`'s `TypeVar` deferral, inherited: the piece's type is read
    // after monomorphization, so one body serves both instantiations.
    assert_eq!(run(r#"func show<T>(x: T): Str = "<${x}>"
print(show(1))
print(show("hi"))"#), "<1>\n<hi>\n");
}

#[test]
fn a_captured_binding_interpolates_inside_a_lambda() {
    assert_eq!(run(r#"func apply(f: (Int -> Str), x: Int): Str = f(x)
let unit = "kg"
print(apply(n -> "${n}${unit}", 5))"#), "5kg\n");
}

// ── agreement with print/repr ────────────────────────────────────────────────

#[test]
fn interpolating_a_value_agrees_with_print() {
    // The same drift guard `test_repr.rs` keeps between `repr` and `print`,
    // extended to the third spelling of the same notation.
    let cases = [
        ("struct", r#"data P(name: Str, age: Int)
let v = P(name="Ann", age=4)"#),
        ("positional struct", "data Lit(Int)\nlet v = Lit(42)"),
        ("list", "let v = [1, 2, 3]"),
        ("nested list", "let v = [[1], [2, 3]]"),
        ("union", r#"data Shape(color: Str) is Circle(r: Int) | Rect(w: Int, h: Int)
let v = Circle(color="red", r=4)"#),
        ("anon union", r#"let v: Int | Str = 7"#),
        ("optional none", r#"let v: Int | None = none"#),
        ("float", "let v = 1.5"),
        ("range", "let v = 0..4"),
    ];
    for (name, decl) in cases {
        let out = run(&format!("{}\nprint(v)\nprint(\"${{v}}\")", decl));
        let mut lines = out.lines();
        let (printed, interpolated) = (lines.next().unwrap(), lines.next().unwrap());
        assert_eq!(printed, interpolated, "print/interpolation drift on {name}: {out:?}");
    }
}

#[test]
fn a_string_nested_in_a_value_is_still_quoted() {
    // Raw insertion is a rule about the *top-level* piece, not a rule about
    // strings — inside a list, `repr` is in charge and quotes.
    assert_eq!(run(r#"print("${["hi"]}")"#), "[\"hi\"]\n");
}

// ── the notation law ─────────────────────────────────────────────────────────

#[test]
fn repr_escapes_dollar_brace_so_its_output_reads_back_as_source() {
    assert_eq!(run(r#"let s = "a\${x} b"
print(repr(s))"#), "\"a\\${x} b\"\n");
}

#[test]
fn repr_leaves_a_lone_dollar_unescaped() {
    assert_eq!(run(r#"print(repr("$5"))"#), "\"$5\"\n");
}

#[test]
fn read_of_repr_round_trips_a_string_containing_dollar_brace() {
    // `read(repr(x)) == x`, DATA.md's law, for the string that interpolation
    // put at risk.
    assert_eq!(run(r#"let s = "a\${x} b"
let back: Str | ReadError = read(repr(s))
print("${back! == s}")"#), "true\n");
}

#[test]
fn read_does_not_interpolate_its_input() {
    // Data is not code: `${x}` arriving in data stays four characters, even
    // though `x` is a binding in scope right here. Without `Lexer::for_data`
    // this would be a parse error at best.
    //
    // The `\$` below is escaping *this* literal, which is source and does
    // interpolate — so what `read` receives is the six characters `${x}`.
    assert_eq!(run(r#"let x = 99
let s: Str | ReadError = read("\"cost: \${x}\"")
print(s!)"#), "cost: ${x}\n");
}

// ── heap behaviour ───────────────────────────────────────────────────────────

#[test]
fn interpolation_survives_gc_stress() {
    // Every piece allocates, and the fold holds an intermediate string
    // alive while building the next one.
    assert_eq!(run_gc_stress(r#"data P(name: Str, age: Int)
mut out = ""
for i in 0..20 do {
    let p = P(name="n${i}", age=i)
    out = "${out}|${p}"
}
print(len(out) > 20)"#), "true\n");
}

// ── errors ───────────────────────────────────────────────────────────────────

#[test]
fn an_unterminated_interpolation_names_the_missing_brace() {
    // Not "unterminated string literal": the outer literal's closing quote
    // is consumed by the scan, so that message would point at the wrong
    // character and suggest the wrong fix.
    let err = rejection(r#"let n = 1
print("oops ${n")"#);
    assert!(err.contains("unterminated '${'"), "{}", err);
}

#[test]
fn an_empty_interpolation_is_a_parse_error() {
    assert!(rejection(r#"print("${}")"#).contains("expected an expression"));
}

#[test]
fn trailing_junk_in_an_interpolation_is_a_parse_error() {
    // `${a b}` has no reading; silently keeping `a` would be worse.
    assert!(rejection(r#"let a = 1
let b = 2
print("${a b}")"#).contains("expected an operator"));
}

#[test]
fn interpolating_a_value_with_no_notation_is_a_type_error() {
    let err = rejection(r#"func f(x: Int): Int = x
print("${f}")"#);
    assert!(err.contains("has no notation"), "{}", err);
}

#[test]
fn an_error_inside_an_interpolation_points_inside_the_string() {
    // What `StrPart::Expr`'s recorded position buys: the caret lands on the
    // offending name, not on the literal as a whole. Line 2, and a column
    // inside the quotes.
    let err = rejection(r#"let n = 1
print("value: ${nope}")"#);
    assert!(err.contains("Unbound variable nope"), "{}", err);
    assert!(err.contains(":2:17"), "expected a span inside the string, got:\n{}", err);
}
