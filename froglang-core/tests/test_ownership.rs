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

/// An early exit (`?`) in a later operand of a call, list, struct, variant or
/// dict literal must still free the operands already compiled: they are not
/// handed to the consumer until it runs. Under `FROG_LEAK_CHECK` a leak shows
/// on stderr.
#[test]
fn an_early_exit_in_a_later_operand_frees_the_earlier_ones() {
    let src = r#"
error ParseError(msg: Str)
data P(name: Str, n: Int)
data Box is Full(s: Str, n: Int) | Empty
func parse(s: Str): Int | ParseError = if s == "bad" then ParseError(msg="nope") else 42
func mk(s: Str): Str = s + "!"
func g(a: Str, n: Int): Int = len(a) + n
func call_case(s: Str): Int | ParseError = g(mk(s), parse(s)?)
func list_case(s: Str): Int | ParseError = { let xs = [mk(s), mk(s) + s, if parse(s)? > 0 then "q" else "r"]
  len(xs) }
func struct_case(s: Str): Int | ParseError = { let p = P(name=mk(s), n=parse(s)?)
  p.n }
func variant_case(s: Str): Int | ParseError = { let b = Full(s=mk(s), n=parse(s)?)
  1 }
func dict_case(s: Str): Int | ParseError = { let d = ["a": mk(s), mk(s): if parse(s)? > 0 then mk(s) else "w", "z": mk(s)]
  len(d) }
for s in ["ok", "bad"] do {
  print(call_case(s))
  print(list_case(s))
  print(struct_case(s))
  print(variant_case(s))
  print(dict_case(s))
}
"#;
    let out = common::run_raw(src);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains("leak:"), "leaked: {}", out.stderr);
    let err = "ParseError(msg=\"nope\")\n";
    assert_eq!(out.stdout, format!("45\n3\n42\n1\n3\n{}", err.repeat(5)));
}

/// Testing a union's tag (a `match`, an iterable loop's end test) reads it in
/// place; the value is still intact and owned afterwards.
#[test]
fn matching_a_live_union_leaves_it_intact() {
    let src = r#"
data Words(items: List<Str>, pos: Int) provides Iterable {
  func next(mut s: Words): Str? = if s.pos >= len(s.items) then none else { let w = s.items[s.pos]
    s.pos = s.pos + 1
    w }
}
data Shape is Circle(name: Str) | Sq(side: Int) | Dot
func f(x: Shape): Int = match x {
is Circle(c) then len(c)
is Sq(q) then q
is Dot then 0
}
func g(x: Str | Int | None): Int = match x {
is Str(s) then len(s)
is Int(i) then i
is None then 0
}
mut t = 0
for w in Words(items=["alpha", "beta"], pos=0) do { t = t + len(w) }
print(t)
let c = Circle(name="round")
print(f(c) + f(Sq(side=3)) + f(Dot))
print(c)
let u: Str | Int | None = "hello"
print(g(u))
print(u)
"#;
    let out = common::run_raw(src);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains("leak:"), "leaked: {}", out.stderr);
    assert_eq!(out.stdout, "9\n8\nShape.Circle(name=\"round\")\n5\n\"hello\"\n");
}

#[test]
fn a_deep_list_is_copied_and_freed_without_recursing() {
    // 200k nested `Cons` nodes: a recursive copy or free overflows the stack.
    let src = r#"
data L is Cons(h: Int, t: L) | Nil
func head(l: L): Int = match l {
is Cons(h, t) then h
is Nil then 0
}
func go(n: Int): Int = {
  mut xs: L = Nil
  for i in 0..n do { xs = Cons(h=i, t=xs) }
  mut ys = xs
  ys = Cons(h=7, t=ys)
  head(xs) + head(ys)
}
print(go(200000))
"#;
    assert_eq!(run(src), "200006\n");
}
