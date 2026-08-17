use std::{fmt::Debug, vec};
use froglang_core::frontend::{expression::{Expression, Parameter}, tokens::{Span, Spanned, Token}, typeck::{Trait, Type, TypeChecker}};
use froglang_core::frontend::parser::Parser;

// Helper function to create a spanned item
fn spanned<T: Debug>(item: T) -> Spanned<T> {
    Spanned {
        item,
        span: Span::new((0, 0), (0, 1))
    }
}

// Helper function to create lambda exprs
fn lambda(params: Vec<&str>, result: Expression) -> Spanned<Expression> {
    let params = params.iter().map(|p| Parameter { name: p.to_string(), ty: None }).collect();
    spanned(Expression::function(params, spanned(result)))
}

// Helper to create identifier expressions
fn ident(name: &str) -> Spanned<Expression> {
    spanned(Expression::literal(Token::Identifier(name.to_string())))
}

// Helper to create int literals
fn int(n: i64) -> Spanned<Expression> {
    spanned(Expression::literal(Token::Int(n)))
}

// Helper to create float literals
fn float(f: f64) -> Spanned<Expression> {
    spanned(Expression::literal(Token::Float(f)))
}

// Helper to create string literals
fn string(s: &str) -> Spanned<Expression> {
    spanned(Expression::literal(Token::String(s.to_string())))
}

fn bool_lit(b: bool) -> Spanned<Expression> {
    spanned(Expression::literal(if b { Token::True } else { Token::False }))
}

#[test]
fn test_basic() {
    let mut t = TypeChecker::new();
    let expr = spanned(Expression::literal(Token::Int(5)));
    let res = t.infer(&expr).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_lambda() {
    let mut t = TypeChecker::new();
    let expr = lambda(vec!["x"], Expression::literal(Token::Identifier("x".to_string())));
    let res = t.infer(&expr).unwrap();
    assert!(matches!(res, Type::Function { params: _, result: _ }));
}

#[test]
fn test_apply() {
    let mut t = TypeChecker::new();
    // (x -> x)(5) :: Int
    let lambda = lambda(vec!["x"], Expression::literal(Token::Identifier("x".to_string())));
    let arg = spanned(Expression::literal(Token::Int(5)));
    let call = spanned(Expression::call(lambda, vec![arg]));
    let res = t.infer(&call).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_all_literal_types() {
    let mut t = TypeChecker::new();

    assert_eq!(t.infer(&int(42)).unwrap(), Type::Int);
    assert_eq!(t.infer(&float(3.14)).unwrap(), Type::Float);
    assert_eq!(t.infer(&string("hello")).unwrap(), Type::Str);
    assert_eq!(t.infer(&spanned(Expression::literal(Token::True))).unwrap(), Type::Bool);
    assert_eq!(t.infer(&spanned(Expression::literal(Token::False))).unwrap(), Type::Bool);
}

#[test]
fn test_unbound_variable_error() {
    let mut t = TypeChecker::new();
    let expr = ident("undefined_var");
    let res = t.infer(&expr);
    assert!(res.is_err());
    assert!(res.unwrap_err().item.msg.contains("Unbound variable"));
}

#[test]
fn test_all_arithmetic_ops() {
    let mut t = TypeChecker::new();

    let ops = vec![Token::Plus, Token::Minus, Token::Star];
    for op in ops {
        let expr = spanned(Expression::binary(op, int(1), int(2)));
        assert_eq!(t.infer(&expr).unwrap(), Type::Int);
    }
}

#[test]
fn test_float_arithmetic() {
    let mut t = TypeChecker::new();
    // Float operands return Float for all arithmetic operators.
    let ops = vec![Token::Plus, Token::Minus, Token::Star, Token::Slash];
    for op in ops {
        let expr = spanned(Expression::binary(op, float(1.0), float(2.0)));
        assert_eq!(t.infer(&expr).unwrap(), Type::Float);
    }
}

#[test]
fn test_float_division() {
    let mut t = TypeChecker::new();
    let expr = spanned(Expression::binary(Token::Slash, float(4.0), float(2.0)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Float);
}

#[test]
fn test_int_division() {
    let mut t = TypeChecker::new();
    // With Num polymorphism, integer division is now valid and returns Int.
    let expr = spanned(Expression::binary(Token::Slash, int(4), int(2)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Int);
}

#[test]
fn test_division_type_mismatch() {
    let mut t = TypeChecker::new();
    // Str does not implement Num, so string division is a type error.
    let expr = spanned(Expression::binary(Token::Slash, string("a"), string("b")));
    let res = t.infer(&expr);
    assert!(res.is_err());
}

#[test]
fn test_mixed_arithmetic_coercion() {
    let mut t = TypeChecker::new();
    // Mixing Int and Float widens to Float.
    let expr = spanned(Expression::binary(Token::Plus, int(1), float(2.0)));
    let res = t.infer(&expr);
    assert_eq!(res.unwrap(), Type::Float);
}

#[test]
fn test_str_arithmetic_error() {
    let mut t = TypeChecker::new();
    // Str + Str is now valid (string concatenation) and returns Str.
    let expr = spanned(Expression::binary(Token::Plus, string("a"), string("b")));
    let res = t.infer(&expr);
    assert_eq!(res.unwrap(), Type::Str);
    // Other arithmetic on Str is still an error.
    let sub = spanned(Expression::binary(Token::Minus, string("a"), string("b")));
    assert!(t.infer(&sub).is_err());
}

#[test]
fn test_comparison_ops_int() {
    let mut t = TypeChecker::new();

    let ops = vec![Token::EqEq, Token::NotEq, Token::Gt,
                   Token::GtEq, Token::Lt, Token::LtEq];

    for op in ops {
        let expr = spanned(Expression::binary(op, int(1), int(2)));
        assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
    }
}

#[test]
fn test_comparison_ops_float() {
    let mut t = TypeChecker::new();
    // Float implements both Eq and Ord.
    let expr = spanned(Expression::binary(Token::Lt, float(1.0), float(2.0)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
    let expr = spanned(Expression::binary(Token::EqEq, float(1.0), float(2.0)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
}

#[test]
fn test_comparison_ops_str() {
    let mut t = TypeChecker::new();
    // Str implements Eq and Ord.
    let expr = spanned(Expression::binary(Token::Lt, string("a"), string("b")));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
    let expr = spanned(Expression::binary(Token::EqEq, string("a"), string("b")));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
}

#[test]
fn test_comparison_ops() {
    let mut t = TypeChecker::new();

    let ops = vec![Token::EqEq, Token::NotEq, Token::Gt,
                   Token::GtEq, Token::Lt, Token::LtEq];

    for op in ops {
        let expr = spanned(Expression::binary(op, int(1), int(2)));
        assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
    }
}

#[test]
fn test_bool_equality() {
    let mut t = TypeChecker::new();
    // Bool implements Eq.
    let expr = spanned(Expression::binary(Token::EqEq, bool_lit(true), bool_lit(false)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
}

#[test]
fn test_unary_ops() {
    let mut t = TypeChecker::new();

    // Unary minus on Int
    let expr = spanned(Expression::unary(Token::Minus, int(5)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Int);

    // Unary minus on Float
    let expr = spanned(Expression::unary(Token::Minus, float(1.5)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Float);

    // Not on bool
    let expr = spanned(Expression::unary(Token::Not, bool_lit(true)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
}

#[test]
fn test_unary_type_mismatch() {
    let mut t = TypeChecker::new();

    // Not on non-bool should fail
    let expr = spanned(Expression::unary(Token::Not, int(5)));
    let res = t.infer(&expr);
    assert!(res.is_err());
}

#[test]
fn test_nested_arithmetic() {
    let mut t = TypeChecker::new();

    // (1 + 2) * 3
    let inner = spanned(Expression::binary(Token::Plus, int(1), int(2)));
    let outer = spanned(Expression::binary(Token::Star, inner, int(3)));
    assert_eq!(t.infer(&outer).unwrap(), Type::Int);
}

#[test]
fn test_conditional_basic() {
    let mut t = TypeChecker::new();

    let cond = spanned(Expression::conditional(
        bool_lit(true),
        int(1),
        Some(int(2))
    ));
    assert_eq!(t.infer(&cond).unwrap(), Type::Int);
}

#[test]
fn test_conditional_no_else() {
    let mut t = TypeChecker::new();

    // No else branch: type is Int | None
    let cond = spanned(Expression::conditional(
        bool_lit(true),
        int(1),
        None
    ));
    assert_eq!(t.infer(&cond).unwrap(), Type::Union(vec![Type::Int, Type::None]));
}

#[test]
fn test_conditional_non_bool_condition() {
    let mut t = TypeChecker::new();

    let cond = spanned(Expression::conditional(
        int(1), // Should be bool
        int(2),
        Some(int(3))
    ));
    let res = t.infer(&cond);
    assert!(res.is_err());
    assert!(res.unwrap_err().item.msg.contains("Incorrect type"));
}

#[test]
fn test_conditional_branch_mismatch_produces_union() {
    let mut t = TypeChecker::new();

    // Mismatched branches produce a union type instead of an error.
    let cond = spanned(Expression::conditional(
        bool_lit(true),
        int(1),           // Int
        Some(string("hello"))  // Str
    ));
    let res = t.infer(&cond).unwrap();
    assert_eq!(res, Type::Union(vec![Type::Int, Type::Str]));
}

#[test]
fn test_lambda_with_multiple_params() {
    let mut t = TypeChecker::new();

    // (x, y) -> x
    let expr = lambda(vec!["x", "y"], Expression::literal(Token::Identifier("x".to_string())));
    let res = t.infer(&expr).unwrap();
    println!("{}", res);

    if let Type::Function { params, .. } = res {
        assert_eq!(params.len(), 2);
        assert!(matches!(params[0], Type::TypeVar { .. }));
        assert!(matches!(params[1], Type::TypeVar { .. }));
    } else {
        panic!("Expected function type");
    }
}

#[test]
fn test_lambda_returns_lambda() {
    let mut t = TypeChecker::new();

    // x -> (y -> x)
    let inner_lambda = lambda(vec!["y"], Expression::literal(Token::Identifier("x".to_string())));
    let outer_lambda = lambda(vec!["x"], inner_lambda.item);

    let res = t.infer(&outer_lambda).unwrap();

    if let Type::Function { result, .. } = res {
        assert!(matches!(*result, Type::Function { .. }));
    } else {
        panic!("Expected function type");
    }
}

#[test]
fn test_call_non_function() {
    let mut t = TypeChecker::new();

    let call = spanned(Expression::call(int(5), vec![int(1)]));
    let res = t.infer(&call);
    assert!(res.is_err());
    assert!(res.unwrap_err().item.msg.contains("Not callable"));
}

#[test]
fn test_function_argument_type_mismatch() {
    let mut t = TypeChecker::new();

    // Create lambda (x -> x + 1), then call with string
    let body = spanned(Expression::binary(
        Token::Plus,
        ident("x"),
        int(1)
    ));
    let lambda = lambda(vec!["x"], body.item);
    let call = spanned(Expression::call(lambda, vec![string("hello")]));

    let res = t.infer(&call);
    assert!(res.is_err());
}

#[test]
fn test_recursive_unification() {
    let mut t = TypeChecker::new();

    // Let f = (x -> x), then f(5) should infer x = Int
    let lambda = lambda(vec!["x"], Expression::literal(Token::Identifier("x".to_string())));
    let call = spanned(Expression::call(lambda, vec![int(5)]));

    let res = t.infer(&call).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_complex_function_composition() {
    let mut t = TypeChecker::new();

    // Test: ((x -> y -> x)(5))(true) should be Int
    let inner_lambda = lambda(vec!["y"], Expression::literal(Token::Identifier("x".to_string())));
    let outer_lambda = lambda(vec!["x"], inner_lambda.item);

    let first_call = spanned(Expression::call(outer_lambda, vec![int(5)]));
    let second_call = spanned(Expression::call(first_call, vec![bool_lit(true)]));

    let res = t.infer(&second_call).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_variable_shadowing_in_lambda() {
    let mut t = TypeChecker::new();

    let lambda = lambda(vec!["x"], Expression::literal(Token::Identifier("x".to_string())));
    let res = t.infer(&lambda).unwrap();
    assert!(matches!(res, Type::Function { .. }));
}

#[test]
fn test_type_variable_display() {
    let tv = Type::TypeVar { name: "test".to_string(), bounds: vec![] };
    assert_eq!(format!("{}", tv), "~test");

    let tv_bounded = Type::TypeVar { name: "t".to_string(), bounds: vec![Trait::Num] };
    assert_eq!(format!("{}", tv_bounded), "~t:Num");

    let func = Type::Function {
        params: vec![Type::Int, Type::TypeVar { name: "a".to_string(), bounds: vec![] }],
        result: Box::new(Type::Bool)
    };
    let display_str = format!("{}", func);
    assert!(display_str.contains("Int"));
    assert!(display_str.contains("~a"));
    assert!(display_str.contains("Bool"));
}

#[test]
fn test_union_display() {
    let u = Type::Union(vec![Type::Int, Type::None]);
    assert_eq!(format!("{}", u), "Int | None");
}

#[test]
fn test_deeply_nested_functions() {
    let mut t = TypeChecker::new();

    // ((x -> x)(y -> y))(5)
    let identity1 = lambda(vec!["x"], Expression::literal(Token::Identifier("x".to_string())));
    let identity2 = lambda(vec!["y"], Expression::literal(Token::Identifier("y".to_string())));

    let call1 = spanned(Expression::call(identity1, vec![identity2]));
    let call2 = spanned(Expression::call(call1, vec![int(5)]));

    let res = t.infer(&call2).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_assignment() {
    let mut t = TypeChecker::new();

    // x = 1; x + 2
    let assign = spanned(Expression::assign(ident("x"), None, int(1)));
    let add = spanned(Expression::binary(Token::Plus, ident("x"), int(2)));

    // Should be an error if we infer `x + 2` without seeing the variable binding first
    let _err = t.infer(&add).unwrap_err();
    // And a success if we first check `x = 1` and use it to update context
    t.infer(&assign).unwrap();
    let res = t.infer(&add).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_check() {
    let mut t = TypeChecker::new();

    // let add(x: Int, y: Int): Int = x + y
    let func = lambda(vec!["x", "y"], Expression::binary(Token::Plus, ident("x"), ident("y")));
    let expected_ty = Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Int) };
    t.check(&func, &expected_ty).unwrap();
}

#[test]
fn test_logical_ops() {
    let mut t = TypeChecker::new();
    // and: Bool, Bool -> Bool
    let expr = spanned(Expression::binary(Token::And, bool_lit(true), bool_lit(false)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
    // or: Bool, Bool -> Bool
    let expr = spanned(Expression::binary(Token::Or, bool_lit(false), bool_lit(true)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Bool);
}

#[test]
fn test_block_type_inference() {
    let mut t = TypeChecker::new();
    // { x = 1; x + 2 } :: Int (block type = type of last expression)
    let block = spanned(Expression::Block(vec![
        spanned(Expression::assign(ident("x"), None, int(1))),
        spanned(Expression::binary(Token::Plus, ident("x"), int(2))),
    ]));
    assert_eq!(t.infer(&block).unwrap(), Type::Int);
}

#[test]
fn test_list_type_inference() {
    let mut t = TypeChecker::new();
    // [1, 2, 3] :: List(Int)
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    assert_eq!(t.infer(&list).unwrap(), Type::List(Box::new(Type::Int)));
}

#[test]
fn test_assignment_with_type_annotation() {
    let mut t = TypeChecker::new();
    // let x: Int = 5
    let ann_type = spanned(Expression::literal(Token::Identifier("Int".to_string())));
    let assign = spanned(Expression::assign(ident("x"), Some(ann_type), int(5)));
    assert_eq!(t.infer(&assign).unwrap(), Type::Int);
    // Type mismatch: let x: Bool = 5 should fail
    let mut t2 = TypeChecker::new();
    let ann_type2 = spanned(Expression::literal(Token::Identifier("Bool".to_string())));
    let assign2 = spanned(Expression::assign(ident("x"), Some(ann_type2), int(5)));
    assert!(t2.infer(&assign2).is_err());
}

// --- Phase B: trait-bounded operator polymorphism ---

#[test]
fn test_num_polymorphism_int() {
    let mut t = TypeChecker::new();
    // 1 + 2 :: Int  (Int implements Num)
    assert_eq!(t.infer(&spanned(Expression::binary(Token::Plus, int(1), int(2)))).unwrap(), Type::Int);
}

#[test]
fn test_num_polymorphism_float() {
    let mut t = TypeChecker::new();
    // 1.0 + 2.0 :: Float  (Float implements Num)
    assert_eq!(t.infer(&spanned(Expression::binary(Token::Plus, float(1.0), float(2.0)))).unwrap(), Type::Float);
}

#[test]
fn test_mixed_num_coercion() {
    let mut t = TypeChecker::new();
    // 1 + 2.0  →  Float (Int widens to Float)
    let res = t.infer(&spanned(Expression::binary(Token::Plus, int(1), float(2.0))));
    assert_eq!(res.unwrap(), Type::Float);
}

#[test]
fn test_str_not_num() {
    let mut t = TypeChecker::new();
    // "a" + "b"  →  Str (string concatenation is now valid)
    let res = t.infer(&spanned(Expression::binary(Token::Plus, string("a"), string("b"))));
    assert_eq!(res.unwrap(), Type::Str);
    // "a" - "b"  →  still an error
    let sub = t.infer(&spanned(Expression::binary(Token::Minus, string("a"), string("b"))));
    assert!(sub.is_err());
}

#[test]
fn test_eq_polymorphism() {
    let mut t = TypeChecker::new();
    // 1 == 1 :: Bool
    assert_eq!(t.infer(&spanned(Expression::binary(Token::EqEq, int(1), int(2)))).unwrap(), Type::Bool);
    // "a" == "b" :: Bool  (Str implements Eq)
    assert_eq!(t.infer(&spanned(Expression::binary(Token::EqEq, string("a"), string("b")))).unwrap(), Type::Bool);
    // true == false :: Bool  (Bool implements Eq)
    assert_eq!(t.infer(&spanned(Expression::binary(Token::EqEq, bool_lit(true), bool_lit(false)))).unwrap(), Type::Bool);
}

#[test]
fn test_ord_polymorphism_str() {
    let mut t = TypeChecker::new();
    // "a" < "b" :: Bool  (Str implements Ord)
    assert_eq!(t.infer(&spanned(Expression::binary(Token::Lt, string("a"), string("b")))).unwrap(), Type::Bool);
}

#[test]
fn test_bool_not_ord() {
    let mut t = TypeChecker::new();
    // true < false  →  type error: Bool does not implement Ord
    let res = t.infer(&spanned(Expression::binary(Token::Lt, bool_lit(true), bool_lit(false))));
    assert!(res.is_err());
}

#[test]
fn test_eq_type_mismatch() {
    let mut t = TypeChecker::new();
    // "a" == 1  →  type error: can't unify Str and Int
    let res = t.infer(&spanned(Expression::binary(Token::EqEq, string("a"), int(1))));
    assert!(res.is_err());
}

#[test]
fn test_infer_num_lambda() {
    let mut t = TypeChecker::new();
    // x -> x + x should infer as: (Num t) => t -> t
    let lambda = lambda(vec!["x"], Expression::binary(
        Token::Plus,
        Expression::literal(Token::Identifier("x".to_string())).at((0, 0)..(0,0)),
        Expression::literal(Token::Identifier("x".to_string())).at((0, 0)..(0,0)),
    ));

    let res = t.infer(&lambda).unwrap();
    // We check structure rather than specific TypeVar names:
    // - param and result should be the same TypeVar
    // - that TypeVar should carry a Num bound
    match res {
        Type::Function { params, result } => {
            assert_eq!(params.len(), 1);
            let param = &params[0];
            assert_eq!(param, &*result, "param and result should be the same TypeVar");
            match param {
                Type::TypeVar { bounds, .. } =>
                    assert!(bounds.contains(&Trait::Num), "param should have Num bound, got {:?}", bounds),
                _ => panic!("expected TypeVar param, got {:?}", param),
            }
        }
        other => panic!("expected Function type, got {:?}", other),
    }
}

// --- Phase A: union types ---

#[test]
fn test_union_from_conditional() {
    let mut t = TypeChecker::new();
    // if true then 1 else "hello"  →  Int | Str
    let cond = spanned(Expression::conditional(bool_lit(true), int(1), Some(string("hello"))));
    assert_eq!(t.infer(&cond).unwrap(), Type::Union(vec![Type::Int, Type::Str]));
}

#[test]
fn test_is_subtype_via_check() {
    let mut t = TypeChecker::new();
    // An Int value satisfies an expected type of Int | Str.
    let expected = Type::Union(vec![Type::Int, Type::Str]);
    let res = t.check(&int(1), &expected).unwrap();
    assert_eq!(res, Type::Int);
}

// --- Union normalization ---

#[test]
fn test_union_dedup() {
    // Int | Str | Int  →  Int | Str
    let u = Type::Union(vec![Type::Int, Type::Str, Type::Int]).normalize();
    assert_eq!(u, Type::Union(vec![Type::Int, Type::Str]));
}

#[test]
fn test_union_order_independent() {
    // Int | Str  ==  Str | Int  (after normalization both are canonically sorted)
    let u1 = Type::Union(vec![Type::Int, Type::Str]).normalize();
    let u2 = Type::Union(vec![Type::Str, Type::Int]).normalize();
    assert_eq!(u1, u2);
}

#[test]
fn test_union_flatten_nested() {
    // Int | (Str | Bool)  →  Bool | Int | Str  (flattened + sorted)
    let u = Type::Union(vec![
        Type::Int,
        Type::Union(vec![Type::Str, Type::Bool]),
    ]).normalize();
    assert_eq!(u, Type::Union(vec![Type::Bool, Type::Int, Type::Str]));
}

#[test]
fn test_union_singleton_collapses() {
    // Union of one type collapses to that type.
    assert_eq!(Type::Union(vec![Type::Int]).normalize(), Type::Int);
}

#[test]
fn test_union_dedup_to_scalar() {
    // Int | Int  →  Int  (collapses after dedup)
    assert_eq!(Type::Union(vec![Type::Int, Type::Int]).normalize(), Type::Int);
}

#[test]
fn test_union_triple_dedup() {
    // Bool | Int | Str | Bool | Int  →  Bool | Int | Str
    let u = Type::Union(vec![
        Type::Bool, Type::Int, Type::Str, Type::Bool, Type::Int,
    ]).normalize();
    assert_eq!(u, Type::Union(vec![Type::Bool, Type::Int, Type::Str]));
}

// --- Union subtyping ---

#[test]
fn test_union_subtype_of_wider_union() {
    // Int | Str  <:  Int | Str | Bool
    let tc = TypeChecker::new();
    let sub = Type::Union(vec![Type::Int, Type::Str]);
    let sup = Type::Union(vec![Type::Int, Type::Str, Type::Bool]);
    assert!(tc.is_subtype(&sub, &sup));
}

#[test]
fn test_union_subtype_reflexive() {
    // Int | Str  <:  Int | Str  (trivially)
    let tc = TypeChecker::new();
    let u = Type::Union(vec![Type::Int, Type::Str]);
    assert!(tc.is_subtype(&u, &u));
}

#[test]
fn test_union_subtype_not_wider() {
    // Int | Str | Bool  is NOT <:  Int | Str  (Bool has no home)
    let tc = TypeChecker::new();
    let sub = Type::Union(vec![Type::Int, Type::Str, Type::Bool]);
    let sup = Type::Union(vec![Type::Int, Type::Str]);
    assert!(!tc.is_subtype(&sub, &sup));
}

#[test]
fn test_scalar_subtype_of_union() {
    // Int  <:  Int | Str | Bool
    let tc = TypeChecker::new();
    assert!(tc.is_subtype(&Type::Int, &Type::Union(vec![Type::Int, Type::Str, Type::Bool])));
}

#[test]
fn test_scalar_not_subtype_of_disjoint_union() {
    // Float  is NOT <:  Int | Str
    let tc = TypeChecker::new();
    assert!(!tc.is_subtype(&Type::Float, &Type::Union(vec![Type::Int, Type::Str])));
}

#[test]
fn test_union_order_independent_subtype() {
    // Subtype check works regardless of variant order.
    // Str | Int  <:  Bool | Int | Str
    let tc = TypeChecker::new();
    let sub = Type::Union(vec![Type::Str, Type::Int]);
    let sup = Type::Union(vec![Type::Bool, Type::Int, Type::Str]);
    assert!(tc.is_subtype(&sub, &sup));
}

// ── Named function declarations ───────────────────────────────────────────────

fn infer_src(src: &str) -> Result<Type, String> {
    let ast = Parser::parse(src).map_err(|e| format!("{:?}", e))?;
    let mut tc = TypeChecker::new();
    tc.infer(&ast).map_err(|e| format!("{}", e))
}

#[test]
fn test_func_decl_inferred_return() {
    // func double(x: Int) = x + x  →  (Int) -> Int, stored in context
    let src = "func double(x: Int) = x + x";
    let ty = infer_src(src).unwrap();
    // The statement type is the function type
    assert_eq!(ty, Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) });
}

#[test]
fn test_func_decl_annotated_return() {
    // func double(x: Int): Int = x + x
    let ty = infer_src("func double(x: Int): Int = x + x").unwrap();
    assert_eq!(ty, Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) });
}

#[test]
fn test_func_decl_return_type_mismatch() {
    // func f(x: Int): Bool = x + 1  →  type error
    assert!(infer_src("func f(x: Int): Bool = x + 1").is_err());
}

#[test]
fn test_func_decl_no_params() {
    // func answer(): Int = 42  →  () -> Int
    let ty = infer_src("func answer(): Int = 42").unwrap();
    assert_eq!(ty, Type::Function { params: vec![], result: Box::new(Type::Int) });
}

#[test]
fn test_func_decl_multiple_params() {
    // func add(x: Int, y: Int): Int = x + y
    let ty = infer_src("func add(x: Int, y: Int): Int = x + y").unwrap();
    assert_eq!(ty, Type::Function { params: vec![Type::Int, Type::Int], result: Box::new(Type::Int) });
}

#[test]
fn test_func_decl_callable_after_decl() {
    // func double(x: Int): Int = x + x
    // double(21)
    let src = "func double(x: Int): Int = x + x\ndouble(21)";
    let ty = infer_src(src).unwrap();
    assert_eq!(ty, Type::Int);
}

#[test]
fn test_func_decl_polymorphic_param() {
    // func id(x) = x  →  ~t -> ~t  (identity)
    let ty = infer_src("func id(x) = x").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
}

// ── Higher-order functions ────────────────────────────────────────────────────

#[test]
fn test_hof_param_called_with_int() {
    // f -> f(1)  →  ([Int] -> ~r) -> ~r
    let ty = infer_src("f -> f(1)").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
    if let Type::Function { params, .. } = ty {
        assert_eq!(params.len(), 1);
        assert!(matches!(&params[0], Type::Function { params: inner_p, .. } if inner_p == &vec![Type::Int]));
    }
}

#[test]
fn test_hof_apply() {
    // f -> x -> f(x)  — apply combinator
    let ty = infer_src("f -> x -> f(x)").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
}

#[test]
fn test_hof_compose() {
    // f -> g -> x -> f(g(x))
    let ty = infer_src("f -> g -> x -> f(g(x))").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
}

#[test]
fn test_hof_constraint_propagated_from_call_site() {
    // f -> f(1) + f(2): both calls constrain f to Int->Num, result is Num
    let ty = infer_src("f -> f(1) + f(2)").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
}

#[test]
fn test_hof_inconsistent_args_err() {
    // f -> f(true) + f(1): first call binds f to Bool->_, second call tries Int — type error
    assert!(infer_src("f -> f(true) + f(1)").is_err());
}

#[test]
fn test_hof_with_named_func() {
    // func apply(f, x: Int) = f(x)   (return type inferred)
    // apply(n -> n + 1, 5)  →  Int
    let src = "func apply(f, x: Int) = f(x)\napply(n -> n + 1, 5)";
    let ty = infer_src(src).unwrap();
    assert_eq!(ty, Type::Int);
}

// ── Bug #2: annotation unification ───────────────────────────────────────────

#[test]
fn test_annotation_constrains_typevar() {
    // x -> (x : Int)  should constrain x to Int, giving Int -> Int
    let ty = infer_src("x -> (x : Int)").unwrap();
    assert_eq!(ty, Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) });
}

#[test]
fn test_return_type_annotation_constrains_body() {
    // func apply(f, x: Int): Int = f(x)  — body infers ~t, annotation constrains to Int
    let ty = infer_src("func apply(f, x: Int): Int = f(x)").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
    if let Type::Function { result, .. } = ty {
        assert_eq!(*result, Type::Int);
    }
}

#[test]
fn test_annotation_wrong_type_still_errors() {
    assert!(infer_src("5 : Bool").is_err());
    assert!(infer_src("true : Int").is_err());
}

// ── Bug #3: union satisfies trait bounds ─────────────────────────────────────

#[test]
fn test_union_num_satisfies_arithmetic() {
    // Int | Float satisfies Num, so (Int|Float) + 2 is valid → Int | Float
    let ty = infer_src("(if true then 1 else 1.0) + 2").unwrap();
    // The union comes directly from the conditional branches (not normalized),
    // so order matches branch order: true=Int, else=Float.
    assert_eq!(ty, Type::Union(vec![Type::Int, Type::Float]));
}

#[test]
fn test_union_num_both_operands() {
    // Both operands are Int|Float — result is the same union
    let ty = infer_src("(if true then 1 else 1.0) * (if false then 2 else 2.0)").unwrap();
    assert_eq!(ty, Type::Union(vec![Type::Int, Type::Float]));
}

#[test]
fn test_union_partial_num_still_errors() {
    // Int | Str does not satisfy Num
    assert!(infer_src("(if true then 1 else \"hi\") + 2").is_err());
}

#[test]
fn test_union_eq_constraint() {
    // Bool | Str satisfies Eq, so == is valid
    let ty = infer_src("(if true then true else \"hi\") == true").unwrap();
    assert_eq!(ty, Type::Bool);
}

// ── Block / conditional-branch lexical scoping ────────────────────────────────
//
// A `let` made inside a block or a conditional branch must not remain bound
// once control leaves it — otherwise codegen can be asked to reference an
// SSA value that only exists on one control-flow path, which used to crash
// the Cranelift verifier instead of producing a type error here.

#[test]
fn test_let_in_braced_branch_does_not_leak() {
    let err = infer_src("if true then { let y = 5 } else 0\ny").unwrap_err();
    assert!(err.contains("Unbound variable"), "unexpected error: {}", err);
}

#[test]
fn test_let_in_bare_branch_does_not_leak() {
    // Branches don't require `{ }` — `if c then let y = 5 else 0` is valid
    // syntax, and must be scoped exactly like the braced form above.
    let err = infer_src("if true then let y = 5 else 0\ny").unwrap_err();
    assert!(err.contains("Unbound variable"), "unexpected error: {}", err);
}

#[test]
fn test_let_in_branch_visible_within_same_branch() {
    // A binding IS visible to the rest of its own branch — only leakage
    // past the conditional is rejected.
    let ty = infer_src("if true then { let y = 5; y + 1 } else 0").unwrap();
    assert_eq!(ty, Type::Int);
}

#[test]
fn test_let_in_nested_block_does_not_leak() {
    // A bare `{ }` block (not attached to an `if`) is also its own scope.
    // Two statements, so the parser doesn't unwrap `{ }` down to a single
    // bare expression (see test_parser::test_block_single_expr_unwraps) —
    // that unwrapping is exactly why the single-statement `{ let y = 5 }`
    // case above is caught by conditional-branch scoping, not Block scoping.
    let err = infer_src("{ let z = 1; z + 1 }\nz").unwrap_err();
    assert!(err.contains("Unbound variable"), "unexpected error: {}", err);
}

// ── Bug #4: colon no longer absorbs arrow ────────────────────────────────────

#[test]
fn test_annotation_does_not_absorb_arrow() {
    // `x : Int -> x + 1` used to silently parse as `x : (Int -> x+1)`.
    // Now `:` parses its RHS at TypeAnnotation precedence (above Assign), so
    // `->` doesn't fire there. The outer `->` then tries arrow_func on
    // `Annotated(x, Int)` which is not a valid lambda param → parse error.
    assert!(Parser::parse("x : Int -> x + 1").is_err());
}

#[test]
fn test_lambda_body_annotation_parsed_correctly() {
    // `x -> x : Int` should parse as `x -> (x : Int)`, constraining x to Int
    let ty = infer_src("x -> x : Int").unwrap();
    assert_eq!(ty, Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) });
}

#[test]
fn test_function_type_annotation_let() {
    // `let f: (Int -> Int) = n -> n + 1` — parenthesised function type in let annotation
    let ty = infer_src("let f: (Int -> Int) = n -> n + 1").unwrap();
    assert_eq!(ty, Type::Function { params: vec![Type::Int], result: Box::new(Type::Int) });
}

#[test]
fn test_function_type_annotation_wrong_type() {
    // Annotation mismatch: declared (Int -> Int) but body returns Bool
    assert!(infer_src("let f: (Int -> Int) = n -> true").is_err());
}

#[test]
fn test_function_type_annotation_expr() {
    // `f : (Int -> Int)` — resolves the function type even in expression position
    // (f is unbound, but the annotation itself must parse and resolve correctly)
    assert!(Parser::parse("f : (Int -> Int)").is_ok());
}

#[test]
fn test_curried_function_type_annotation() {
    // `(Int -> Int -> Int)` is a valid curried function type annotation
    let ty = infer_src("let f: (Int -> Int -> Int) = x -> y -> x + y").unwrap();
    assert!(matches!(ty, Type::Function { .. }));
}
