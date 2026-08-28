use std::{fmt::Debug, vec};
use froglang_core::frontend::{expression::{Expression, Mutability, Parameter}, tokens::{Span, Spanned, Token}, type_expr::TypeExpr, typeck::{Trait, Type, TypeChecker}};
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
    let params = params.iter().map(|p| Parameter { name: p.to_string(), ty: None, mutable: false }).collect();
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

    // `not` on a non-Bool, non-Truthy type (a Function) should still fail —
    // Int/Float/Str/List/None coerce via `Trait::Truthy` (see ERRORS.md
    // Phase 6), but a Function doesn't.
    let expr = spanned(Expression::unary(Token::Not, lambda(vec!["x"], ident("x").item)));
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

    // A Function condition is neither Bool nor Truthy (see ERRORS.md Phase
    // 6) — Int itself now coerces via `Trait::Truthy`, so this test uses a
    // type that still doesn't.
    let cond = spanned(Expression::conditional(
        lambda(vec!["x"], ident("x").item),
        int(2),
        Some(int(3))
    ));
    let res = t.infer(&cond);
    assert!(res.is_err());
}

#[test]
fn test_conditional_branch_mismatch_produces_union() {
    let mut t = TypeChecker::new();

    // Mismatched branches produce a union type instead of an error.
    // `Int | Bool` rather than `Int | Str`: `infer` now lowers as it goes
    // (one pass), and `lower_widen` rejects a `Str` member of a union that
    // needs boxing outright — so `if c then 1 else "hello"` is not a program
    // that compiles, and never was; the old two-pass `infer` just stopped
    // before finding out.
    let cond = spanned(Expression::conditional(
        bool_lit(true),
        int(1),            // Int
        Some(bool_lit(false))  // Bool
    ));
    let res = t.infer(&cond).unwrap();
    assert_eq!(res, Type::Union(vec![Type::Int, Type::Bool]));
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

    // let x = 1; x + 2
    let assign = spanned(Expression::assign(ident("x"), None, int(1), Some(Mutability::Immutable)));
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
    // { let x = 1; x + 2 } :: Int (block type = type of last expression)
    let block = spanned(Expression::Block(vec![
        spanned(Expression::assign(ident("x"), None, int(1), Some(Mutability::Immutable))),
        spanned(Expression::binary(Token::Plus, ident("x"), int(2))),
    ]));
    assert_eq!(t.infer(&block).unwrap(), Type::Int);
}

#[test]
fn test_list_type_inference() {
    let mut t = TypeChecker::new();
    // [1, 2, 3] :: List<Int>
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    assert_eq!(t.infer(&list).unwrap(), Type::list(Type::Int));
}

#[test]
fn test_index_type_inference() {
    let mut t = TypeChecker::new();
    // [1, 2, 3][0] :: Int
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let idx = spanned(Expression::index(list, int(0)));
    assert_eq!(t.infer(&idx).unwrap(), Type::Int);
}

#[test]
fn test_index_non_list_is_error() {
    let mut t = TypeChecker::new();
    // 5[0] is a type error: 5 is not a List
    let idx = spanned(Expression::index(int(5), int(0)));
    assert!(t.infer(&idx).is_err());
}

#[test]
fn test_index_non_int_index_is_error() {
    let mut t = TypeChecker::new();
    // [1, 2, 3][true] is a type error: index must be Int
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let idx = spanned(Expression::index(list, bool_lit(true)));
    assert!(t.infer(&idx).is_err());
}

#[test]
fn test_slice_type_inference() {
    let mut t = TypeChecker::new();
    // [1, 2, 3][0:2] :: List<Int>
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let s = spanned(Expression::slice(list, Some(int(0)), Some(int(2))));
    assert_eq!(t.infer(&s).unwrap(), Type::list(Type::Int));
}

#[test]
fn test_slice_omitted_bounds() {
    let mut t = TypeChecker::new();
    // [1, 2, 3][:] :: List<Int>
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let s = spanned(Expression::slice(list, None, None));
    assert_eq!(t.infer(&s).unwrap(), Type::list(Type::Int));
}

#[test]
fn test_slice_non_list_is_error() {
    let mut t = TypeChecker::new();
    let s = spanned(Expression::slice(int(5), Some(int(0)), None));
    assert!(t.infer(&s).is_err());
}

#[test]
fn test_slice_non_int_bound_is_error() {
    let mut t = TypeChecker::new();
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let s = spanned(Expression::slice(list, Some(bool_lit(true)), None));
    assert!(t.infer(&s).is_err());
}

#[test]
fn test_range_type_inference() {
    let mut t = TypeChecker::new();
    // 1..5 :: List<Int>
    let r = spanned(Expression::range(int(1), int(5)));
    assert_eq!(t.infer(&r).unwrap(), Type::list(Type::Int));
}

#[test]
fn test_range_non_int_bound_is_error() {
    let mut t = TypeChecker::new();
    let r = spanned(Expression::range(bool_lit(true), int(5)));
    assert!(t.infer(&r).is_err());

    let mut t2 = TypeChecker::new();
    let r2 = spanned(Expression::range(int(1), bool_lit(true)));
    assert!(t2.infer(&r2).is_err());
}

#[test]
fn test_for_loop_type_inference() {
    let mut t = TypeChecker::new();
    // for x in [1, 2, 3] do x  ::  None
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let fl = spanned(Expression::for_loop("x".to_string(), list, None, ident("x")));
    assert_eq!(t.infer(&fl).unwrap(), Type::None);
}

#[test]
fn test_for_loop_var_scoped_to_body() {
    let mut t = TypeChecker::new();
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let fl = spanned(Expression::for_loop("x".to_string(), list, None, ident("x")));
    t.infer(&fl).unwrap();
    // `x` must not leak into the surrounding scope after the loop.
    assert!(t.infer(&ident("x")).is_err());
}

#[test]
fn test_for_loop_non_list_iterable_is_error() {
    let mut t = TypeChecker::new();
    let fl = spanned(Expression::for_loop("x".to_string(), int(5), None, ident("x")));
    assert!(t.infer(&fl).is_err());
}

#[test]
fn test_for_loop_non_bool_cond_is_error() {
    let mut t = TypeChecker::new();
    // A Function guard is neither Bool nor Truthy (see ERRORS.md Phase 6) —
    // Int itself now coerces via `Trait::Truthy`, so this test uses a type
    // that still doesn't.
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let fl = spanned(Expression::for_loop("x".to_string(), list, Some(lambda(vec!["y"], ident("y").item)), ident("x")));
    assert!(t.infer(&fl).is_err());
}

#[test]
fn test_comprehension_type_inference() {
    let mut t = TypeChecker::new();
    // [for x in [1, 2, 3] do x]  ::  List<Int>
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let fl = spanned(Expression::for_loop("x".to_string(), list, None, ident("x")));
    let comp = spanned(Expression::comprehension(fl));
    assert_eq!(t.infer(&comp).unwrap(), Type::list(Type::Int));
}

#[test]
fn test_comprehension_with_filter_type_inference() {
    let mut t = TypeChecker::new();
    // [for x in [1, 2, 3] if <Function> do x] — a Function guard is neither
    // Bool nor Truthy (see ERRORS.md Phase 6); `if x` itself is no longer a
    // type error since `x: Int` now coerces via `Trait::Truthy`.
    let list = spanned(Expression::Tuple(vec![int(1), int(2), int(3)]));
    let fl = spanned(Expression::for_loop("x".to_string(), list, Some(lambda(vec!["y"], ident("y").item)), ident("x")));
    let comp = spanned(Expression::comprehension(fl));
    assert!(t.infer(&comp).is_err());
}

#[test]
fn test_assignment_with_type_annotation() {
    let mut t = TypeChecker::new();
    // let x: Int = 5
    let ann_type = spanned(TypeExpr::Name("Int".to_string()));
    let assign = spanned(Expression::assign(ident("x"), Some(ann_type), int(5), Some(Mutability::Immutable)));
    assert_eq!(t.infer(&assign).unwrap(), Type::Int);
    // Type mismatch: let x: Bool = 5 should fail
    let mut t2 = TypeChecker::new();
    let ann_type2 = spanned(TypeExpr::Name("Bool".to_string()));
    let assign2 = spanned(Expression::assign(ident("x"), Some(ann_type2), int(5), Some(Mutability::Immutable)));
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
    // if true then 1 else false  →  Int | Bool (see the note on
    // `test_conditional_branch_mismatch_produces_union` for why not `Str`)
    let cond = spanned(Expression::conditional(bool_lit(true), int(1), Some(bool_lit(false))));
    assert_eq!(t.infer(&cond).unwrap(), Type::Union(vec![Type::Int, Type::Bool]));
}

#[test]
fn test_is_subtype_via_check() {
    let mut t = TypeChecker::new();
    // An Int value satisfies an expected type of Int | Bool. `check`
    // reports the type the value checks *at* — the union, since that is
    // what the value has once it has been widened into the slot, not the
    // narrower `Int` it started as.
    let expected = Type::Union(vec![Type::Bool, Type::Int]);
    let res = t.check(&int(1), &expected).unwrap();
    assert_eq!(res, expected);
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
    // Int | (Str | Bool)  →  Int | Bool | Str  (flattened + sorted into
    // `Ord for Type`'s canonical order, which is what assigns the tags)
    let u = Type::Union(vec![
        Type::Int,
        Type::Union(vec![Type::Str, Type::Bool]),
    ]).normalize();
    assert_eq!(u, Type::Union(vec![Type::Int, Type::Bool, Type::Str]));
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
    // Bool | Int | Str | Bool | Int  →  Int | Bool | Str
    let u = Type::Union(vec![
        Type::Bool, Type::Int, Type::Str, Type::Bool, Type::Int,
    ]).normalize();
    assert_eq!(u, Type::Union(vec![Type::Int, Type::Bool, Type::Str]));
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
    // `Conditional`'s branch join normalizes (like every other join site),
    // so member order is canonical (`Ord for Type`), not branch order.
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
    // Int | Bool does not satisfy Num
    assert!(infer_src("(if true then 1 else false) + 2").is_err());
}

#[test]
fn test_union_eq_constraint() {
    // Bool | Int satisfies Eq, so == is valid
    let ty = infer_src("(if true then true else 1) == true").unwrap();
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

// ── `mut` argument checking: scoping and positional alignment ─────────────────

#[test]
fn mut_exclusivity_check_stays_aligned_with_argument_positions() {
    // `lower_call` built `all_roots` by pushing only the arguments that are
    // bare identifiers, then indexed it by *argument* position in the
    // exclusivity loop. Any non-identifier argument before a `mut` one
    // therefore shifted the two out of step: this call has two arguments
    // and one identifier among them, so the loop indexed a length-1 vector
    // at 1 and panicked ("index out of bounds").
    //
    // It is a panic rather than a wrong answer, which is why no amount of
    // existing `mut` testing caught it — every prior test passed bare
    // identifiers for every argument.
    let src = "\
        func plain(x: Int): Int = x + 1\n\
        func bump(a: Int, mut b: Int): Int = { b = b + a\nb }\n\
        mut n = 1\n\
        bump(plain(2), mut n)";
    assert!(infer_src(src).is_ok(), "expected this to type-check: {:?}", infer_src(src));
}

#[test]
fn mut_exclusivity_still_rejects_a_repeated_root_past_a_non_identifier_argument() {
    // The other half of the same fix: realigning the vector must not lose
    // the check it exists for. `n` is passed `mut` and also appears as a
    // later argument, with a non-identifier argument in front to exercise
    // the alignment.
    let src = "\
        func plain(x: Int): Int = x + 1\n\
        func f(a: Int, mut b: Int, c: Int): Int = { b = b + a + c\nb }\n\
        mut n = 1\n\
        f(plain(2), mut n, n)";
    let err = infer_src(src).expect_err("expected an exclusivity error");
    assert!(
        err.contains("can't be passed 'mut' and also appear as another argument"),
        "unexpected error: {}", err,
    );
}

#[test]
fn a_nested_funcs_mut_parameters_do_not_leak_to_the_outer_scope() {
    // `func_mut_params` is a flat name -> mutability-list map with no scope
    // structure, so an inner `func f` used to keep dictating argument
    // mutability for calls to the *outer* `f` after its scope closed. Here
    // the inner `f` declares a `mut` first parameter, so the later
    // top-level `f(5)` was rejected with "argument 1 must be marked 'mut'"
    // — an error about a function that has no `mut` parameters at all.
    //
    // Checked at the type-checker level rather than by running it: a nested
    // named `func` is a separate, pre-existing codegen limitation.
    let src = "\
        func f(x: Int): Int = x * 10\n\
        func outer(): Int = {\n\
          func f(mut a: Int, b: Int): Int = { a = a + b\na }\n\
          mut m = 1\n\
          f(mut m, 2)\n\
        }\n\
        f(5)";
    assert!(infer_src(src).is_ok(), "expected this to type-check: {:?}", infer_src(src));
}

// ── Occurs check (TRAITS.md Stage 2) ────────────────────────────────────────

/// Pushing an empty mutable list's own alias into itself unifies the
/// list's still-open element TypeVar with the list's own type — `~t` with
/// `List<~t>` — which is exactly the infinite type an occurs check exists
/// to reject. Two separate bindings (`xs`/`ys`) rather than `xs.push(xs)`
/// directly, since that's already rejected earlier by `finish_push`'s
/// same-root exclusivity check (`push_with_the_same_root_as_both_arguments_
/// is_rejected` in test_value_semantics.rs) — this test wants the occurs
/// check specifically, not that unrelated guard.
#[test]
fn pushing_a_lists_own_alias_into_itself_is_a_type_error_not_a_hang() {
    let src = "\
        mut xs = []\n\
        mut ys = xs\n\
        xs.push(ys)\n\
        xs";
    let err = infer_src(src).unwrap_err();
    assert!(err.contains("Can't unify"), "unexpected error: {}", err);
}

// ── Type schemes: generalization and instantiation (TRAITS.md Stage 2) ────────

/// The acceptance test stated in TRAITS.md Stage 2, verbatim: a
/// let-bound lambda generalizes, so each call gets its own fresh
/// instantiation instead of the first call permanently pinning the type.
#[test]
fn a_let_bound_lambda_generalizes_across_calls_at_different_types() {
    let src = "{ let f = x -> x + x\nlet a = f(1)\nlet b = f(1.5)\na }";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

/// Same acceptance case, spelled as a `func` declaration rather than a
/// let-bound lambda — both are "a syntactic function value", the value
/// restriction's criterion.
#[test]
fn a_func_declaration_generalizes_across_calls_at_different_types() {
    let src = "{ func id(x) = x\nlet a = id(1)\nlet b = id(\"s\")\na }";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

/// The value restriction: a `mut`-bound lambda does not generalize, even
/// though its value is a syntactic function — the same program with `let`
/// in place of `mut` type-checks (previous test), so this failure is
/// specifically about mutability, not about the lambda shape.
#[test]
fn a_mut_bound_lambda_does_not_generalize() {
    let src = "{ mut f = x -> x + x\nlet a = f(1)\nlet b = f(1.5)\na }";
    let err = infer_src(src).unwrap_err();
    assert!(err.contains("Can't unify"), "unexpected error: {}", err);
}

/// Bounds travel from a scheme's binders onto each fresh instantiation
/// (not just the first) — `max`'s `x > y` requires `Ord`, so an `Ord`
/// call succeeds and a non-`Ord` call (`Bool`) fails, at every
/// instantiation, not only the one that happened to run first.
#[test]
fn a_generic_functions_bounds_are_enforced_at_every_instantiation() {
    let src = "{ func maxof(x, y) = if x > y then x else y\nlet a = maxof(1, 2)\nlet b = maxof(true, false)\na }";
    let err = infer_src(src).unwrap_err();
    assert!(err.contains("Can't unify"), "unexpected error: {}", err);
}

/// UFCS resolution step 3 (`lower_ufcs_call`'s free-function branch) does
/// its own `ctx.get` lookup outside `lower_literal`'s `Var` arm, so it
/// needs its own instantiation call — without it, a generic free
/// function's receiver parameter binds permanently to whichever type
/// dot-called it first.
#[test]
fn ufcs_over_a_generic_free_function_resolves_independently_at_each_receiver_type() {
    let src = "{ func id(x) = x\nlet a = (1).id()\nlet b = (\"s\").id()\na }";
    assert_eq!(infer_src(src).unwrap(), Type::Int);
}

// ── `TRAITS.md` Stage 3a: generic `data` declarations ────────────────────────

/// `==`'s operator-type check (`builtin_op_type`'s `"==" | "!="` arm)
/// unifies both operands against a fresh `Eq`-bounded `TypeVar`, which
/// consults `type_implements`'s `Eq` arm when binding it to a concrete
/// type. For a generic struct instantiation that arm now recurses into
/// `args` — `Pair<Int, Int>` is `Eq` because `Int` is (on both args).
#[test]
fn a_generic_struct_instantiated_at_eq_args_is_itself_eq() {
    let src = "data Pair<A, B>(fst: A, snd: B)\n\
               Pair(fst=1, snd=2) == Pair(fst=3, snd=4)";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);
}

/// Instantiated at a composite arg, which is where the recursion into
/// `args` actually earns its keep: `Box<List<Str>>` is `Eq` only because
/// `List<Str>` is, and that answer comes from the `List` arm, not from
/// "it's a registered struct name".
///
/// There is deliberately no negative twin any more. Every *value* type is
/// `Eq` now — scalars, `None`, structs, lists, unions, and any nesting of
/// them — which is the point of the `DATA.md` effort; the two types that
/// aren't (`Never`, and a function type) can't be a struct field or a type
/// argument. What used to be the negative case, a recursive type, is
/// rejected at the operator by `check_comparable` instead, with a
/// different error — see `a_self_referential_union_is_rejected_at_the_operator`.
#[test]
fn a_generic_struct_instantiated_at_a_composite_arg_is_eq() {
    let src = "data Box<A>(item: A)\n\
               Box(item=[\"a\"]) == Box(item=[\"b\"])";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);
}

// ── `plans/DATA.md` stage 1: structural derivation recurses into fields ──────

/// `Eq` is structural for every value type: the derivation recurses into a
/// struct's fields, a list's elements and a union's members, at any nesting.
/// The composites here are the ones that each needed their own arm — a
/// nested struct, a `List`, a union with a payload, an optional.
///
/// Before the recursion existed, every struct name satisfied `Eq` outright,
/// and since `build_struct_eq`'s synthesized per-field comparisons are never
/// re-checked, a field whose type had no real comparison was compared as a
/// raw pointer: two structs with equal contents answered `false`, silently.
#[test]
fn a_struct_is_eq_when_its_fields_are() {
    let src = "data Inner(v: Int)\n\
               data Outer(i: Inner, name: Str)\n\
               Outer(i=Inner(v=1), name=\"a\") == Outer(i=Inner(v=1), name=\"a\")";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);

    let src = "data W(xs: List<Int>)\n\
               W(xs=[1]) == W(xs=[1])";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);

    let src = "data Shape is Sq(w: Int) | Circle(r: Int)\n\
               data Inner(s: Shape)\n\
               data Outer(i: Inner)\n\
               Outer(i=Inner(s=Sq(w=1))) == Outer(i=Inner(s=Sq(w=1)))";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);

    let src = "data W(o: Int | None)\n\
               W(o=1) == W(o=none)";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);
}

// ── recursive types: rejected at the operator, not by the trait ──────────────
//
// A recursive type *is* structurally `Eq` — `type_implements` says so, and
// its `seen` guard is what makes deciding that terminate. What's missing is
// a way to *emit* the comparison: both `eq_union` and `eq_list` monomorphize
// one statically known type per step, so a type that encloses itself would
// need unbounded code. `check_comparable` predicts that and reports it at
// the operator, which is where the limit actually is.

#[test]
fn a_self_referential_union_is_rejected_at_the_operator() {
    let src = "data Node is Lit(v: Int) | Add(lhs: Node, rhs: Node)\n\
               Lit(v=1) == Lit(v=2)";
    let err = infer_src(src).unwrap_err();
    assert!(err.contains("recursive") && err.contains("comparing"), "unexpected error: {}", err);
}

/// The other way a cycle can close: a struct reaching itself through a
/// `List` field. Nothing boxes it away the way a union member is boxed, and
/// before `plans/DATA.md` stage 0 taught the walks to descend into a list's
/// element type it wasn't a cycle for them at all — `print` on one of these
/// overflowed the compiler's own stack.
#[test]
fn a_struct_recursive_through_a_list_field_is_rejected_at_the_operator() {
    let src = "data Tree(v: Int, kids: List<Tree>)\n\
               Tree(v=1, kids=[]) == Tree(v=1, kids=[])";
    let err = infer_src(src).unwrap_err();
    assert!(err.contains("recursive") && err.contains("Tree"), "unexpected error: {}", err);
}

/// Two sibling fields of the same type are not a cycle — `check_comparable`
/// pops its stack on the way back out, so it must not mistake a diamond for
/// recursion.
#[test]
fn a_type_repeated_across_sibling_fields_is_not_recursive() {
    let src = "data Inner(v: Int)\n\
               data Outer(a: Inner, b: Inner)\n\
               Outer(a=Inner(v=1), b=Inner(v=2)) == Outer(a=Inner(v=1), b=Inner(v=2))";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);
}

// ── structural equality on lists and unions ──────────────────────────────────

/// `List<T>` is `Eq` when `T` is — what anyone coming from Python expects
/// `[1, 2] == [1, 2]` to mean. It used to be a flat "requires Eq, got
/// [Int]".
#[test]
fn a_list_of_eq_elements_is_eq() {
    assert_eq!(infer_src("[1, 2] == [1, 2]").unwrap(), Type::Bool);
    assert_eq!(infer_src("[[\"a\"]] != [[\"b\"]]").unwrap(), Type::Bool);
}

/// An empty list literal never fixes its element type, so the check meets a
/// bare variable there — undetermined, not failing.
#[test]
fn an_empty_list_is_eq() {
    assert_eq!(infer_src("[] == []").unwrap(), Type::Bool);
}

/// Unions, both kinds: a payload-less one (whose value *is* its tag) and a
/// payload-carrying one (which needs `eq_union`'s runtime dispatch).
#[test]
fn a_union_is_eq_when_its_members_are() {
    let src = "data Color is Red | Green | Blue\n\
               Red == Green";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);

    let src = "data Shape is Sq(w: Int) | Circle(r: Int)\n\
               [Sq(w=1)] == [Circle(r=2)]";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);
}

/// `None` is `Eq`, so an optional is — without it `Int | None` failed the
/// "every member is `Eq`" rule and no optional could be compared at all.
#[test]
fn an_optional_is_eq() {
    assert_eq!(infer_src("none == none").unwrap(), Type::Bool);
    let src = "func f(c: Bool): Int | None = if c then 1 else none\n\
               f(true) == f(false)";
    assert_eq!(infer_src(src).unwrap(), Type::Bool);
}
