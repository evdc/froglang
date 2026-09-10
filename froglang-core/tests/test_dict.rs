//! `Dict<K, V>` — literal syntax, indexing, equality, mutation, iteration,
//! and the `Trait::Hash` key restriction. See `plans/DATA.md`'s Dict design
//! and `runtime::gc::FrogDict`'s doc comment for the representation this
//! exercises.

mod common;
use common::{run, run_raw, run_gc_stress, run_cow_verify};

fn type_error(src: &str) -> String {
    let ast = froglang_core::frontend::parser::Parser::parse(src).expect("parse error");
    let mut tc = froglang_core::frontend::typeck::TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    err.item.msg
}

// ── literal ──────────────────────────────────────────────────────────────────

#[test]
fn empty_dict_literal_prints_bracket_colon_bracket() {
    assert_eq!(run("print([:])"), "[:]\n");
}

#[test]
fn a_dict_literal_prints_in_insertion_order() {
    assert_eq!(run(r#"print(["c": 3, "a": 1, "b": 2])"#), "[\"c\": 3, \"a\": 1, \"b\": 2]\n");
}

#[test]
fn a_later_duplicate_key_in_a_literal_overwrites_the_earlier_one() {
    assert_eq!(run(r#"print(["a": 1, "a": 2])"#), "[\"a\": 2]\n");
}

#[test]
fn mixing_key_value_pairs_with_bare_elements_is_a_parse_error() {
    let out = run_raw(r#"print(["a": 1, 2])"#);
    assert_ne!(out.status, Some(0));
}

// ── key kinds ────────────────────────────────────────────────────────────────

#[test]
fn int_keys_work() {
    assert_eq!(run("print([1: \"a\", 2: \"b\"][2])"), "b\n");
}

#[test]
fn float_keys_work_and_normalize_negative_zero() {
    assert_eq!(run("print([0.0: \"z\"][-0.0])"), "z\n");
}

#[test]
fn bool_keys_work() {
    assert_eq!(run("print([true: 1, false: 0][true])"), "1\n");
}

#[test]
fn str_keys_work() {
    assert_eq!(run(r#"print(["k": 42]["k"])"#), "42\n");
}

#[test]
fn a_non_hash_key_type_is_rejected() {
    let msg = type_error(r#"[[1, 2]: "a"]"#);
    assert!(msg.contains("Hash"), "unexpected message: {}", msg);
}

// ── indexing ─────────────────────────────────────────────────────────────────

#[test]
fn indexing_reads_the_value_for_a_present_key() {
    assert_eq!(run(r#"print(["a": 1, "b": 2]["b"])"#), "2\n");
}

#[test]
fn indexing_a_missing_key_aborts_the_process() {
    let out = run_raw(r#"print(["a": 1]["z"])"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stderr.contains("key not found"), "stderr: {}", out.stderr);
}

// ── get ──────────────────────────────────────────────────────────────────────

#[test]
fn get_on_a_present_key_returns_the_value() {
    assert_eq!(run(r#"print(["a": 1].get("a") catch -1)"#), "1\n");
}

#[test]
fn get_on_a_missing_key_returns_a_key_error() {
    let out = run(r#"print(["a": 1].get("z"))"#);
    assert!(out.starts_with("KeyError("), "unexpected output: {}", out);
}

#[test]
fn get_propagates_through_question_mark() {
    let src = r#"
        func look(d: Dict<Str, Int>, k: Str): Int | KeyError = {
            let v = d.get(k)?
            v
        }
        print(look(["a": 1], "a") catch -1)
    "#;
    assert_eq!(run(src), "1\n");
}

// ── membership ───────────────────────────────────────────────────────────────

#[test]
fn in_reports_key_membership() {
    assert_eq!(run(r#"print("a" in ["a": 1])"#), "true\n");
    assert_eq!(run(r#"print("z" in ["a": 1])"#), "false\n");
}

/// `in` on a `Dict` checks the *key* type — the value type never enters
/// into it, so `1 in ["a": 1]` (`1`'s type doesn't match the `Str` key
/// type) is a type error, not a runtime `false`.
#[test]
fn in_type_checks_the_left_operand_against_the_key_type_not_the_value_type() {
    let msg = type_error(r#"1 in ["a": 1]"#);
    assert!(msg.contains("key"), "unexpected message: {}", msg);
}

// ── len / truthiness ─────────────────────────────────────────────────────────

#[test]
fn len_counts_entries() {
    assert_eq!(run(r#"print(len(["a": 1, "b": 2]))"#), "2\n");
    assert_eq!(run("print(len([:]))"), "0\n");
}

#[test]
fn an_empty_dict_is_falsey_a_nonempty_one_is_truthy() {
    assert_eq!(run("if [:] then print(\"t\") else print(\"f\")"), "f\n");
    assert_eq!(run(r#"if ["a": 1] then print("t") else print("f")"#), "t\n");
}

// ── equality ─────────────────────────────────────────────────────────────────

#[test]
fn equal_dicts_compare_equal_regardless_of_insertion_order() {
    assert_eq!(run(r#"print(["a": 1, "b": 2] == ["b": 2, "a": 1])"#), "true\n");
}

#[test]
fn a_differing_value_decides_inequality() {
    assert_eq!(run(r#"print(["a": 1] == ["a": 2])"#), "false\n");
}

#[test]
fn a_differing_key_set_decides_inequality() {
    assert_eq!(run(r#"print(["a": 1] == ["a": 1, "b": 2])"#), "false\n");
}

// ── keys / values / for ─────────────────────────────────────────────────────

#[test]
fn keys_and_values_preserve_insertion_order() {
    assert_eq!(run(r#"print(["c": 3, "a": 1].keys())"#), "[\"c\", \"a\"]\n");
    assert_eq!(run(r#"print(["c": 3, "a": 1].values())"#), "[3, 1]\n");
}

#[test]
fn for_loop_iterates_keys_in_insertion_order() {
    let src = r#"
        mut out = []
        for k in ["c": 3, "a": 1, "b": 2] do out.push(k)
        print(out)
    "#;
    assert_eq!(run(src), "[\"c\", \"a\", \"b\"]\n");
}

// ── mutation ─────────────────────────────────────────────────────────────────

#[test]
fn index_assignment_overwrites_an_existing_key() {
    let src = r#"
        mut d = ["a": 1]
        d["a"] = 100
        print(d)
    "#;
    assert_eq!(run(src), "[\"a\": 100]\n");
}

#[test]
fn index_assignment_inserts_a_new_key_at_the_end() {
    let src = r#"
        mut d = ["a": 1]
        d["b"] = 2
        print(d)
    "#;
    assert_eq!(run(src), "[\"a\": 1, \"b\": 2]\n");
}

#[test]
fn index_assignment_through_an_alias_does_not_affect_the_original() {
    let src = r#"
        mut a = ["a": 1]
        mut b = a
        b["a"] = 999
        print(a)
        print(b)
    "#;
    assert_eq!(run(src), "[\"a\": 1]\n[\"a\": 999]\n");
}

#[test]
fn nested_dict_mutation_works_and_does_not_alias() {
    let src = r#"
        mut a = ["x": ["y": 1]]
        mut b = a
        b["x"]["y"] = 42
        print(a)
        print(b)
    "#;
    assert_eq!(run(src), "[\"x\": [\"y\": 1]]\n[\"x\": [\"y\": 42]]\n");
}

#[test]
fn mutation_survives_under_the_cow_aliasing_verifier() {
    let src = r#"
        mut a = ["a": 1]
        mut b = a
        b["a"] = 2
        print(a)
        print(b)
    "#;
    assert_eq!(run_cow_verify(src), "[\"a\": 1]\n[\"a\": 2]\n");
}

// ── remove ───────────────────────────────────────────────────────────────────

#[test]
fn remove_on_a_present_key_returns_the_value_and_deletes_it() {
    let src = r#"
        mut d = ["a": 1, "b": 2]
        print(d.remove("a") catch -1)
        print(d)
        print(len(d))
    "#;
    assert_eq!(run(src), "1\n[\"b\": 2]\n1\n");
}

#[test]
fn remove_on_a_missing_key_returns_a_key_error_and_leaves_the_dict_unchanged() {
    let src = r#"
        mut d = ["a": 1]
        let r = d.remove("z")
        print(r)
        print(d)
    "#;
    let out = run(src);
    let mut lines = out.lines();
    assert!(lines.next().unwrap().starts_with("KeyError("));
    assert_eq!(lines.next().unwrap(), "[\"a\": 1]");
}

#[test]
fn a_removed_entrys_slot_is_reused_by_a_later_insert_in_insertion_order() {
    let src = r#"
        mut d = ["a": 1, "b": 2, "c": 3]
        d.remove("b") catch -1
        d["d"] = 4
        print(d)
    "#;
    assert_eq!(run(src), "[\"a\": 1, \"c\": 3, \"d\": 4]\n");
}

// ── nesting / GC ─────────────────────────────────────────────────────────────

#[test]
fn a_dict_nested_in_a_list_prints_and_round_trips() {
    assert_eq!(run(r#"print([["a": 1], ["b": 2]])"#), "[[\"a\": 1], [\"b\": 2]]\n");
}

// ── read / notation law ─────────────────────────────────────────────────────

#[test]
fn read_parses_a_dict_literal() {
    let src = r#"
        let d: Dict<Str, Int> | ReadError = read("[\"a\": 1, \"b\": 2]")
        print(d)
    "#;
    assert_eq!(run(src), "[\"a\": 1, \"b\": 2]\n");
}

#[test]
fn read_of_repr_of_a_dict_round_trips() {
    let src = r#"
        let d: Dict<Str, Int> | ReadError = read(repr(["x": 1, "y": 2]))
        print(d)
    "#;
    assert_eq!(run(src), "[\"x\": 1, \"y\": 2]\n");
}

#[test]
fn read_of_a_malformed_dict_yields_a_read_error() {
    let src = r#"
        let d: Dict<Str, Int> | ReadError = read("[\"a\": 1")
        print(d)
    "#;
    assert!(run(src).starts_with("ReadError("));
}

// ── JSON ─────────────────────────────────────────────────────────────────────

#[test]
fn json_to_str_serializes_a_dict_as_an_object() {
    assert_eq!(run(r#"print(json.to_str(["a": 1, "b": 2]))"#), "{\"a\":1,\"b\":2}\n");
}

#[test]
fn json_to_str_of_an_empty_dict_is_an_empty_object() {
    assert_eq!(run("print(json.to_str([:]))"), "{}\n");
}

#[test]
fn json_parse_reads_an_object_into_a_dict() {
    let src = r#"
        let d: Dict<Str, Int> | JsonError = json.parse("{\"x\": 1, \"y\": 2}")
        print(d)
    "#;
    let out = run(src);
    assert!(out == "[\"x\": 1, \"y\": 2]\n" || out == "[\"y\": 2, \"x\": 1]\n", "unexpected output: {}", out);
}

#[test]
fn json_round_trip_through_to_str_and_parse_preserves_value_equality() {
    let src = r#"
        let d: Dict<Str, Int> | JsonError = json.parse(json.to_str(["p": 3, "q": 4]))
        print(d == ["p": 3, "q": 4])
    "#;
    assert_eq!(run(src), "true\n");
}

/// `Notation::Json`'s check runs in `desugar_notation`, a post-pass over
/// the typed tree (see `finish_len` and friends for the general shape) —
/// not visible to `type_error`'s bare `check_and_lower`, so this needs the
/// real pipeline (`run_raw`, via the `froglang-core` binary) rather than
/// the `type_error` helper the other rejection tests use.
#[test]
fn json_to_str_rejects_a_non_str_keyed_dict() {
    let out = run_raw(r#"print(json.to_str([1: "a"]))"#);
    assert_ne!(out.status, Some(0));
    assert!(out.stdout.contains("Str"), "unexpected stdout: {}", out.stdout);
}

#[test]
fn a_dict_survives_a_forced_collection_with_str_keys_and_values() {
    let src = r#"
        mut d = [:]
        for i in 0..64 do d[repr(i)] = repr(i)
        print(len(d))
    "#;
    assert_eq!(run_gc_stress(src), "64\n");
}
