//! The `implicit-copy` lint (`plans/NOGC.md` §1.4, item 5): `frog check
//! --copies` compiles a program to a native object without running it and
//! lists every deep copy codegen made because a consumer kept a value that
//! something else still owns. Copies are implicit by design; this is how they
//! are made visible.

use std::process::Command;

const FROG: &str = env!("CARGO_BIN_EXE_froglang-core");

fn copies(src: &str) -> (String, bool) {
    let out = Command::new(FROG).args(["check", "--copies", src]).output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

#[test]
fn a_function_that_only_reads_its_list_makes_no_copy() {
    let (out, ok) = copies(
        "func total(xs: List<Int>): Int = {\n mut t = 0\n for x in xs do t = t + x\n t\n}\n\
         func f(): Int = {\n let a = [1, 2, 3]\n total(a) + total(a)\n}\n\
         f()");
    assert!(ok, "{out}");
    assert!(out.contains("0 implicit copies"), "{out}");
}

#[test]
fn binding_a_live_list_to_a_second_name_is_reported_with_its_location() {
    let (out, ok) = copies(
        "func f(): Int = {\n let a = [1, 2, 3]\n let b = a\n a[0] + b[0]\n}\nf()");
    assert!(ok, "{out}");
    assert!(out.contains("warning: implicit-copy: deep copy of a `List<Int>` value"), "{out}");
    assert!(out.contains("<check>:3:"), "{out}");
    assert!(out.contains("1 implicit copy"), "{out}");
}

#[test]
fn moving_a_list_at_its_last_use_is_not_a_copy() {
    let (out, ok) = copies(
        "func f(): Int = {\n let a = [1, 2, 3]\n let b = a\n b[0]\n}\nf()");
    assert!(ok, "{out}");
    assert!(out.contains("0 implicit copies"), "{out}");
}

#[test]
fn pushing_a_list_that_is_still_in_use_is_a_copy() {
    let (out, ok) = copies(
        "func f(): Int = {\n let a = [1]\n mut s = [[0]]\n push(mut s, a)\n a[0] + s[1][0]\n}\nf()");
    assert!(ok, "{out}");
    assert!(out.contains("1 implicit copy"), "{out}");
}

#[test]
fn the_flag_may_follow_the_input_and_plain_check_is_unchanged() {
    let out = Command::new(FROG).args(["check", "1 + 1", "--copies"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("0 implicit copies"));
    let plain = Command::new(FROG).args(["check", "1 + 1"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&plain.stdout), ":: Int\n");
}

#[test]
fn a_type_error_is_still_reported_as_an_error() {
    let out = Command::new(FROG).args(["check", "--copies", "1 + \"a\""]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Type error"));
}
