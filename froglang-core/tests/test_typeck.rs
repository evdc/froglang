use std::fmt::Debug;
use froglang_core::frontend::{expression::{Expression, Parameter}, tokens::{Span, Spanned, Token}, typeck::{Type, TypeChecker}};

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
    
    // Test each basic type
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
fn test_float_division() {
    let mut t = TypeChecker::new();
    let expr = spanned(Expression::binary(Token::Slash, float(4.0), float(2.0)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Float);
}

#[test]
fn test_division_type_mismatch() {
    let mut t = TypeChecker::new();
    // Division requires floats, but we're passing ints
    let expr = spanned(Expression::binary(Token::Slash, int(4), int(2)));
    let res = t.infer(&expr);
    assert!(res.is_err());
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
fn test_unary_ops() {
    let mut t = TypeChecker::new();
    
    // Unary minus on int
    let expr = spanned(Expression::unary(Token::Minus, int(5)));
    assert_eq!(t.infer(&expr).unwrap(), Type::Int);
    
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
#[ignore = "should type as Int | None, not implemented yet"]
fn test_conditional_no_else() {
    let mut t = TypeChecker::new();
    
    let cond = spanned(Expression::conditional(
        bool_lit(true),
        int(1),
        None
    ));
    assert_eq!(t.infer(&cond).unwrap(), Type::Int);
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
fn test_conditional_branch_mismatch() {
    let mut t = TypeChecker::new();
    
    let cond = spanned(Expression::conditional(
        bool_lit(true),
        int(1),        // Int
        Some(string("hello"))  // Str - should fail
    ));
    let res = t.infer(&cond);
    assert!(res.is_err());
    assert!(res.unwrap_err().item.msg.contains("must both unify"));
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
        // Both params should be type variables
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
    
    // Should be a function returning a function
    if let Type::Function { result, .. } = res {
        assert!(matches!(*result, Type::Function { .. }));
    } else {
        panic!("Expected function type");
    }
}

// #[test]
// fn test_function_call_arity_mismatch() {
//     let _t = TypeChecker::new();
    
//     // Create lambda that takes 2 params but call with 1
//     let lambda = lambda(vec!["x", "y"], Expression::literal(Token::Identifier("x".to_string())));
//     let _call = spanned(Expression::call(lambda, vec![int(5)]));
    
//     // This should actually panic or fail in zip - the current implementation has a bug here
//     // The zip will just ignore extra params, which is wrong
//     // We should add an arity check
// }

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
    
    // Create a scenario where type variables need to be unified transitively
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
    // First create x -> y -> x
    let inner_lambda = lambda(vec!["y"], Expression::literal(Token::Identifier("x".to_string())));
    let outer_lambda = lambda(vec!["x"], inner_lambda.item);
    
    // Apply to 5
    let first_call = spanned(Expression::call(outer_lambda, vec![int(5)]));
    
    // Apply result to true
    let second_call = spanned(Expression::call(first_call, vec![bool_lit(true)]));
    
    let res = t.infer(&second_call).unwrap();
    assert_eq!(res, Type::Int);
}

#[test]
fn test_variable_shadowing_in_lambda() {
    let mut t = TypeChecker::new();
    
    // Assuming we have a variable 'x' in outer scope, lambda should shadow it
    // This test checks that the lambda parameter doesn't interfere with outer scope
    let lambda = lambda(vec!["x"], Expression::literal(Token::Identifier("x".to_string())));
    let res = t.infer(&lambda).unwrap();
    
    // The lambda should still work fine regardless of outer scope
    assert!(matches!(res, Type::Function { .. }));
}

#[test]
fn test_type_variable_display() {
    // Test the Display implementation for Type::TypeVar
    let tv = Type::TypeVar { name: "test".to_string() };
    assert_eq!(format!("{}", tv), "~test");
    
    let func = Type::Function { 
        params: vec![Type::Int, Type::TypeVar { name: "a".to_string() }],
        result: Box::new(Type::Bool)
    };
    let display_str = format!("{}", func);
    assert!(display_str.contains("Int"));
    assert!(display_str.contains("~a"));
    assert!(display_str.contains("Bool"));
}

#[test]
fn test_deeply_nested_functions() {
    let mut t = TypeChecker::new();
    
    // Test with deeply nested function calls to stress the unification system
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
    let err = t.infer(&add).unwrap_err();
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