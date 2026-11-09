//! One variable per declaration, not per name: an inner `let`/`mut`, loop
//! variable, match binder or `catch` binder that shadows an outer name must
//! leave the outer binding untouched, and the outer one must be usable (and
//! freed exactly once) after the inner scope ends. `common::run` runs every
//! program under the leak check, so a shadow that drops, overwrites or
//! double-frees the outer value fails here too.
//!
//! Each scenario is a template run once per value kind (Int, Str, List, Dict,
//! struct, boxed union, inline union); `$A` is the type annotation, `$O` the
//! outer value, `$I` the inner one, and `$S(x)` prints `x` in the kind's format.

mod common;
use common::run;

struct Kind {
    name:  &'static str,
    /// Type annotation text (`: T`) for the `let`, if the kind needs one.
    ann:   &'static str,
    outer: &'static str,
    inner: &'static str,
    /// What `show(outer)` / `show(inner)` print.
    out_o: &'static str,
    out_i: &'static str,
}

const KINDS: &[Kind] = &[
    Kind { name: "int",    ann: "",                  outer: "1",                          inner: "10",                          out_o: "1",           out_i: "10" },
    Kind { name: "str",    ann: "",                  outer: "\"out\" + \"er\"",           inner: "\"in\" + \"ner\"",            out_o: "outer",       out_i: "inner" },
    Kind { name: "list",   ann: "",                  outer: "[1, 2]",                     inner: "[3, 4, 5]",                   out_o: "[1, 2]",      out_i: "[3, 4, 5]" },
    Kind { name: "dict",   ann: "",                  outer: "[\"a\": 1]",                 inner: "[\"b\": 2, \"c\": 3]",        out_o: "[\"a\": 1]",  out_i: "[\"b\": 2, \"c\": 3]" },
    Kind { name: "struct", ann: "",                  outer: "Pt(name = \"o\" + \"ut\", n = 1)", inner: "Pt(name = \"i\" + \"n\", n = 2)", out_o: "Pt(name=\"out\", n=1)", out_i: "Pt(name=\"in\", n=2)" },
    Kind { name: "tree",   ann: "",                  outer: "Node(l = Leaf, r = Leaf, label = \"o\" + \"ut\")", inner: "Node(l = Leaf, r = Leaf, label = \"i\" + \"n\")", out_o: "out", out_i: "in" },
    Kind { name: "union",  ann: ": Int | Str",       outer: "\"out\" + \"er\"",           inner: "\"in\" + \"ner\"",            out_o: "\"outer\"",   out_i: "\"inner\"" },
];

/// Declarations shared by every program: the data types, and one printing
/// function per kind (`show_<kind>`).
const PRELUDE: &str = r#"
data Pt(name: Str, n: Int)
data Tree is Leaf | Node(l: Tree, r: Tree, label: Str)
func label_of(t: Tree): Str = match t {
  is Leaf then "."
  is Node(l, r, label) then label
}
func show_int(x: Int) = print(x)
func show_str(x: Str) = print(x)
func show_list(x: List<Int>) = print(x)
func show_dict(x: Dict<Str, Int>) = print(x)
func show_struct(x: Pt) = print(x)
func show_tree(x: Tree) = print(label_of(x))
func show_union(x: Int | Str) = print(x)
"#;

fn expand(template: &str, k: &Kind) -> String {
    let mut out = template
        .replace("$A", k.ann)
        .replace("$O", k.outer)
        .replace("$I", k.inner)
        .replace("$S(", &format!("show_{}(", k.name));
    out.insert_str(0, PRELUDE);
    out
}

/// Run `template` for every kind; `expect` builds the expected stdout from
/// the kind's printed outer/inner values.
fn each_kind(template: &str, expect: impl Fn(&Kind) -> String) {
    for k in KINDS {
        let src = expand(template, k);
        let got = common::run(&src);
        assert_eq!(got, expect(k), "kind `{}`, program:\n{}", k.name, src);
    }
}

fn lines(parts: &[&str]) -> String {
    parts.iter().map(|p| format!("{p}\n")).collect()
}

// ── plain blocks ─────────────────────────────────────────────────────────────

#[test]
fn an_int_declared_in_an_inner_block_leaves_the_outer_one_alone() {
    // The reported repro.
    assert_eq!(run("mut n = 1\n{ mut n = 10 }\nprint(n)"), "1\n");
    assert_eq!(run("let n = 1\n{ let n = 10\n  print(n) }\nprint(n)"), "10\n1\n");
}

#[test]
fn an_inner_block_shadows_and_the_outer_value_is_intact_after() {
    each_kind(
        "let v$A = $O\n{ let v$A = $I\n  $S(v) }\n$S(v)",
        |k| lines(&[k.out_i, k.out_o]),
    );
}

#[test]
fn an_inner_mut_shadows_a_let() {
    each_kind(
        "let v$A = $O\n{ mut v$A = $I\n  $S(v) }\n$S(v)",
        |k| lines(&[k.out_i, k.out_o]),
    );
}

#[test]
fn reassigning_the_shadow_does_not_touch_the_outer_binding() {
    each_kind(
        "mut v$A = $O\n{ mut v$A = $I\n  v = $I\n  $S(v) }\n$S(v)\nv = $I\n$S(v)",
        |k| lines(&[k.out_i, k.out_o, k.out_i]),
    );
}

#[test]
fn assigning_an_outer_binding_from_an_inner_block_without_a_declaration_updates_it() {
    // No `let`/`mut`: this is a rebinding of the outer variable, not a shadow.
    each_kind(
        "mut v$A = $O\n{ v = $I }\n$S(v)",
        |k| lines(&[k.out_i]),
    );
}

#[test]
fn shadows_nest_three_deep() {
    each_kind(
        "let v$A = $O\n{ let v$A = $I\n  { let v$A = $O\n    $S(v) }\n  $S(v) }\n$S(v)",
        |k| lines(&[k.out_o, k.out_i, k.out_o]),
    );
}

#[test]
fn an_outer_value_read_for_the_last_time_before_the_block_is_not_moved_away() {
    // The read of `v` before the block consumes it only if nothing later can
    // read the outer `v`. The block's own `let v` must not make it look dead.
    each_kind(
        "let v$A = $O\nlet first = [v]\n{ let v$A = $I\n  $S(v) }\nprint(len(first))\n$S(v)",
        |k| lines(&[k.out_i, "1", k.out_o]),
    );
}

#[test]
fn a_shadow_that_is_moved_leaves_the_outer_binding_usable() {
    each_kind(
        "let v$A = $O\nlet kept = { let v$A = $I\n  let w = [v]\n  w }\n$S(v)\nprint(len(kept))\n$S(kept[0])",
        |k| lines(&[k.out_o, "1", k.out_i]),
    );
}

#[test]
fn a_shadow_that_is_consumed_by_a_call_leaves_the_outer_binding_usable() {
    each_kind(
        "let v$A = $O\nmut acc: List<Int> = []\n{ let v$A = $I\n  let pair = [v, v]\n  push(mut acc, len(pair)) }\n$S(v)\nprint(acc)",
        |k| lines(&[k.out_o, "[2]"]),
    );
}

#[test]
fn the_outer_binding_is_moved_on_its_real_last_use_after_the_block() {
    each_kind(
        "let v$A = $O\n{ let v$A = $I\n  $S(v) }\nlet last = [v]\nprint(len(last))",
        |k| lines(&[k.out_i, "1"]),
    );
}

// ── conditionals ─────────────────────────────────────────────────────────────

#[test]
fn shadows_in_if_and_else_branches() {
    for taken in ["true", "false"] {
        let tpl = format!(
            "func run(c: Bool) = {{\n  let v$A = $O\n  if c then {{ let v$A = $I\n    $S(v) }} else {{ let v$A = $O\n    $S(v) }}\n  $S(v)\n}}\nrun({taken})"
        );
        each_kind(&tpl, |k| if taken == "true" { lines(&[k.out_i, k.out_o]) } else { lines(&[k.out_o, k.out_o]) });
    }
}

#[test]
fn shadows_in_branches_at_the_top_level() {
    each_kind(
        "let v$A = $O\nif true then { let v$A = $I\n  $S(v) } else { let v$A = $O\n  $S(v) }\nif false then { let v$A = $O\n  $S(v) } else { let v$A = $I\n  $S(v) }\n$S(v)",
        |k| lines(&[k.out_i, k.out_i, k.out_o]),
    );
}

#[test]
fn a_bare_let_as_a_branch_is_scoped_to_the_branch() {
    // Not the union kinds: an else-less `if` widens the branch into
    // `T | None`, which cannot yet widen a union-typed `T`.
    for k in KINDS.iter().filter(|k| k.name != "union" && k.name != "tree") {
        let src = expand(
            "let v$A = $O\nif true then let v$A = $I\nif false then 0 else let v$A = $I\n$S(v)",
            k,
        );
        assert_eq!(run(&src), lines(&[k.out_o]), "kind `{}`", k.name);
    }
}

// ── loops ────────────────────────────────────────────────────────────────────

#[test]
fn a_loop_body_declares_a_fresh_shadow_each_iteration() {
    each_kind(
        "let v$A = $O\nfor i in [1, 2, 3] do {\n  let v$A = $I\n  $S(v)\n}\n$S(v)",
        |k| lines(&[k.out_i, k.out_i, k.out_i, k.out_o]),
    );
}

#[test]
fn a_loop_body_that_consumes_its_shadow_each_iteration() {
    each_kind(
        "let v$A = $O\nmut total = 0\nfor i in [1, 2, 3] do {\n  let v$A = $I\n  let w = [v]\n  total = total + len(w)\n}\nprint(total)\n$S(v)",
        |k| lines(&["3", k.out_o]),
    );
}

#[test]
fn a_for_loop_variable_shadows_an_outer_name() {
    each_kind(
        "let v$A = $O\nfor v in [$I, $I] do $S(v)\n$S(v)",
        |k| lines(&[k.out_i, k.out_i, k.out_o]),
    );
}

#[test]
fn a_for_loop_variable_shadows_an_outer_name_of_another_type() {
    each_kind(
        "let v$A = $O\nfor v in 1..3 do print(v)\nfor v in [\"p\", \"q\"] do print(v)\n$S(v)",
        |k| lines(&["1", "2", "p", "q", k.out_o]),
    );
}

#[test]
fn a_comprehension_variable_shadows_an_outer_name() {
    each_kind(
        "let v$A = $O\nlet n = len([for v in [$I, $I] do v])\nprint(n)\nlet m = len([for v in [$I, $I, $I] if true do [v]])\nprint(m)\n$S(v)",
        |k| lines(&["2", "3", k.out_o]),
    );
}

#[test]
fn a_loop_variable_read_for_the_last_time_does_not_move_the_outer_binding() {
    each_kind(
        "let v$A = $O\nfor v in [$I] do {\n  let w = [v]\n  print(len(w))\n}\nlet z = [v]\nprint(len(z))",
        |_| lines(&["1", "1"]),
    );
}

#[test]
fn an_outer_read_before_a_loop_that_redeclares_it_is_not_its_last_use() {
    each_kind(
        "let v$A = $O\nlet first = [v]\nfor i in [1, 2] do {\n  let v$A = $I\n  $S(v)\n}\nprint(len(first))\n$S(v)",
        |k| lines(&[k.out_i, k.out_i, "1", k.out_o]),
    );
}

// ── match and catch binders ─────────────────────────────────────────────────

#[test]
fn a_match_binder_shadows_an_outer_name() {
    each_kind(
        "let v$A = $O\nlet u: Int | Str = \"bound\" + \"!\"\nmatch u {\n  is Int(v) then print(v + 1)\n  is Str(v) then print(v)\n}\n$S(v)",
        |k| lines(&["bound!", k.out_o]),
    );
}

#[test]
fn each_match_arm_binds_its_own_variable() {
    each_kind(
        "let v$A = $O\nfor u in [1, 2] do {\n  let w: Int | Str = if u == 1 then \"one\" + \"!\" else 2\n  match w {\n    is Int(v) then print(v * 10)\n    is Str(v) then print(v)\n  }\n}\n$S(v)",
        |k| lines(&["one!", "20", k.out_o]),
    );
}

#[test]
fn a_match_that_narrows_its_subject_under_its_own_name_leaves_the_outer_binding() {
    // `u` is rebound inside the arm at the narrowed type; afterwards it is the union again.
    let src = format!("{PRELUDE}\nlet u: Int | Str = \"a\" + \"b\"\nlet r = match u {{\n  is Int then 0\n  is Str then len(u)\n}}\nprint(r)\nprint(u)");
    assert_eq!(run(&src), "2\n\"ab\"\n");
}

#[test]
fn a_catch_binder_shadows_an_outer_name() {
    for fail in ["true", "false"] {
        let tpl = format!(
            "error Boom(msg: Str)\nfunc f(b: Bool): Int | Boom = if b then Boom(msg = \"bo\" + \"om\") else 5\nlet v$A = $O\nlet r = f({fail}) catch [v] -> len(v.msg)\nprint(r)\n$S(v)"
        );
        let want = if fail == "true" { "4" } else { "5" };
        each_kind(&tpl, |k| lines(&[want, k.out_o]));
    }
}

#[test]
fn a_catch_binder_with_a_handler_that_moves_it() {
    each_kind(
        "error Boom(msg: Str)\nfunc f(): Int | Boom = Boom(msg = \"bo\" + \"om\")\nlet v$A = $O\nlet first = [v]\nlet r = f() catch [v] -> len([v.msg])\nprint(r)\nprint(len(first))\n$S(v)",
        |k| lines(&["1", "1", k.out_o]),
    );
}

// ── redeclaration in one scope ──────────────────────────────────────────────

#[test]
fn redeclaring_a_name_in_the_same_scope_at_the_top_level() {
    each_kind(
        "let v$A = $O\n$S(v)\nlet v$A = $I\n$S(v)",
        |k| lines(&[k.out_o, k.out_i]),
    );
}

#[test]
fn redeclaring_a_name_in_the_same_block() {
    each_kind(
        "{ let v$A = $O\n  $S(v)\n  let v$A = $I\n  $S(v) }",
        |k| lines(&[k.out_o, k.out_i]),
    );
}

#[test]
fn redeclaring_a_name_in_a_function_body() {
    each_kind(
        "func run() = {\n  let v$A = $O\n  $S(v)\n  let v$A = $I\n  $S(v)\n}\nrun()",
        |k| lines(&[k.out_o, k.out_i]),
    );
}

#[test]
fn redeclaring_a_name_at_a_different_type() {
    // Every ordered pair of kinds, at the top level and in a block.
    for a in KINDS {
        for b in KINDS {
            let top = format!(
                "{PRELUDE}\nlet v{} = {}\nshow_{}(v)\nlet v{} = {}\nshow_{}(v)",
                a.ann, a.outer, a.name, b.ann, b.inner, b.name
            );
            assert_eq!(run(&top), lines(&[a.out_o, b.out_i]), "top level {} -> {}", a.name, b.name);
            let block = format!(
                "{PRELUDE}\n{{ let v{} = {}\n  show_{}(v)\n  let v{} = {}\n  show_{}(v) }}",
                a.ann, a.outer, a.name, b.ann, b.inner, b.name
            );
            assert_eq!(run(&block), lines(&[a.out_o, b.out_i]), "block {} -> {}", a.name, b.name);
        }
    }
}

#[test]
fn an_inner_shadow_at_a_different_type_than_the_outer_binding() {
    for a in KINDS {
        for b in KINDS {
            let src = format!(
                "{PRELUDE}\nlet v{} = {}\n{{ let v{} = {}\n  show_{}(v) }}\nshow_{}(v)",
                a.ann, a.outer, b.ann, b.inner, b.name, a.name
            );
            assert_eq!(run(&src), lines(&[b.out_i, a.out_o]), "{} shadowed by {}", a.name, b.name);
        }
    }
}

#[test]
fn a_redeclaration_that_reads_the_previous_binding() {
    each_kind(
        "let v$A = $O\nlet v = [v]\nlet v = len(v)\nprint(v)",
        |_| lines(&["1"]),
    );
    each_kind(
        "let v$A = $O\n{ let v = [v, v]\n  let v = len(v)\n  print(v) }\n$S(v)",
        |k| lines(&["2", k.out_o]),
    );
}

// ── parameters ──────────────────────────────────────────────────────────────

#[test]
fn a_declaration_may_shadow_a_parameter() {
    for k in KINDS {
        let ty = match k.name {
            "int" => "Int", "str" => "Str", "list" => "List<Int>", "dict" => "Dict<Str, Int>",
            "struct" => "Pt", "tree" => "Tree", _ => "Int | Str",
        };
        let src = format!(
            "{PRELUDE}\nfunc f(v: {ty}) = {{\n  {{ let v{} = {}\n    show_{}(v) }}\n  show_{}(v)\n  let v{} = {}\n  show_{}(v)\n}}\nf({})",
            k.ann, k.inner, k.name, k.name, k.ann, k.inner, k.name, k.outer
        );
        assert_eq!(run(&src), lines(&[k.out_i, k.out_o, k.out_i]), "kind `{}`:\n{}", k.name, src);
    }
}

#[test]
fn an_early_return_inside_a_shadow_of_a_mut_parameter_hands_back_the_parameter() {
    let src = "func f(mut n: Int, c: Bool): Int = {\n  if c then {\n    let n = 100\n    print(n)\n    return 0\n  }\n  n = n + 1\n  0\n}\nmut k = 1\nf(mut k, true)\nprint(k)\nf(mut k, false)\nprint(k)";
    assert_eq!(run(src), "100\n1\n2\n");
}

#[test]
fn a_shadow_of_a_mut_list_parameter_leaves_the_caller_value_intact() {
    let src = "func f(mut xs: List<Int>) = {\n  { let xs = [9, 9]\n    print(xs) }\n  push(mut xs, 3)\n}\nmut ys = [1, 2]\nf(mut ys)\nprint(ys)";
    assert_eq!(run(src), "[9, 9]\n[1, 2, 3]\n");
}

// ── arenas ──────────────────────────────────────────────────────────────────

#[test]
fn shadowing_an_outer_binding_inside_an_arena_block() {
    // The case an earlier restriction rejected.
    each_kind(
        "let v$A = $O\nlet r = with Arena { let v$A = $I\n  $S(v)\n  1 }\nprint(r)\n$S(v)",
        |k| lines(&[k.out_i, "1", k.out_o]),
    );
}

#[test]
fn an_arena_block_may_return_its_shadow() {
    each_kind(
        "let v$A = $O\nlet r$A = with Arena { let v$A = $I\n  v }\n$S(r)\n$S(v)",
        |k| lines(&[k.out_i, k.out_o]),
    );
}

#[test]
fn nested_arenas_and_blocks_shadow_independently() {
    each_kind(
        "let v$A = $O\nwith Arena {\n  let v$A = $I\n  with Arena {\n    let v$A = $O\n    $S(v)\n  }\n  { let v$A = $I\n    $S(v) }\n  $S(v)\n}\n$S(v)",
        |k| lines(&[k.out_o, k.out_i, k.out_i, k.out_o]),
    );
}

#[test]
fn an_arena_block_that_moves_its_shadow_leaves_the_outer_binding() {
    each_kind(
        "let v$A = $O\nlet n = with Arena { let v$A = $I\n  let w = [v]\n  len(w) }\nprint(n)\n$S(v)",
        |k| lines(&["1", k.out_o]),
    );
}

#[test]
fn a_loop_in_an_arena_shadows_its_variable() {
    each_kind(
        "let v$A = $O\nwith Arena {\n  for v in [$I, $I] do $S(v)\n  for i in [1, 2] do {\n    let v$A = $I\n    $S(v)\n  }\n}\n$S(v)",
        |k| lines(&[k.out_i, k.out_i, k.out_i, k.out_i, k.out_o]),
    );
}

#[test]
fn a_match_binder_in_an_arena_shadows_an_outer_name() {
    each_kind(
        "let v$A = $O\nwith Arena {\n  let u: Int | Str = \"in\" + \"ner\"\n  match u {\n    is Int(v) then print(v)\n    is Str(v) then print(v)\n  }\n}\n$S(v)",
        |k| lines(&["inner", k.out_o]),
    );
}

// ── the REPL: top-level bindings persist by name across entries ─────────────

mod repl {
    use froglang_core::state::FrogState;

    fn show(s: &mut FrogState, src: &str) -> String {
        format!("{:?}", s.eval(src).unwrap().0)
    }

    #[test]
    fn a_later_entry_may_redeclare_a_binding_at_another_type() {
        // `FrogState` checks for leaks when it is dropped.
        let mut s = FrogState::new();
        s.eval("let v = \"a\" + \"b\"").unwrap();
        assert_eq!(show(&mut s, "v"), "Str(\"ab\")");
        s.eval("let v = [1, 2]").unwrap();
        assert_eq!(show(&mut s, "v"), "List([Int(1), Int(2)])");
        s.eval("let v = 5").unwrap();
        assert_eq!(show(&mut s, "v"), "Int(5)");
        s.eval("let v = \"back\" + \"!\"").unwrap();
        assert_eq!(show(&mut s, "v"), "Str(\"back!\")");
    }

    #[test]
    fn one_entry_may_redeclare_a_name_it_declared_before() {
        let mut s = FrogState::new();
        s.eval("let w = \"x\" + \"y\"\nlet w = 7").unwrap();
        assert_eq!(show(&mut s, "w"), "Int(7)");
        s.eval("let w = [\"p\" + \"q\"]\nlet w = \"r\" + \"s\"").unwrap();
        assert_eq!(show(&mut s, "w"), "Str(\"rs\")");
    }

    #[test]
    fn an_inner_block_in_a_later_entry_does_not_replace_an_earlier_binding() {
        let mut s = FrogState::new();
        s.eval("let z = \"keep\" + \"!\"").unwrap();
        s.eval("{ let z = \"gone\" + \"!\"\n  print(z) }").unwrap();
        assert_eq!(show(&mut s, "z"), "Str(\"keep!\")");
        s.eval("for z in [\"a\" + \"b\"] do print(z)").unwrap();
        assert_eq!(show(&mut s, "z"), "Str(\"keep!\")");
    }

    #[test]
    fn reassigning_a_binding_from_an_earlier_entry_still_updates_it() {
        let mut s = FrogState::new();
        s.eval("mut m = \"one\" + \"!\"").unwrap();
        s.eval("m = \"two\" + \"!\"").unwrap();
        assert_eq!(show(&mut s, "m"), "Str(\"two!\")");
        s.eval("{ m = \"three\" + \"!\" }").unwrap();
        assert_eq!(show(&mut s, "m"), "Str(\"three!\")");
    }
}
