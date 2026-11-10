//! `TRAITS.md` Part 7: the `Linear` marker trait — a built-in `Trait`
//! variant for now (see `frontend::linear`'s own doc comment for why, and
//! for the future-prelude-declaration refactor path). Covers both checks:
//! rule 1 (no `Copy`-classified — i.e. aliasing — read of a `Linear` value),
//! rule 2 (a `Linear` binding consumed on one `if`/`match` branch must
//! be consumed on every branch or none) and rule 3 (it must be consumed on
//! every path that doesn't fail before it goes out of scope), plus the two exemptions from rule
//! 1 that make `mut self`-style usage actually usable: a function's own
//! `mut` parameter, and a `PlaceAssign`'s own root. Also how `match` consumes
//! a Linear subject: once, by taking it apart or by handing the narrowed
//! subject to the arm; that a value containing a Linear value is Linear
//! itself; and that reading a field inspects a value unless it is the
//! value's last use, which takes it apart.

use froglang_core::state::FrogState;

#[test]
fn a_type_without_provides_linear_is_unaffected() {
    let mut s = FrogState::new();
    let src = "data P(x: Int)\nlet a = P(x=1)\nlet b = a\na.x + b.x";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(2)");
}

#[test]
fn reading_a_linear_value_twice_inside_a_function_body_is_rejected() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func f(): Int = {\n\
               let a = Buf(x=1)\n\
               let b = a\n\
               a.x\n\
               }\n\
               f()";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("'a' is Linear"), "{err}");
    assert!(err.contains("aliased"), "{err}");
}

#[test]
fn moving_a_linear_value_into_a_new_binding_is_fine_when_never_read_again() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func f(): Int = {\n\
               let a = Buf(x=1)\n\
               let b = a\n\
               b.x\n\
               }\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(1)");
}

#[test]
fn a_linear_value_consumed_on_only_one_branch_is_rejected() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func consume(b: Buf): Int = b.x\n\
               func f(): Int = {\n\
               let a = Buf(x=1)\n\
               if true then consume(a) else 0\n\
               }\n\
               f()";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("'a' is Linear"), "{err}");
    assert!(err.contains("consumed on only one branch"), "{err}");
}

#[test]
fn a_linear_value_consumed_on_every_branch_is_accepted() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func consume(b: Buf): Int = b.x\n\
               func f(): Int = {\n\
               let a = Buf(x=1)\n\
               if true then consume(a) else consume(a)\n\
               }\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(1)");
}

#[test]
fn a_linear_value_never_consumed_on_any_path_is_rejected() {
    // `plans/NOGC.md` §3.6: consumed on every success path, not "every branch
    // or none" — consuming it on none of them leaks it.
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func f(): Int = {\n\
               let a = Buf(x=1)\n\
               if true then 1 else 0\n\
               }\n\
               f()";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("'a' is Linear and is not consumed on every path"), "{err}");
}

#[test]
fn an_unconsumed_linear_parameter_is_rejected() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func f(b: Buf): Int = 1\n\
               f(Buf(x=1))";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("'b' is Linear and is not consumed on every path"), "{err}");
}

#[test]
fn a_linear_value_declared_in_a_branch_must_be_consumed_there() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func consume(b: Buf): Int = b.x\n\
               func f(c: Bool): Int = if c then { let a = Buf(x=1); 0 } else 0\n\
               f(true)";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("'a' is Linear and is not consumed on every path"), "{err}");
}

#[test]
fn a_linear_value_that_ends_a_block_is_consumed_by_it() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func consume(b: Buf): Int = b.x\n\
               func f(): Int = consume({ let a = Buf(x=3); a })\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(3)");
}

#[test]
fn a_mut_self_style_parameter_can_read_its_own_field_repeatedly() {
    // The realistic `Sink`/`mut self` shape (`TRAITS.md` Part 7, `DATA.md`
    // Stage 4): a `mut` parameter's copy-out forces every read of it to be
    // `Copy`-classified by `liveness.rs`'s own convention, which would
    // otherwise make this pattern impossible to write at all.
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func bump(mut b: Buf): Int = {\n\
               b.x = b.x + 1\n\
               b.x\n\
               }\n\
               func done(b: Buf): Int = b.x\n\
               func f(): Int = {\n\
               mut a = Buf(x=1)\n\
               let n = bump(mut a)\n\
               n * 10 + done(a)\n\
               }\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(22)");
}

#[test]
fn place_assign_read_modify_write_on_its_own_root_is_not_aliasing() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func f(): Int = {\n\
               mut a = Buf(x=1)\n\
               a.x = a.x + 1\n\
               a.x\n\
               }\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(2)");
}

#[test]
fn provides_linear_parses_alongside_error() {
    let mut s = FrogState::new();
    let (v, _) = s.eval("data E(msg: Str) provides Error, Linear, Abort { func abort(e: E): None = none }\n1").unwrap();
    assert_eq!(format!("{:?}", v), "Int(1)");
}

#[test]
fn an_unknown_trait_name_in_provides_is_still_rejected() {
    let mut s = FrogState::new();
    let err = s.eval("data X(n: Int) provides NotATrait\n1").unwrap_err().to_string();
    assert!(err.contains("Unknown trait 'NotATrait'"), "{err}");
}

#[test]
fn consuming_an_outer_value_before_a_same_named_inner_binding_still_counts() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func consume(b: Buf): Int = b.x\n\
               func f(c: Bool): Int = {\n\
               let x = Buf(x=1)\n\
               if c then { consume(x); let x = Buf(x=2); consume(x) } else 0\n\
               }\n\
               f(true)";
    let err = s.eval(src).unwrap_err().to_string();
    assert!(err.contains("consumed on only one branch"), "{err}");
}

const RES: &str = "data Res(id: Int) provides Linear, Abort { func abort(r: Res): None = none }\n\
                   func close(r: Res): Int = r.id\n\
                   data R is A(r: Res) | B(y: Int) provides Linear, Abort { func abort(u: R): None = none }\n";

fn eval(src: &str) -> Result<String, String> {
    let mut s = FrogState::new();
    s.eval(&format!("{RES}{src}")).map(|(v, _)| format!("{:?}", v)).map_err(|e| e.to_string())
}

#[test]
fn a_match_that_takes_a_linear_union_apart_consumes_it() {
    // The desugared match reads its subject once per arm; that used to be
    // reported as aliasing the subject's temporary.
    let v = eval("func f(u: R): Int = match u {\n\
                  is A(r) then close(r)\n\
                  is B(y) then y\n\
                  }\n\
                  f(A(r=Res(id=3))) + f(B(y=4))").unwrap();
    assert_eq!(v, "Int(7)");
}

#[test]
fn a_narrowed_linear_subject_must_be_consumed_in_every_arm() {
    let ok = eval("func done(u: R): Int = match u {\n\
                   is A(r) then close(r)\n\
                   is B(y) then y\n\
                   }\n\
                   func f(u: R): Int = match u {\n\
                   is A then done(u)\n\
                   is B then done(u) + 10\n\
                   }\n\
                   f(B(y=1))").unwrap();
    assert_eq!(ok, "Int(11)");
    let err = eval("func done(u: R): Int = match u {\n\
                   is A(r) then close(r)\n\
                   is B(y) then y\n\
                   }\n\
                    func f(u: R): Int = match u {\n\
                    is A then 0\n\
                    is B then done(u)\n\
                    }\n\
                    f(B(y=1))").unwrap_err();
    assert!(err.contains("'u' is Linear and is not consumed on every path"), "{err}");
}

#[test]
fn a_pattern_that_drops_a_linear_field_is_rejected() {
    let err = eval("func f(u: R): Int = match u {\n\
                    is A(_) then 0\n\
                    is B(y) then y\n\
                    }\n\
                    f(B(y=1))").unwrap_err();
    assert!(err.contains("this arm drops field 'r', which is Linear"), "{err}");
}

#[test]
fn a_bindless_arm_on_an_unnamed_linear_subject_is_rejected() {
    // Nothing holds the matched `Res` afterwards: no bind, and no name to narrow.
    let err = eval("func mk(): Res | None = Res(id=1)\n\
                    func f(): Int = match mk() {\n\
                    is Res then 1\n\
                    is None then 0\n\
                    }\n\
                    f()").unwrap_err();
    assert!(err.contains("this arm drops the matched value, which is Linear (Res)"), "{err}");
}

#[test]
fn a_guarded_arm_can_bind_a_linear_field() {
    let v = eval("func f(u: R): Int = match u {\n\
                  is A(r) and r.id > 5 then close(r)\n\
                  is A(r) then close(r) + 100\n\
                  is B(y) then y\n\
                  }\n\
                  f(A(r=Res(id=1))) + f(A(r=Res(id=9)))").unwrap();
    assert_eq!(v, "Int(110)");
}

// ── Linear is contained: a value holding a Linear value is Linear ──────────

const WRAP: &str = "data Keep(r: Res)\n\
                    data Two(a: Res, n: Int)\n\
                    data Box<T>(v: T)\n";

fn eval_wrap(src: &str) -> Result<String, String> { eval(&format!("{WRAP}{src}")) }

#[test]
fn a_struct_holding_a_linear_field_cannot_be_copied() {
    let err = eval_wrap("func f(): Int = {\n\
                         let k = Keep(r=Res(id=1))\n\
                         let k2 = k\n\
                         close(k.r) + close(k2.r)\n\
                         }\n\
                         f()").unwrap_err();
    assert!(err.contains("'k' is Linear"), "{err}");
}

#[test]
fn a_struct_holding_a_linear_field_must_be_consumed() {
    let err = eval_wrap("func f(): Int = {\n\
                         let k = Keep(r=Res(id=1))\n\
                         0\n\
                         }\n\
                         f()").unwrap_err();
    assert!(err.contains("'k' is Linear and is not consumed on every path"), "{err}");
    let ok = eval_wrap("func f(): Int = {\n\
                        let k = Keep(r=Res(id=4))\n\
                        close(k.r)\n\
                        }\n\
                        f()").unwrap();
    assert_eq!(ok, "Int(4)");
}

#[test]
fn a_generic_instantiated_with_a_linear_type_is_linear() {
    let err = eval_wrap("func f(): Int = {\n\
                         let b = Box(v=Res(id=1))\n\
                         let c = b\n\
                         close(b.v) + close(c.v)\n\
                         }\n\
                         f()").unwrap_err();
    assert!(err.contains("'b' is Linear"), "{err}");
}

#[test]
fn a_union_with_any_linear_member_is_linear() {
    // `Res | None` used to need *every* member to be Linear.
    let err = eval("func f(x: Res | None): Int = 0\nf(none)").unwrap_err();
    assert!(err.contains("'x' is Linear and is not consumed on every path"), "{err}");
}

#[test]
fn a_list_of_linear_values_cannot_be_copied() {
    let err = eval("func f(): Int = {\n\
                    let xs = [Res(id=1)]\n\
                    let ys = xs\n\
                    close(xs[0]) + close(ys[0])\n\
                    }\n\
                    f()").unwrap_err();
    assert!(err.contains("'xs' is Linear"), "{err}");
}

// ── Field reads: inspection, or taking the value apart at its last use ────

#[test]
fn reading_several_fields_of_a_linear_value_is_not_aliasing() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int, y: Int) provides Linear, Abort { func abort(b: Buf): None = none }\n\
               func f(b: Buf): Int = b.x + b.y\n\
               f(Buf(x=1, y=2))";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(3)");
}

#[test]
fn reading_a_field_while_the_value_is_live_inspects_it() {
    let v = eval_wrap("func f(t: Two): Int = t.n + close(t.a)\n\
                       f(Two(a=Res(id=1), n=2))").unwrap();
    assert_eq!(v, "Int(3)");
}

#[test]
fn taking_a_value_apart_may_not_drop_a_linear_field() {
    let err = eval_wrap("func f(t: Two): Int = t.n\n\
                         f(Two(a=Res(id=1), n=2))").unwrap_err();
    assert!(err.contains("reading 't.n' at the last use of 't' takes it apart, which drops 't.a'"), "{err}");
}

#[test]
fn pulling_a_linear_field_out_of_a_live_value_is_aliasing() {
    let err = eval_wrap("func f(k: Keep): Int = close(k.r) + close(k.r)\n\
                         f(Keep(r=Res(id=1)))").unwrap_err();
    assert!(err.contains("'k' is Linear"), "{err}");
}

#[test]
fn a_question_mark_names_a_linear_value_it_cannot_abort() {
    let err = eval_wrap("error Bad(msg: Str)\n\
                         func step(n: Int): Int | Bad = if n < 0 then Bad(msg=\"neg\") else n\n\
                         func f(n: Int): Int | Bad = {\n\
                         let k = Keep(r=Res(id=1))\n\
                         let v = step(n)?\n\
                         close(k.r) + v\n\
                         }\n\
                         f(1)").unwrap_err();
    assert!(err.contains("Keep has no `abort`, so a `?` here cannot abort it"), "{err}");
}
