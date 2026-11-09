//! `with Arena { body }` (plans/NOGC.md step 4): the body's allocations bump
//! a region, its frees are no-ops, and its result is copied out when it
//! ends. `common::run` runs each program under the leak check, so a program
//! that leaves an outer object unfreed, or frees an arena object twice,
//! fails here.

mod common;
use common::{run, run_raw};

// ── results of each type ─────────────────────────────────────────────────────

#[test]
fn an_arena_block_returns_each_kind_of_value() {
    let src = r#"
data Point(x: Int, y: Int)
data Person(name: Str, tags: List<Str>)
data Tree is Leaf | Branch(left: Tree, right: Tree, label: Str)
data Shape is Circle(r: Int) | Square(w: Int)
func labels(t: Tree): Str = match t {
  is Leaf then "."
  is Branch(l, r, label) then label + "(" + labels(l) + labels(r) + ")"
}

let i = with Arena { 6 * 7 }
let s = with Arena { "ab" + "cd" }
let xs = with Arena { [for n in [1, 2, 3] do n * 2] }
let d = with Arena { ["a": [1, 2], "b": [3]] }
let p = with Arena { Point(x = 1, y = 2) }
let q = with Arena { Person(name = "Ada" + "!", tags = ["x" + "y", "z"]) }
let t = with Arena { Branch(left = Leaf, right = Branch(left = Leaf, right = Leaf, label = "in" + "ner"), label = "top") }
let sh = with Arena { Circle(r = 4) }
let u: Int | Str = with Arena { "union" + "!" }
let n = with Arena { none }
print(i)
print(s)
print(xs)
print(d)
print(p)
print(q)
print(labels(t))
print(sh)
print(u)
print(n)
"#;
    assert_eq!(run(src), concat!(
        "42\n", "abcd\n", "[2, 4, 6]\n", "[\"a\": [1, 2], \"b\": [3]]\n",
        "Point(x=1, y=2)\n", "Person(name=\"Ada!\", tags=[\"xy\", \"z\"])\n",
        "top(.inner(..))\n", "Shape.Circle(r=4)\n", "\"union!\"\n", "none\n",
    ));
}

#[test]
fn an_arena_blocks_result_survives_the_arena_and_can_be_changed() {
    let src = r#"
mut xs = with Arena { ["a" + "b", "c"] }
push(mut xs, "d")
xs[0] = "z"
print(xs)
"#;
    assert_eq!(run(src), "[\"z\", \"c\", \"d\"]\n");
}

// ── loops and nesting ────────────────────────────────────────────────────────

#[test]
fn arena_blocks_in_a_loop_accumulate_outside_results() {
    let src = r#"
mut sums: List<Str> = []
for i in 0..4 do {
  let s = with Arena {
    mut acc = ""
    for k in 0..i + 1 do { acc = acc + repr(k) + "." }
    acc
  }
  push(mut sums, s)
}
print(sums)
"#;
    assert_eq!(run(src), "[\"0.\", \"0.1.\", \"0.1.2.\", \"0.1.2.3.\"]\n");
}

#[test]
fn nested_arena_blocks_copy_out_one_level_at_a_time() {
    let src = r#"
let r = with Arena {
  let a = ["outer" + "!"]
  let b = with Arena {
    let c = with Arena { ["inner" + "!", "deep"] }
    [c[0], a[0], c[1]]
  }
  [b[2], b[0], b[1]]
}
print(r)
"#;
    assert_eq!(run(src), "[\"deep\", \"inner!\", \"outer!\"]\n");
}

// ── outer bindings ───────────────────────────────────────────────────────────

#[test]
fn an_arena_block_reads_outer_bindings_and_leaves_them_intact() {
    let src = r#"
let names = ["ann", "bo"]
let tag = "id-"
let r = with Arena {
  [for n in names do tag + n]
}
print(r)
print(names)
"#;
    assert_eq!(run(src), "[\"id-ann\", \"id-bo\"]\n[\"ann\", \"bo\"]\n");
}

#[test]
fn the_last_use_of_an_outer_binding_inside_a_block_is_a_copy_not_a_move() {
    // `names` is not read after the block, so it would be a `Move` anywhere
    // else. In an arena it must stay the outer scope's to free.
    let src = r#"
let names = ["ann", "bo"]
let r = with Arena { names }
print(r)
let t = "tail"
let u = with Arena { t }
print(u)
"#;
    assert_eq!(run(src), "[\"ann\", \"bo\"]\ntail\n");
}

#[test]
fn an_outer_binding_passed_to_a_consuming_function_is_copied_in() {
    let src = r#"
func keep(xs: List<Str>): List<Str> = {
  mut ys = xs
  push(mut ys, "!")
  ys
}
let base = ["a", "b"]
let r = with Arena { keep(base) }
print(r)
print(base)
"#;
    assert_eq!(run(src), "[\"a\", \"b\", \"!\"]\n[\"a\", \"b\"]\n");
}

#[test]
fn bindings_made_inside_a_block_may_be_mutated_freely() {
    let src = r#"
let r = with Arena {
  mut acc: List<Str> = []
  for i in 0..5 do {
    push(mut acc, "n" + repr(i))
    acc[0] = "first"
  }
  mut count = 0
  count = count + len(acc)
  acc.push(repr(count))
  acc
}
print(r)
"#;
    assert_eq!(run(src), "[\"first\", \"n1\", \"n2\", \"n3\", \"n4\", \"5\"]\n");
}

#[test]
fn a_function_called_in_a_block_allocates_in_the_arena() {
    let src = r#"
func build(n: Int): List<Str> = {
  mut out: List<Str> = []
  for i in 0..n do { push(mut out, "item" + repr(i)) }
  out
}
func total(xs: List<Str>): Int = {
  mut t = 0
  for x in xs do { t = t + len(x) }
  t
}
let n = with Arena { total(build(100)) }
print(n)
"#;
    assert_eq!(run(src), "590\n");
}

// ── leaving a block early ────────────────────────────────────────────────────

#[test]
fn return_inside_an_arena_block_copies_the_value_out() {
    let src = r#"
func pick(n: Int): List<Str> = {
  let outer = ["o" + repr(n)]
  let r = with Arena {
    let mid = ["m" + "!", outer[0]]
    if n > 2 then return [mid[0], outer[0], "early"]
    [mid[0], mid[1], "late"]
  }
  [r[0], r[1], r[2], "done"]
}
print(pick(1))
print(pick(5))
"#;
    assert_eq!(run(src), "[\"m!\", \"o1\", \"late\", \"done\"]\n[\"m!\", \"o5\", \"early\"]\n");
}

#[test]
fn return_from_nested_arena_blocks_leaves_every_one() {
    let src = r#"
func f(n: Int): Str = {
  let outer = "keep"
  let a = with Arena {
    let b = with Arena {
      if n > 0 then return outer + "-" + repr(n)
      "inner"
    }
    b + "!"
  }
  a + "?"
}
print(f(7))
print(f(0))
"#;
    assert_eq!(run(src), "keep-7\ninner!?\n");
}

#[test]
fn the_error_operator_inside_an_arena_block_propagates_out() {
    let src = r#"
error Oops(msg: Str) provides Error
func risky(n: Int): Str | Oops = if n > 1 then Oops(msg = "big " + repr(n)) else "ok" + repr(n)
func run(n: Int): Str | Oops = {
  let kept = ["k"]
  let s = with Arena {
    let t = risky(n)? + "!"
    t + kept[0]
  }
  s + "."
}
print(run(0))
print(run(5) catch e -> "caught")
"#;
    assert_eq!(run(src), "\"ok0!k.\"\ncaught\n");
}

#[test]
fn return_without_a_value_inside_an_arena_block() {
    let src = r#"
func f(n: Int) = {
  with Arena {
    let s = "x" + repr(n)
    if n > 1 then return
    print(s)
  }
  print("after")
}
f(1)
f(2)
"#;
    assert_eq!(run(src), "x1\nafter\n");
}

// ── memory ───────────────────────────────────────────────────────────────────

/// The `bytes` figure `gc_dump()` printed, one per call.
fn dumped_bytes(stderr: &str) -> Vec<usize> {
    stderr.lines().filter(|l| l.starts_with("=== heap:"))
        .map(|l| l.split(", ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap())
        .collect()
}

#[test]
fn a_loop_of_arena_blocks_keeps_the_heap_flat() {
    let src = r#"
mut total = 0
for i in 0..3000 do {
  total = total + with Arena {
    let xs = [for k in 0..50 do repr(k) + "-" + repr(i)]
    len(xs)
  }
  if i == 10 or i == 2999 then gc_dump()
}
print(total)
"#;
    let out = run_raw(src);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let bytes = dumped_bytes(&out.stderr);
    assert_eq!(bytes.len(), 2, "{}", out.stderr);
    assert_eq!(bytes[0], bytes[1], "the heap grew across arena blocks: {}", out.stderr);
}

// ── the outer state is frozen ────────────────────────────────────────────────

fn rejected(src: &str) -> String {
    let out = run_raw(src);
    assert_ne!(out.status, Some(0), "expected a compile error, got: {}", out.stdout);
    format!("{}{}", out.stdout, out.stderr)
}

const FROZEN: &str = "outside this 'with Arena' block";

#[test]
fn assigning_an_outer_binding_inside_a_block_is_an_error() {
    let err = rejected("mut n = 0\nwith Arena { n = 5 }\nprint(n)");
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn assigning_into_an_outer_struct_inside_a_block_is_an_error() {
    let err = rejected("data P(x: Int)\nmut p = P(x = 1)\nwith Arena { p.x = 5 }\nprint(p.x)");
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn assigning_an_outer_list_element_inside_a_block_is_an_error() {
    let err = rejected("mut xs = [1, 2]\nwith Arena { xs[0] = 5 }\nprint(xs)");
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn assigning_an_outer_dict_entry_inside_a_block_is_an_error() {
    let err = rejected("mut d = [\"a\": 1]\nwith Arena { d[\"b\"] = 5 }\nprint(d)");
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn pushing_to_an_outer_list_inside_a_block_is_an_error() {
    let err = rejected("mut xs: List<Str> = []\nwith Arena { push(mut xs, \"a\" + \"b\") }\nprint(xs)");
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn a_mutating_method_on_an_outer_list_inside_a_block_is_an_error() {
    let err = rejected("mut xs: List<Int> = []\nwith Arena { xs.push(1) }\nprint(xs)");
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn passing_an_outer_binding_as_a_mut_argument_inside_a_block_is_an_error() {
    let src = "func bump(mut n: Int) = { n = n + 1 }\nmut k = 0\nwith Arena { bump(mut k) }\nprint(k)";
    let err = rejected(src);
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn a_binding_in_an_enclosing_block_is_still_frozen_by_a_nested_arena() {
    let src = "with Arena {\n  mut xs: List<Str> = []\n  with Arena { push(mut xs, \"a\") }\n  print(xs)\n}";
    let err = rejected(src);
    assert!(err.contains(FROZEN), "{err}");
}

#[test]
fn only_arena_is_a_with_form() {
    let err = rejected("with Scope { 1 }");
    assert!(err.contains("only `with Arena"), "{err}");
}
