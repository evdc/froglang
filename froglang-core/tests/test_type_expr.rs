// Tests for the type grammar (`frontend::type_expr` + `Grammar::type_expr`).
//
// Before this existed, `func` params, `func` return types, and `data` fields
// each parsed their annotation with `Parser::identifier()` — exactly one
// token. Everything below that isn't a bare name was a parse error at those
// sites, so most of these are new capability rather than regression cover.

use froglang_core::frontend::parser::Parser;
use froglang_core::frontend::type_expr::TypeExpr;
use froglang_core::frontend::typeck::{Type, TypeChecker};

/// Parse and type-check a program, returning the type of its final expression.
fn infer_src(src: &str) -> Result<Type, String> {
    let ast = Parser::parse(src).map_err(|e| format!("{:?}", e))?;
    let mut tc = TypeChecker::new();
    tc.infer(&ast).map_err(|e| format!("{:?}", e))
}

fn type_error(src: &str) -> String {
    let ast = Parser::parse(src).expect("parse error");
    let mut tc = TypeChecker::new();
    let err = tc.check_and_lower(ast).expect_err("expected a type error");
    format!("{:?}", err)
}

/// The `TypeExpr` an annotation parses to, via `let x: <ty> = 0`.
fn parse_ty(src: &str) -> TypeExpr {
    use froglang_core::frontend::expression::Expression;
    let ast = Parser::parse(&format!("let x: {} = 0", src)).expect("parse error");
    let stmts = match ast.item {
        Expression::Block(s) => s,
        other => vec![froglang_core::frontend::tokens::Spanned::from(other, ast.span)],
    };
    match &stmts[0].item {
        Expression::Assign(a) => a.typ.as_ref().expect("annotation present").item.clone(),
        other => panic!("expected an assignment, got {:?}", other),
    }
}

// ── shapes the grammar must produce ──────────────────────────────────────────

#[test]
fn test_parse_name() {
    assert_eq!(parse_ty("Int"), TypeExpr::Name("Int".to_string()));
}

#[test]
fn test_parse_union() {
    match parse_ty("Int | Str | Bool") {
        TypeExpr::Union(members) => assert_eq!(members.len(), 3),
        other => panic!("expected a union, got {}", other),
    }
}

#[test]
fn test_parse_optional_is_postfix_on_the_atom_not_the_union() {
    // `Int | Str?` is `Int | (Str?)`, not `(Int | Str)?` — `?` binds tighter
    // than `|`. Both happen to normalize to the same set here, so assert on
    // the parse rather than on the resolved type.
    match parse_ty("Int | Str?") {
        TypeExpr::Union(members) => {
            assert_eq!(members.len(), 2);
            assert!(matches!(members[1].item, TypeExpr::Optional(_)));
        }
        other => panic!("expected a union, got {}", other),
    }
}

#[test]
fn test_parse_apply() {
    match parse_ty("List(Int)") {
        TypeExpr::Apply(name, args) => {
            assert_eq!(name, "List");
            assert_eq!(args.len(), 1);
        }
        other => panic!("expected an application, got {}", other),
    }
}

#[test]
fn test_parse_dotted_name() {
    assert_eq!(parse_ty("utils.Point"), TypeExpr::Name("utils.Point".to_string()));
}

#[test]
fn test_parse_multiline_union_after_pipe() {
    // A newline is allowed after `|`, mirroring `data ... is ...`.
    let ast = Parser::parse("let x: Int |\n Str |\n Bool = 0");
    assert!(ast.is_ok(), "{:?}", ast.err());
}

// ── resolution ───────────────────────────────────────────────────────────────

#[test]
fn test_optional_resolves_to_union_with_none() {
    assert_eq!(infer_src("let x: Int? = 5").unwrap(), Type::Union(vec![Type::Int, Type::None]).normalize());
}

// `Int | Bool` rather than `Int | Str` here and below: a `Str` member of a
// union that needs boxing is a lowering limitation `lower_widen` rejects
// outright (`"Str is not yet supported as a member of a union that needs
// boxing"`), so `let x: Int | Str = "a"` is not a program that compiles.
// These tests are about the type *grammar* reaching these annotation sites,
// so they use a member pair the back end can actually represent. Before the
// front end became a single pass they read `Int | Str` and passed, because
// `infer` stopped short of lowering and never reached that check — the
// programs themselves were rejected by `run` all the same.
#[test]
fn test_union_annotation_accepts_either_member() {
    assert_eq!(infer_src("let x: Int | Bool = 5").unwrap(), Type::Union(vec![Type::Int, Type::Bool]).normalize());
    assert_eq!(infer_src("let x: Int | Bool = true").unwrap(), Type::Union(vec![Type::Int, Type::Bool]).normalize());
}

#[test]
fn test_union_annotation_rejects_a_non_member() {
    assert!(infer_src("let x: Int | Str = true").is_err());
}

#[test]
fn test_list_annotation() {
    assert_eq!(infer_src("let xs: List(Int) = [1, 2, 3]").unwrap(), Type::list(Type::Int));
    assert!(infer_src("let xs: List(Str) = [1, 2, 3]").is_err());
}

#[test]
fn test_nested_list_annotation() {
    assert_eq!(
        infer_src("let xs: List(List(Int)) = [[1], [2]]").unwrap(),
        Type::list(Type::list(Type::Int)),
    );
}

#[test]
fn test_none_is_a_nameable_type() {
    assert_eq!(infer_src("let x: None = for i in [1] do i").unwrap(), Type::None);
}

// ── the sites that could not take a compound annotation before ───────────────

#[test]
fn test_func_return_type_may_be_a_union() {
    // See the note above `test_union_annotation_accepts_either_member` on
    // why the member pair is `Int | Bool` rather than `Int | Str`.
    let src = "func f(n: Int): Int | Bool = if n > 0 then 1 else true\nf(1)";
    assert_eq!(
        infer_src(src).unwrap(),
        Type::Union(vec![Type::Int, Type::Bool]).normalize(),
    );
}

#[test]
fn test_func_param_type_may_be_a_list() {
    assert_eq!(infer_src("func total(xs: List(Int)): Int = xs[0]\ntotal([7])").unwrap(), Type::Int);
}

#[test]
fn test_data_field_type_may_be_a_list() {
    let src = "data Bag(items: List(Int))\nlet b = Bag(items=[1, 2])\nb.items[1]";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

#[test]
fn test_data_field_type_may_be_a_union() {
    let src = "data Cell(v: Int | Str)\nlet c = Cell(v=1)\nc.v";
    assert_eq!(
        infer_src(src).unwrap(),
        Type::Union(vec![Type::Int, Type::Str]).normalize(),
    );
}

#[test]
fn test_variant_field_type_may_be_compound() {
    let src = "data Shape is Poly(pts: List(Int)) | Dot\nlet s = Poly(pts=[1, 2])\nif s is Dot then 0 else 1";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

// ── errors ───────────────────────────────────────────────────────────────────

#[test]
fn test_unknown_type_name_is_an_error() {
    assert!(type_error("let x: Nope = 1").contains("Unknown type 'Nope'"));
}

#[test]
fn test_a_local_cannot_shadow_a_type_name() {
    // The old `Expression`-sniffing resolver fell back to the *value*
    // environment, so a local named `Int` would have been picked up as a type.
    assert!(type_error("let Foo = 1\nlet x: Foo = 2").contains("Unknown type 'Foo'"));
}

#[test]
fn test_list_arity_is_checked() {
    assert!(type_error("let x: List(Int, Str) = [1]").contains("exactly 1 type argument"));
}

#[test]
fn test_non_generic_type_rejects_arguments() {
    assert!(type_error("let x: Int(Str) = 1").contains("does not take type arguments"));
}

#[test]
fn test_bare_arrow_still_needs_parens() {
    // Preserved from before the type grammar existed.
    assert!(Parser::parse("let f: Int -> Int = x -> x").is_err());
    assert!(Parser::parse("let f: (Int -> Int) = x -> x").is_ok());
}

#[test]
fn test_tuple_type_is_rejected_with_an_explanation() {
    let err = format!("{:?}", Parser::parse("let x: (Int, Str) = 1").unwrap_err());
    assert!(err.contains("tuple types are not supported"), "{}", err);
}
