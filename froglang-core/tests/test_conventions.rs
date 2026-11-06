// Parameter conventions (plans/NOGC.md §1.2, §6 step 1): which parameters a
// callee only reads (Borrow) and which let their value escape (Consume).

use froglang_core::frontend::conventions::{infer, Conventions, Convention};
use froglang_core::frontend::liveness::number_nodes;
use froglang_core::frontend::parser::Parser;
use froglang_core::frontend::typeck::TypeChecker;

fn conventions_of(src: &str) -> Conventions {
    let ast = Parser::parse(src).expect("parse error");
    let mut tc = TypeChecker::new();
    let mut typed = tc.check_and_lower(ast).expect("type error");
    tc.lower_function_values(&mut typed).expect("function value error");
    tc.desugar_notation(&mut typed).expect("notation error");
    number_nodes(&mut typed);
    infer(&typed)
}

#[test]
fn read_only_parameter_borrows() {
    let c = conventions_of("func f(s: Str): Int = len(s)\n");
    assert_eq!(c.convention("f", 0), Some(Convention::Borrow));
}

#[test]
fn returned_parameter_consumes() {
    let c = conventions_of("func f(s: Str): Str = s\n");
    assert_eq!(c.convention("f", 0), Some(Convention::Consume));
}

#[test]
fn bound_parameter_consumes() {
    let c = conventions_of("func f(s: Str): Int = { let t = s\n len(t) }\n");
    assert_eq!(c.convention("f", 0), Some(Convention::Consume));
}

#[test]
fn literal_element_consumes() {
    let c = conventions_of("func f(s: Str): List<Str> = [s]\n");
    assert_eq!(c.convention("f", 0), Some(Convention::Consume));
}

#[test]
fn consume_propagates_through_a_call() {
    // `g` returns its argument, so `f` hands its argument to a consuming position.
    let src = "func g(s: Str): Str = s\nfunc f(s: Str): Str = g(s)\n";
    let c = conventions_of(src);
    assert_eq!(c.convention("g", 0), Some(Convention::Consume));
    assert_eq!(c.convention("f", 0), Some(Convention::Consume));
}

#[test]
fn borrow_propagates_through_a_call() {
    // `g` only reads, so passing `s` on to it leaves `f` borrowing too.
    let src = "func g(s: Str): Int = len(s)\nfunc f(s: Str): Int = g(s)\n";
    let c = conventions_of(src);
    assert_eq!(c.convention("f", 0), Some(Convention::Borrow));
}

#[test]
fn recursion_reaches_a_fixpoint() {
    let src = "func r(s: Str, n: Int): Str = if n == 0 then s else r(s, n - 1)\n";
    let c = conventions_of(src);
    assert_eq!(c.convention("r", 0), Some(Convention::Consume));
}

#[test]
fn mut_parameter_is_not_classified() {
    let c = conventions_of("func f(mut xs: List<Int>): Int = len(xs)\n");
    let p = &c.functions.iter().find(|f| f.name == "f").unwrap().params[0];
    assert!(p.is_mut);
}

#[test]
fn borrowing_call_argument_is_recorded() {
    let src = "func g(s: Str): Int = len(s)\nlet x = \"hi\"\nprint(g(x))\n";
    let c = conventions_of(src);
    assert!(!c.borrowed_args.is_empty(), "the Var passed to a borrowing parameter is recorded");
}

#[test]
fn ambiguous_name_is_not_analysed() {
    let src = "func f(s: Str): Int = len(s)\nfunc f(s: Str): Str = s\n";
    let c = conventions_of(src);
    assert!(c.functions.is_empty());
    assert_eq!(c.ambiguous, vec!["f".to_string()]);
}
