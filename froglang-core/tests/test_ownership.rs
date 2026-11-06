//! Programs whose output must not depend on the memory model: each one exercises
//! a path that ownership codegen (`FROG_OWNERSHIP`, plans/NOGC.md §6 step 2)
//! frees or copies, and that the GC model handles by collection instead. The suite
//! runs in both modes, and `FROG_LEAK_CHECK` reports any object a run leaves live.

mod common;
use common::run;

#[test]
fn user_iterable_loop_sums_its_items() {
    let src = r#"
data Countdown(n: Int) provides Iterable {
  func next(mut s: Countdown): Int? = if s.n <= 0 then none else { let v = s.n
    s.n = s.n - 1
    v }
}
mut total = 0
for x in Countdown(n=4) do {
  total = total + x
}
print(total)
"#;
    assert_eq!(run(src), "10\n");
}

#[test]
fn user_iterable_of_strings_owns_each_element() {
    let src = r#"
data Words(items: List<Str>, pos: Int) provides Iterable {
  func next(mut s: Words): Str? = if s.pos >= len(s.items) then none else { let w = s.items[s.pos]
    s.pos = s.pos + 1
    w }
}
for w in Words(items=["alpha", "beta", "gamma"], pos=0) do {
  print(w + "!")
}
print("done")
"#;
    assert_eq!(run(src), "alpha!\nbeta!\ngamma!\ndone\n");
}

#[test]
fn reassigning_a_local_inside_a_loop_keeps_the_last_value() {
    let src = r#"
func go(n: Int): Str = {
  mut best = "none"
  for i in 0..n do {
    best = "k"
    0
  }
  best
}
print(go(2))
"#;
    assert_eq!(run(src), "k\n");
}

#[test]
fn reassigning_a_top_level_binding_inside_a_loop_is_visible_after_it() {
    let src = r#"
mut best = "none"
for i in 0..1 do {
  best = "k"
}
print(best)
"#;
    assert_eq!(run(src), "k\n");
}

#[test]
fn reassigning_a_top_level_binding_inside_a_block_is_visible_after_it() {
    let src = r#"
mut best = "none"
{ best = "k" }
print(best)
"#;
    assert_eq!(run(src), "k\n");
}

#[test]
fn question_mark_passes_an_ok_value_through_a_function() {
    let src = r#"
error ParseError(msg: Str)
func parse(s: Str): Int | ParseError = if s == "bad" then ParseError(msg="nope") else 42
func run(s: Str): Int | ParseError = {
  let n = parse(s)?
  n * 2
}
print(run("ok"))
"#;
    assert_eq!(run(src), "84\n");
}

#[test]
fn dict_insert_remove_and_overwrite_keep_their_values() {
    let src = r#"
mut d = ["a": "x"]
d["b"] = "y"
d["a"] = "z"
print(d)
print(d.remove("a"))
print(d)
print(len(d))
"#;
    assert_eq!(run(src), "[\"a\": \"z\", \"b\": \"y\"]\n\"z\"\n[\"b\": \"y\"]\n1\n");
}

#[test]
fn a_copy_does_not_alias_the_original_list() {
    let src = r#"
mut a = [1, 2]
let b = a
a[0] = 9
print(b)
print(a)
func bump(mut xs: List<Int>): None = push(mut xs, 7)
let c = a
bump(mut a)
print(c)
print(a)
"#;
    assert_eq!(run(src), "[1, 2]\n[9, 2]\n[9, 2]\n[9, 2, 7]\n");
}

#[test]
fn a_string_copy_is_independent_of_its_source() {
    let src = r#"
mut s = "ab"
let t = s
print(t)
mut d = ["k": "v"]
mut ks = d.keys()
mut vs = d.values()
print(ks)
print(vs)
print(len(s + t))
"#;
    assert_eq!(run(src), "ab\n[\"k\"]\n[\"v\"]\n4\n");
}
