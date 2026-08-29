//! `TRAITS.md` Part 7: the `Linear` marker trait — a built-in `Trait`
//! variant for now (see `frontend::linear`'s own doc comment for why, and
//! for the future-prelude-declaration refactor path). Covers both checks:
//! rule 1 (no `Copy`-classified — i.e. aliasing — read of a `Linear` value)
//! and rule 2 (a `Linear` binding consumed on one `if`/`match` branch must
//! be consumed on every branch or none), plus the two exemptions from rule
//! 1 that make `mut self`-style usage actually usable: a function's own
//! `mut` parameter, and a `PlaceAssign`'s own root.

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
    let src = "data Buf(x: Int) provides Linear\n\
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
    let src = "data Buf(x: Int) provides Linear\n\
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
    let src = "data Buf(x: Int) provides Linear\n\
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
    let src = "data Buf(x: Int) provides Linear\n\
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
fn a_linear_value_never_consumed_on_either_branch_is_accepted() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear\n\
               func f(): Int = {\n\
               let a = Buf(x=1)\n\
               if true then 1 else 0\n\
               }\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(1)");
}

#[test]
fn a_mut_self_style_parameter_can_read_its_own_field_repeatedly() {
    // The realistic `Sink`/`mut self` shape (`TRAITS.md` Part 7, `DATA.md`
    // Stage 4): a `mut` parameter's copy-out forces every read of it to be
    // `Copy`-classified by `liveness.rs`'s own convention, which would
    // otherwise make this pattern impossible to write at all.
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear\n\
               func bump(mut b: Buf): Int = {\n\
               b.x = b.x + 1\n\
               b.x\n\
               }\n\
               func f(): Int = {\n\
               mut a = Buf(x=1)\n\
               bump(mut a)\n\
               }\n\
               f()";
    let (v, _) = s.eval(src).unwrap();
    assert_eq!(format!("{:?}", v), "Int(2)");
}

#[test]
fn place_assign_read_modify_write_on_its_own_root_is_not_aliasing() {
    let mut s = FrogState::new();
    let src = "data Buf(x: Int) provides Linear\n\
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
    let (v, _) = s.eval("data E(msg: Str) provides Error, Linear\n1").unwrap();
    assert_eq!(format!("{:?}", v), "Int(1)");
}

#[test]
fn an_unknown_trait_name_in_provides_is_still_rejected() {
    let mut s = FrogState::new();
    let err = s.eval("data X(n: Int) provides NotATrait\n1").unwrap_err().to_string();
    assert!(err.contains("Unknown trait 'NotATrait'"), "{err}");
}
