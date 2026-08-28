//! `plans/DATA.md` Stage 3: fn-ptr → span source map, `FrogState` entry
//! source retention, and rustc-style span rendering of type errors.

use froglang_core::state::FrogState;

#[test]
fn a_type_error_is_rendered_with_a_source_snippet_and_caret() {
    let mut s = FrogState::new();
    let src = "let x = 1\nlet y = x + \"nope\"\n";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("2 | let y = x + \"nope\""), "{err}");
    assert!(err.contains("-->"), "{err}");
    // A caret line under the snippet, indented to line up with the gutter.
    assert!(err.lines().last().unwrap().contains('^'), "{err}");
}

#[test]
fn declaring_a_func_records_its_name_and_span_in_the_source_map() {
    let mut s = FrogState::new();
    s.eval("func add(a: Int, b: Int): Int = a + b\n1").unwrap();
    let entry = s.codegen.source_map().iter().find(|e| e.name == "add").expect("add not in source map");
    assert_eq!(entry.entry_id, 0);
    // The recorded span should point at the first line (0-indexed — the
    // lexer's own convention, see `diagnostics::render_span`'s doc comment).
    assert_eq!(entry.span.start.line, 0);
}

#[test]
fn each_eval_call_gets_its_own_entry_source() {
    let mut s = FrogState::new();
    s.eval("1").unwrap();
    s.eval("2").unwrap();
    assert_eq!(s.entry_sources.len(), 2);
    assert_eq!(s.entry_sources[0].source, "1");
    assert_eq!(s.entry_sources[1].source, "2");
    assert_eq!(s.entry_source(1).unwrap().source, "2");
}

#[test]
fn a_failed_eval_does_not_consume_an_entry_source_or_leave_a_stale_source_map_entry() {
    let mut s = FrogState::new();
    assert!(s.eval("1 + \"a\"").is_err());
    assert_eq!(s.entry_sources.len(), 0);
    assert!(s.codegen.source_map().is_empty());

    // And the state stays usable afterward (existing rollback discipline).
    let (v, _) = s.eval("func f(): Int = 1\nf()").unwrap();
    assert_eq!(format!("{:?}", v), "Int(1)");
    assert_eq!(s.entry_sources.len(), 1);
    assert_eq!(s.codegen.source_map().len(), 1);
}

#[test]
fn source_map_entries_across_two_entries_carry_the_right_entry_id() {
    let mut s = FrogState::new();
    s.eval("func f(): Int = 1\nf()").unwrap();
    s.eval("func g(): Int = 2\ng()").unwrap();
    let names: Vec<(&str, usize)> = s.codegen.source_map().iter().map(|e| (e.name.as_str(), e.entry_id)).collect();
    assert!(names.contains(&("f", 0)), "{:?}", names);
    assert!(names.contains(&("g", 1)), "{:?}", names);
}
