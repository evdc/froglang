//! `Pool<T>` and `Id<T>` (plans/NOGC.md step 5): graph data addressed by
//! generational ids. Every program goes through `common::run`, so each one
//! is also a leak test. Stale-id panics go through `run_raw`, since they
//! exit with status 1 rather than succeeding.

mod common;
use common::{run, run_raw};

/// Run `src` expecting a stale-id panic. Returns stderr.
fn stale_panic(src: &str) -> String {
    let out = run_raw(src);
    assert_eq!(out.status, Some(1), "expected a clean abort, got {:?}\nstderr:\n{}", out.status, out.stderr);
    assert!(out.stderr.contains("stale Id"), "{}", out.stderr);
    out.stderr
}

/// Run `src` expecting a compile-time rejection. Returns its diagnostic text.
fn rejected(src: &str) -> String {
    let out = run_raw(src);
    assert_ne!(out.status, Some(0), "expected a compile error, but it ran:\n{}", out.stdout);
    format!("{}{}", out.stdout, out.stderr)
}

// ── basics ───────────────────────────────────────────────────────────────────

#[test]
fn add_returns_an_id_that_indexes_the_element() {
    let src = r#"
data Node(label: Str, edges: List<Int>)
mut g = Pool()
let a = g.add(Node(label = "a", edges = []))
let b = g.add(Node(label = "b", edges = [a.index]))
print(g[a].label)
print(g[b].label)
print(g[b].edges)
"#;
    assert_eq!(run(src), "a\nb\n[0]\n");
}

#[test]
fn an_id_is_a_plain_struct_with_index_and_gen() {
    let src = "mut g = Pool()\nlet a = g.add(7)\nprint(a.index)\nprint(a.gen)\nprint(a == a)";
    assert_eq!(run(src), "0\n0\ntrue\n");
}

#[test]
fn pool_values_can_be_scalars_and_strings() {
    let src = r#"
mut ints = Pool()
let i = ints.add(41)
print(ints[i] + 1)
mut strs = Pool()
let s = strs.add("hi")
print(strs[s] + "!")
mut eithers = Pool()
let e = eithers.add(3)
print(eithers[e])
"#;
    assert_eq!(run(src), "42\nhi!\n3\n");
}

#[test]
fn an_element_can_be_written_through_its_id() {
    let src = r#"
data Node(label: Str, edges: List<Int>)
mut g = Pool()
let a = g.add(Node(label = "a", edges = []))
let b = g.add(Node(label = "b", edges = []))
g[a].edges.push(b.index)
g[a].label = "A"
g[b] = Node(label = "B", edges = [a.index])
print(g[a].label)
print(g[a].edges)
print(g[b].label)
"#;
    assert_eq!(run(src), "A\n[1]\nB\n");
}

#[test]
fn a_cycle_is_just_ids_pointing_at_each_other() {
    let src = r#"
data Node(label: Str, edges: List<Id<Node>>)
mut g = Pool()
let a = g.add(Node(label = "a", edges = []))
let b = g.add(Node(label = "b", edges = [a]))
g[a].edges.push(b)
print(g[g[a].edges[0]].edges[0] == a)
print(g[g[g[a].edges[0]].edges[0]].label)
"#;
    assert_eq!(run(src), "true\na\n");
}

#[test]
fn a_pool_passed_as_a_mut_parameter_is_updated_in_place() {
    let src = r#"
func bump(mut p: Pool<Int>, id: Id<Int>) = {
  p[id] = p[id] + 10
}
func fill(mut p: Pool<Int>, n: Int): Id<Int> = p.add(n)
mut g = Pool()
let a = g.add(1)
bump(mut g, a)
let b = fill(mut g, 5)
print(g[a])
print(g[b])
"#;
    assert_eq!(run(src), "11\n5\n");
}

// ── remove, reuse and generations ────────────────────────────────────────────

#[test]
fn get_returns_none_for_a_removed_id() {
    let src = r#"
mut g = Pool()
let a = g.add("x")
print(g.get(a))
g.remove(a)
print(g.get(a))
"#;
    // A union's members print with their quotes; a bare `Str` does not.
    assert_eq!(run(src), "\"x\"\nnone\n");
}

#[test]
fn a_freed_slot_is_reused_under_a_new_generation() {
    let src = r#"
mut g = Pool()
let a = g.add("first")
g.remove(a)
let b = g.add("second")
print(b.index)
print(b.gen)
print(g[b])
print(g.get(a))
"#;
    assert_eq!(run(src), "0\n1\nsecond\nnone\n");
}

#[test]
fn the_free_list_is_last_in_first_out() {
    let src = r#"
mut g = Pool()
let a = g.add(1)
let b = g.add(2)
let c = g.add(3)
g.remove(a)
g.remove(c)
let d = g.add(4)
let e = g.add(5)
print(d.index)
print(e.index)
print(g[b])
"#;
    assert_eq!(run(src), "2\n0\n2\n");
}

#[test]
fn reading_a_removed_id_panics() {
    stale_panic("mut g = Pool()\nlet a = g.add(1)\ng.remove(a)\nprint(g[a])");
}

#[test]
fn writing_through_a_removed_id_panics() {
    stale_panic("mut g = Pool()\nlet a = g.add(1)\ng.remove(a)\ng[a] = 2");
}

#[test]
fn removing_a_removed_id_panics() {
    stale_panic("mut g = Pool()\nlet a = g.add(1)\ng.remove(a)\ng.remove(a)");
}

#[test]
fn a_stale_id_still_panics_after_its_slot_is_reused() {
    stale_panic("mut g = Pool()\nlet a = g.add(1)\ng.remove(a)\nlet b = g.add(2)\nprint(g[a])");
}

// ── leaks: removed and replaced heap values are freed ───────────────────────

#[test]
fn heap_values_are_freed_across_removal_reuse_and_replacement() {
    let src = r#"
data Node(label: Str, edges: List<Int>)
mut g = Pool()
let a = g.add(Node(label = "aaaaaaaaaaaaaaaaaaaaaa", edges = [1, 2, 3]))
g.remove(a)
let b = g.add(Node(label = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", edges = [4]))
g[b] = Node(label = "replacement-replacement-replacement", edges = [5])
g[b].edges.push(9)
print(g[b].edges)
mut t = Pool()
for i in [1, 2, 3] do {
  let id = t.add("a heap string that is long enough, number " + repr(i))
  t.remove(id)
}
print(len(t.slots))
"#;
    assert_eq!(run(src), "[5, 9]\n1\n");
}

// ── typing ───────────────────────────────────────────────────────────────────

#[test]
fn an_id_from_one_pool_type_cannot_index_another() {
    let err = rejected("mut a = Pool()\nmut b = Pool()\nlet x = a.add(1)\nlet y = b.add(\"s\")\nprint(b[x])");
    assert!(err.contains("takes an Id<"), "{err}");
}

#[test]
fn adding_a_value_of_the_wrong_type_is_an_error() {
    let err = rejected("mut g = Pool()\nlet a = g.add(1)\ng.add(\"s\")");
    assert!(err.contains("Pool<Int>.add takes"), "{err}");
}

#[test]
fn an_integer_is_not_an_id() {
    let err = rejected("mut g = Pool()\nlet a = g.add(1)\nprint(g[3])");
    assert!(err.contains("takes an Id<"), "{err}");
}

#[test]
fn an_immutable_pool_cannot_be_added_to() {
    let err = rejected("let g = Pool()\ng.add(1)");
    assert!(err.contains("not mutable"), "{err}");
}

#[test]
fn an_unknown_method_is_an_error() {
    let err = rejected("mut g = Pool()\ng.clear()");
    assert!(err.contains("'clear'"), "{err}");
}

// ── arenas ───────────────────────────────────────────────────────────────────

#[test]
fn an_arena_block_can_build_and_read_its_own_pool() {
    let src = r#"
let total = with Arena {
  mut k = Pool()
  let a = k.add(40)
  let b = k.add(2)
  k[a] + k[b]
}
print(total)
"#;
    assert_eq!(run(src), "42\n");
}

#[test]
fn an_arena_block_cannot_mutate_an_outer_pool() {
    let err = rejected("mut g = Pool()\nwith Arena { g.add(1) }");
    assert!(err.contains("outside this 'with Arena' block"), "{err}");
}

// ── unsupported forms are rejected rather than miscompiled ──────────────────

#[test]
fn a_pool_method_through_a_field_is_rejected() {
    let err = rejected("data Holder(p: Pool<Int>)\nmut h = Holder(p = Pool())\nlet a = h.p.add(1)");
    assert!(err.contains("'add'"), "{err}");
}

// ── review regressions ───────────────────────────────────────────────────────

#[test]
fn a_user_can_declare_data_slot() {
    let src = "data Slot(x: Int)\nmut g = Pool()\nlet a = g.add(Slot(x = 5))\nprint(g[a].x)";
    assert_eq!(run(src), "5\n");
}

#[test]
fn get_with_an_out_of_range_index_is_none() {
    let src = "mut g = Pool()\nlet _a = g.add(1)\nmut h = Pool()\nlet _x = h.add(1)\nlet b = g.add(2)\nmut k = Pool()\nlet _y = k.add(9)\nprint(k.get(b) == none)";
    assert_eq!(run(src), "true\n");
}

#[test]
fn indexing_and_removing_with_an_out_of_range_id_is_a_stale_panic() {
    let pre = "mut g = Pool()\nlet _a = g.add(1)\nmut h = Pool()\nlet _b = h.add(1)\nlet b2 = h.add(2)\n";
    stale_panic(&format!("{pre}print(g[b2])"));
    stale_panic(&format!("{pre}g.remove(b2)"));
}

#[test]
fn a_negative_index_id_is_stale_not_wrapped() {
    let pre = "mut g = Pool()\nlet _a = g.add(1)\nlet _b = g.add(2)\n";
    stale_panic(&format!("{pre}print(g[Id(index = -1, gen = 0)])"));
    stale_panic(&format!("{pre}g.remove(Id(index = -1, gen = 0))"));
    assert_eq!(run(&format!("{pre}print(g.get(Id(index = -1, gen = 0)) == none)")), "true\n");
}

#[test]
fn reading_a_linear_element_out_of_a_pool_is_rejected() {
    let head = "data Buf(x: Int) provides Linear\nfunc f(): Int = {\n  mut g = Pool()\n  let a = g.add(Buf(x = 1))\n";
    let msg = rejected(&format!("{head}  let v = g[a]\n  v.x\n}}\nprint(f())"));
    assert!(msg.contains("Linear"), "{msg}");
    let msg = rejected(&format!("{head}  let v = g.get(a)\n  0\n}}\nprint(f())"));
    assert!(msg.contains("Linear"), "{msg}");
}

#[test]
fn reading_a_field_of_a_linear_pool_element_is_fine() {
    let src = "data Buf(x: Int) provides Linear\nfunc f(): Int = {\n  mut g = Pool()\n  let a = g.add(Buf(x = 1))\n  g[a].x\n}\nprint(f())";
    assert_eq!(run(src), "1\n");
}

#[test]
fn an_annotated_pool_binding_type_checks() {
    let src = "mut g: Pool<Int> = Pool()\nlet a = g.add(3)\nprint(g[a])";
    assert_eq!(run(src), "3\n");
    let err = rejected("mut g: Pool<Int> = Pool()\ng.add(\"x\")");
    assert!(err.contains("Pool<Int>.add takes a Int, got Str"), "{}", err);
}
