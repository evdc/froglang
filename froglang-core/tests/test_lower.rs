use froglang_core::frontend::{
    parser::Parser,
    typed_ast::{TypedExpr, TypedExprKind},
    typeck::{Type, TypeChecker},
    tokens::Token,
};

fn lower(src: &str) -> TypedExpr {
    let ast = Parser::parse(src).expect("parse error");
    let mut tc = TypeChecker::new();
    let result = tc.check_and_lower(ast).expect("type error").item;
    // The parser always wraps input in a Block.  Unwrap single-statement
    // programs so the tests can assert directly on the expression kind.
    match result.kind {
        TypedExprKind::Block(mut stmts) if stmts.len() == 1 => stmts.remove(0).item,
        _ => result,
    }
}

// ── literals ─────────────────────────────────────────────────────────────────

#[test]
fn test_int_literal() {
    let r = lower("42");
    assert_eq!(r.ty, Type::Int);
    assert!(matches!(r.kind, TypedExprKind::IntLit(42)));
}

#[test]
fn test_float_literal() {
    let r = lower("3.14");
    assert_eq!(r.ty, Type::Float);
    assert!(matches!(r.kind, TypedExprKind::FloatLit(_)));
}

#[test]
fn test_bool_literal() {
    let r = lower("true");
    assert_eq!(r.ty, Type::Bool);
    assert!(matches!(r.kind, TypedExprKind::BoolLit(true)));
}

#[test]
fn test_str_literal() {
    let r = lower("\"hello\"");
    assert_eq!(r.ty, Type::Str);
    assert!(matches!(r.kind, TypedExprKind::StrLit(_)));
}

// ── binary expressions ────────────────────────────────────────────────────────

#[test]
fn test_binary_add() {
    let r = lower("1 + 2");
    assert_eq!(r.ty, Type::Int);
    match r.kind {
        TypedExprKind::Binary { op: Token::Plus, left, right } => {
            assert_eq!(left.item.ty,  Type::Int);
            assert_eq!(right.item.ty, Type::Int);
        },
        _ => panic!("expected Binary(+)"),
    }
}

#[test]
fn test_binary_float_add() {
    let r = lower("1.0 + 2.0");
    assert_eq!(r.ty, Type::Float);
}

#[test]
fn test_binary_comparison() {
    let r = lower("1 < 2");
    assert_eq!(r.ty, Type::Bool);
    assert!(matches!(r.kind, TypedExprKind::Binary { op: Token::Lt, .. }));
}

// ── unary expressions ─────────────────────────────────────────────────────────

#[test]
fn test_unary_minus() {
    let r = lower("-1");
    assert_eq!(r.ty, Type::Int);
    assert!(matches!(r.kind, TypedExprKind::Unary { op: Token::Minus, .. }));
}

#[test]
fn test_unary_not() {
    let r = lower("not true");
    assert_eq!(r.ty, Type::Bool);
    assert!(matches!(r.kind, TypedExprKind::Unary { op: Token::Not, .. }));
}

// ── assignments ───────────────────────────────────────────────────────────────

#[test]
fn test_assign_int() {
    let r = lower("let x = 5");
    assert_eq!(r.ty, Type::Int);
    match r.kind {
        TypedExprKind::Assign { name, value } => {
            assert_eq!(name, "x");
            assert_eq!(value.item.ty, Type::Int);
        },
        _ => panic!("expected Assign"),
    }
}

#[test]
fn test_assign_with_annotation() {
    let r = lower("let x : Int = 5");
    assert_eq!(r.ty, Type::Int);
    assert!(matches!(r.kind, TypedExprKind::Assign { .. }));
}

// ── annotated expressions ─────────────────────────────────────────────────────

#[test]
fn test_annotated_absorbed() {
    // The Annotated wrapper disappears; the kind comes from the inner expression.
    let r = lower("1 : Int");
    assert_eq!(r.ty, Type::Int);
    assert!(matches!(r.kind, TypedExprKind::IntLit(1)));
}

// ── functions ─────────────────────────────────────────────────────────────────

#[test]
fn test_function_literal() {
    // x + 1 constrains x to Int, so the lambda is Int -> Int.
    let r = lower("x -> x + 1");
    match &r.ty {
        Type::Function { params, result } => {
            assert_eq!(params, &[Type::Int]);
            assert_eq!(**result, Type::Int);
        },
        other => panic!("expected Function type, got {:?}", other),
    }
    match &r.kind {
        TypedExprKind::Function { params, return_type, .. } => {
            assert_eq!(params, &[("x".to_string(), Type::Int)]);
            assert_eq!(*return_type, Type::Int);
        },
        _ => panic!("expected Function kind"),
    }
}

#[test]
fn test_function_identity_unconstrained() {
    // Pure identity: parameter type stays as a TypeVar (no constraint to resolve it).
    let r = lower("x -> x");
    assert!(matches!(r.ty, Type::Function { .. }));
}

// ── call sites resolve TypeVars ───────────────────────────────────────────────

#[test]
fn test_call_resolves_typevar() {
    // After f(2), the call result must be Int.
    let r = lower("let f = n -> n + 1\nf(2)");
    match &r.kind {
        TypedExprKind::Block(stmts) => {
            let last = stmts.last().expect("block should be non-empty");
            assert_eq!(last.item.ty, Type::Int);
            assert!(matches!(last.item.kind, TypedExprKind::Call { .. }));
        },
        _ => panic!("expected Block"),
    }
}

#[test]
fn test_block_last_type() {
    let r = lower("let x = 1\nlet y = 2\nx + y");
    assert_eq!(r.ty, Type::Int);
    match &r.kind {
        TypedExprKind::Block(stmts) => {
            assert_eq!(stmts.len(), 3);
            assert_eq!(stmts[2].item.ty, Type::Int);
        },
        _ => panic!("expected Block"),
    }
}

// ── conditional ───────────────────────────────────────────────────────────────

#[test]
fn test_conditional() {
    let r = lower("if true then 1 else 2");
    assert_eq!(r.ty, Type::Int);
    assert!(matches!(r.kind, TypedExprKind::Conditional { .. }));
}

// ── list ──────────────────────────────────────────────────────────────────────

#[test]
fn test_list_literal() {
    let r = lower("[1, 2, 3]");
    assert_eq!(r.ty, Type::List(Box::new(Type::Int)));
    assert!(matches!(r.kind, TypedExprKind::List(_)));
}

// ── type errors still propagate ───────────────────────────────────────────────

#[test]
fn test_type_error_propagates() {
    let ast = Parser::parse("1 + true").expect("parse error");
    let mut tc = TypeChecker::new();
    assert!(tc.check_and_lower(ast).is_err());
}
