//! `plans/DATA.md` Stage 6 — the `#name(...)` annotation sigil, `annotation`
//! declarations, and struct-field defaults (Stage 6's own prerequisite).
//!
//! Scope of this stage, as actually built (see `plans/DATA.md`'s Stage 6
//! section): parsing, typed validation (unknown annotation, wrong field,
//! wrong type, missing required field, both sugars), and both attachment
//! directions (leading/forward and trailing/backward) on top-level
//! declarations and on struct/variant fields. No consumer exists yet — a
//! validated annotation is checked and then discarded, since host exposure
//! (Stage 7) and JSON (Stage 8) are separate, later stages. So every test
//! here asserts either a successful compile (the annotation validated and
//! had no runtime effect) or a specific type error message.

mod common;
use common::{run, run_raw};

// ---------------------------------------------------------------------
// Cross-module: an annotation declared and used in an *imported* file.
// Needs its own two-file harness — `common::run`'s single scratch file
// can't exercise `import`. Regression coverage for a real bug found while
// building this: `frontend::modules`'s per-module name-mangling renamed an
// `annotation` declaration's own name but not a `#name(...)` use's
// reference to it (both the top-level `Decorated` wrapper's and a
// `FieldDecl`'s own `annotations` list), so an annotation used anywhere
// but its own declaring file always read as "unknown annotation".
// ---------------------------------------------------------------------

struct ModulePair { stdout: String, status: Option<i32> }

fn run_two_files(lib_src: &str, main_src: &str) -> ModulePair {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("frog_annot_mod_{}_{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("failed to create scratch module dir");
    let lib_path = dir.join("lib.frog");
    let main_path = dir.join("main.frog");
    std::fs::write(&lib_path, lib_src).expect("failed to write lib.frog");
    std::fs::write(&main_path, main_src).expect("failed to write main.frog");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_froglang-core"))
        .args(["run", main_path.to_str().expect("scratch path is valid UTF-8")])
        .output()
        .expect("failed to run froglang-core binary");
    let _ = std::fs::remove_dir_all(&dir);
    ModulePair {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        status: output.status.code(),
    }
}

#[test]
fn an_annotation_declared_and_used_in_an_imported_module_resolves() {
    let out = run_two_files(
        r#"
annotation model()
#model
data Point(x: Int, y: Int)
"#,
        r#"
import "./lib.frog" { Point }
let p = Point(x=1, y=2)
print(p.x)
"#,
    );
    assert_eq!(out.status, Some(0), "stdout: {}", out.stdout);
    assert_eq!(out.stdout, "1\n");
}

#[test]
fn a_field_level_annotation_declared_in_an_imported_module_resolves() {
    let out = run_two_files(
        r#"
annotation unique()
data Point(x: Int #unique, y: Int)
"#,
        r#"
import "./lib.frog" { Point }
let p = Point(x=1, y=2)
print(p.x)
"#,
    );
    assert_eq!(out.status, Some(0), "stdout: {}", out.stdout);
    assert_eq!(out.stdout, "1\n");
}

// ---------------------------------------------------------------------
// Struct-field defaults
// ---------------------------------------------------------------------

#[test]
fn a_missing_field_with_a_default_uses_it() {
    let out = run(r#"
data Person(name: Str, age: Int = 0)
let p = Person(name="Alice")
print(p.age)
"#);
    assert_eq!(out, "0\n");
}

#[test]
fn an_explicit_value_overrides_the_default() {
    let out = run(r#"
data Person(name: Str, age: Int = 0)
let p = Person(name="Alice", age=42)
print(p.age)
"#);
    assert_eq!(out, "42\n");
}

#[test]
fn a_default_can_reference_an_optional_field() {
    let out = run(r#"
data Config(label: Str? = none)
let c = Config()
print(c.label == none)
"#);
    assert_eq!(out, "true\n");
}

#[test]
fn a_default_works_on_a_nominal_union_variant_field() {
    let out = run(r#"
data Shape(color: Str = "black") is Circle(r: Int) | Square(side: Int = 1)
let s = Shape.Square()
print(s.color)
match s {
    is Square(side) then print(side)
    else print(-1)
}
"#);
    assert_eq!(out, "black\n1\n");
}

#[test]
fn a_default_value_of_the_wrong_type_is_a_type_error() {
    let out = run_raw(r#"
data Person(name: Str, age: Int = "nope")
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("doesn't match its declared type"), "stdout: {}", out.stdout);
}

#[test]
fn a_non_literal_default_is_a_type_error() {
    let out = run_raw(r#"
func zero(): Int = 0
data Person(name: Str, age: Int = zero())
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("literal constant"), "stdout: {}", out.stdout);
}

#[test]
fn a_positional_field_cannot_have_a_default() {
    let out = run_raw(r#"
data Lit(Int = 0)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("positional field cannot have a default"), "stdout: {}", out.stdout);
}

#[test]
fn a_missing_field_with_no_default_is_still_an_error() {
    let out = run_raw(r#"
data Person(name: Str, age: Int)
let p = Person(name="Alice")
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("Missing field"), "stdout: {}", out.stdout);
}

// ---------------------------------------------------------------------
// `annotation` declarations and `#name(...)` validation
// ---------------------------------------------------------------------

#[test]
fn a_valid_annotation_use_compiles_with_no_runtime_effect() {
    let out = run(r#"
annotation json(name: Str? = none, skip: Bool = false, required: Bool = false)

#json(name="lastName")
data Person(
    id: Int          #json(skip)
    last_name: Str
)
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn an_unknown_annotation_is_an_error() {
    let out = run_raw(r#"
#nope
data Person(name: Str)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("unknown annotation '#nope'"), "stdout: {}", out.stdout);
}

#[test]
fn an_annotation_field_of_the_wrong_type_is_an_error() {
    let out = run_raw(r#"
annotation json(name: Str)
#json(name=5)
data Person(name: Str)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("expected Str, got Int"), "stdout: {}", out.stdout);
}

#[test]
fn a_missing_required_annotation_field_is_an_error() {
    let out = run_raw(r#"
annotation json(name: Str)
#json()
data Person(name: Str)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("missing required field 'name'"), "stdout: {}", out.stdout);
}

#[test]
fn an_unknown_field_on_a_known_annotation_is_an_error() {
    let out = run_raw(r#"
annotation json(name: Str)
#json(namee="x")
data Person(name: Str)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("no field 'namee'"), "stdout: {}", out.stdout);
}

#[test]
fn a_duplicate_field_in_one_annotation_use_is_an_error() {
    let out = run_raw(r#"
annotation json(name: Str = "x")
#json(name="a", name="b")
data Person(name: Str)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("duplicate field"), "stdout: {}", out.stdout);
}

#[test]
fn bare_identifier_sugar_sets_a_bool_field_true() {
    let out = run(r#"
annotation json(skip: Bool = false)
data Person(id: Int #json(skip))
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn single_field_positional_sugar_works() {
    let out = run(r#"
annotation rename(name: Str)
data Person(id: Int #rename("identifier"))
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn a_bare_fieldless_annotation_use_needs_no_parens() {
    let out = run(r#"
annotation primary_key()
data Person(id: Int #primary_key)
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn a_dotted_annotation_name_round_trips() {
    let out = run(r#"
annotation db.model(table: Str? = none)
#db.model(table="people")
data Person(name: Str)
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn annotation_fields_must_be_named() {
    let out = run_raw(r#"
annotation json(Str)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("must be named"), "stdout: {}", out.stdout);
}

// ---------------------------------------------------------------------
// Attachment: leading (forward) vs trailing (backward), and func decls
// ---------------------------------------------------------------------

#[test]
fn leading_annotation_attaches_to_the_next_top_level_declaration() {
    let out = run(r#"
annotation test()
#test
func add(a: Int, b: Int): Int = a + b
print(add(2, 3))
"#);
    assert_eq!(out, "5\n");
}

#[test]
fn trailing_annotation_attaches_to_the_preceding_field_on_the_same_line() {
    let out = run(r#"
annotation unique()
data Person(
    id: Int #unique
    name: Str
)
let p = Person(id=1, name="Alice")
print(p.id)
"#);
    assert_eq!(out, "1\n");
}

#[test]
fn multiple_stacked_leading_annotations_all_validate() {
    let out = run(r#"
annotation a()
annotation b()
#a
#b
data Person(name: Str)
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn leading_and_trailing_annotations_on_the_same_declaration_both_apply() {
    let out = run(r#"
annotation a()
annotation b()
#a
data Person(name: Str) #b
print("ok")
"#);
    assert_eq!(out, "ok\n");
}

#[test]
fn a_second_declaration_of_the_same_annotation_name_is_an_error() {
    let out = run_raw(r#"
annotation json(name: Str = "x")
annotation json(other: Int = 0)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("already declared"), "stdout: {}", out.stdout);
}

#[test]
fn an_annotation_declaration_may_itself_carry_an_annotation() {
    // `hoist_annotation_decls` runs before `strip_and_validate_annotations`
    // (it must — the strip pass validates against what the hoist registers),
    // so a decorated `annotation` declaration is still wrapped in
    // `Decorated` when the hoist sees it. Before the fix, the hoist skipped
    // it and the lowering loop discarded it, so `#model` below read as an
    // unknown annotation.
    let out = run(r#"
annotation doc(text: Str)
#doc(text="x")
annotation model()
#model
data Point(x: Int, y: Int)
let p = Point(x=1, y=2)
print(p.x)
"#);
    assert_eq!(out, "1\n");
}

#[test]
fn a_default_on_a_generic_field_is_rejected() {
    // The declaration's binder `TypeVar` has no single type to check the
    // literal against; accepting it used to bind the binder var globally
    // and leave the construction site's fresh var unbound, which reached
    // codegen as a bare `TypeVar` and panicked there.
    let out = run_raw(r#"
data Box<T>(value: T = 0)
let c = Box()
print(c.value)
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("cannot have a default value"), "stdout: {}", out.stdout);
}

#[test]
fn a_default_on_a_non_generic_field_of_a_generic_struct_still_works() {
    let out = run(r#"
data Box<T>(value: T, count: Int = 7)
let b = Box(value="x")
print(b.count)
"#);
    assert_eq!(out, "7\n");
}

#[test]
fn an_annotation_on_an_import_is_a_clear_error() {
    // `frontend::modules` erases `import` statements before the
    // typechecker — the only pass that knows what an annotation means —
    // ever runs, so an annotation on one could only be dropped
    // unvalidated. It used to be reported as a *nested* import, which
    // named the wrong problem entirely.
    let out = run_two_files(
        r#"
data Point(x: Int, y: Int)
"#,
        r#"
annotation model()
#model
import "./lib.frog" { Point }
print("unreachable")
"#,
    );
    assert_ne!(out.status, Some(0));
    assert!(
        out.stdout.contains("annotations are not supported on 'import'"),
        "stdout: {}", out.stdout,
    );
}

#[test]
fn a_leading_annotation_works_inside_a_braced_block() {
    // `Grammar::block_expr` parses `{ ... }` with `Parser::expression`
    // rather than `Parser::statement`, so it needs its own copy of the
    // leading/trailing annotation peel — without it `#` was a hard parse
    // error anywhere inside braces, and `lower_block`'s
    // `strip_and_validate_annotations` call was unreachable.
    let out = run(r#"
annotation note()
func f(): Int = { #note
  let x = 1
  x + 1
}
print(f())
"#);
    assert_eq!(out, "2\n");
}

#[test]
fn a_leading_annotation_works_on_a_single_statement_braced_block() {
    // A lone statement is normally unwrapped out of its block; a decorated
    // one must stay wrapped, or the `Decorated` node escapes into
    // expression position where nothing strips it.
    let out = run(r#"
annotation note()
func f(): Int = { #note
  7
}
print(f())
"#);
    assert_eq!(out, "7\n");
}

#[test]
fn a_trailing_annotation_works_inside_a_braced_block() {
    let out = run(r#"
annotation tag(name: Str)
func f(): Int = {
  let y = 2 #tag(name="y")
  y
}
print(f())
"#);
    assert_eq!(out, "2\n");
}

#[test]
fn an_unknown_annotation_inside_a_braced_block_is_an_error() {
    let out = run_raw(r#"
func f(): Int = { #nope
  1
}
print(f())
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("unknown annotation '#nope'"), "stdout: {}", out.stdout);
}

#[test]
fn a_duplicate_field_in_an_annotation_declaration_is_an_error() {
    // `validate_annotation_use` finds a provided value by name (first
    // match) but type-checks positionally, so before this was rejected
    // `#a(x=1)` below reported "field 'x': expected Str, got Int" — a type
    // error about a field the program never mentioned twice.
    let out = run_raw(r#"
annotation a(x: Int, x: Str)
#a(x=1)
data P(y: Int)
print("unreachable")
"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("declares field 'x' twice"), "stdout: {}", out.stdout);
}

#[test]
fn a_default_on_a_positional_field_is_reported_at_the_field() {
    // The error used to be raised after advancing past `=`, spanned on
    // `parser.current_token` — i.e. on the default value, reading as if
    // the value were the problem. It belongs on the positional field.
    let src = "data Lit(Int = 3)\nprint(\"unreachable\")\n";
    let out = run_raw(src);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("a positional field cannot have a default value"), "stdout: {}", out.stdout);
    let type_offset = src.find("Int").expect("literal is in the source");
    let value_offset = src.find("3").expect("literal is in the source");
    assert!(out.stdout.contains(&format!("0:{}", type_offset)), "stdout: {}", out.stdout);
    assert!(!out.stdout.contains(&format!("0:{}", value_offset)), "stdout: {}", out.stdout);
}
