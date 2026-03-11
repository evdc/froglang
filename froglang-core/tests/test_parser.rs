use froglang_core::frontend::parser::{ParseError, Precedence};
use froglang_core::frontend::expression::{Expression, Parameter};
use froglang_core::frontend::lexer::LexerError;
use froglang_core::frontend::tokens::{Position, Spanned, Token};
use froglang_core::frontend::parser::*;


#[inline]
pub fn pos(l: u32, c: u32) -> Position {
    Position { line: l, col: c}
}

// TODO: the current impl is right associative for binary operators of equal precedence
// Addition etc is generally supposed to be left associative

#[test]
fn test_basic_arithmetic() {
    // Testing 1 + 2
    assert_eq!(
        Parser::new("1 + 2").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Plus,
            Spanned::new(Expression::literal(Token::Int(1)), pos(0, 0), pos(0, 1)),
            Spanned::new(Expression::literal(Token::Int(2)), pos(0, 4), pos(0, 5)),
        )
    );
}

#[test]
fn test_operator_precedence() {
    // Testing 1 + 2 * 3, * has higher precedence than +
    assert_eq!(
        Parser::new("1 + 2 * 3").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Plus,
            Expression::literal(Token::Int(1)).at((0, 0)..(0, 1)),
            Spanned::new(
                Expression::binary(
                    Token::Star,
                    Expression::literal(Token::Int(2)).at((0, 4)..(0, 5)),
                    Expression::literal(Token::Int(3)).at((0, 8)..(0, 9))
                ),
                pos(0, 4), pos(0, 9)
            ),
        )
    );

    assert_eq!(
        Parser::new("1 * 2 + 3").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Plus, 
            Expression::binary(
                Token::Star, 
                Expression::literal(Token::Int(1)).at((0,0)..(0,1)), 
                Expression::literal(Token::Int(2)).at((0, 4)..(0, 5))
            ).at((0,0)..(0,5)),
            Expression::literal(Token::Int(3)).at((0,8)..(0,9))
        )
    );
}

#[test]
fn test_unary_operations() {
    // Testing -5
    assert_eq!(
        Parser::new("-5").expression(Precedence::Assign).unwrap().item,
        Expression::unary(
            Token::Minus,
            Expression::literal(Token::Int(5)).at((0, 1)..(0, 2)),
        )
    );
}

#[test]
fn test_nested_expressions() {
    // With left-associativity, 1 + 2*3 + 4 parses as (1 + (2*3)) + 4
    let actual = Parser::new("1 + 2 * 3 + 4").expression(Precedence::Assign).unwrap().item;
    let expected = Expression::binary(
        Token::Plus,
        Expression::binary(
            Token::Plus,
            Expression::literal(Token::Int(1)).at((0, 0)..(0, 1)),
            Expression::binary(
                Token::Star,
                Expression::literal(Token::Int(2)).at((0, 4)..(0, 5)),
                Expression::literal(Token::Int(3)).at((0, 8)..(0, 9))
            ).at((0, 4)..(0, 9)),
        ).at((0, 0)..(0, 9)),
        Expression::literal(Token::Int(4)).at((0, 12)..(0, 13)),
    );
    assert_eq!(actual, expected, "actual: {}\nexpected: {}", actual, expected);
}

#[test]
fn test_complex_expressions() {
    // Testing 1 + 2 * 3 - 4 / 5
    // With left-associativity: (1 + (2*3)) - (4/5)
    assert_eq!(
        Parser::new("1 + 2 * 3 - 4 / 5").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Minus,
            Expression::binary(
                Token::Plus,
                Expression::literal(Token::Int(1)).at((0, 0)..(0, 1)),
                Expression::binary(
                    Token::Star,
                    Expression::literal(Token::Int(2)).at((0, 4)..(0, 5)),
                    Expression::literal(Token::Int(3)).at((0, 8)..(0, 9))
                ).at((0, 4)..(0, 9))
            ).at((0, 0)..(0, 9)),
            Expression::binary(
                Token::Slash,
                Expression::literal(Token::Int(4)).at((0, 12)..(0, 13)),
                Expression::literal(Token::Int(5)).at((0, 16)..(0, 17))
            ).at((0, 12)..(0, 17))
        )
    );
}

#[test]
fn test_unary_with_binary() {
    // Testing -1 + 2
    assert_eq!(
        Parser::new("-1 + 2").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Plus,
            Expression::unary(
                Token::Minus,
                Expression::literal(Token::Int(1)).at((0, 1)..(0, 2))
            ).at((0, 0)..(0, 2)),
            Expression::literal(Token::Int(2)).at((0, 5)..(0, 6))
        )
    );
}

#[test]
fn test_consecutive_unary() {
    // Testing --5 (double negation)
    assert_eq!(
        Parser::new("--5").expression(Precedence::Assign).unwrap().item,
        Expression::unary(
            Token::Minus,
            Expression::unary(
                Token::Minus,
                Expression::literal(Token::Int(5)).at((0, 2)..(0, 3))
            ).at((0, 1)..(0, 3))
        )
    );
}

#[test]
fn test_string_literals() {
    // Testing string literal
    assert_eq!(
        Parser::new("\"hello\"").expression(Precedence::Assign).unwrap().item,
        Expression::literal(Token::String("hello".to_string()))
    );
}

#[test]
fn test_identifier_literals() {
    // Testing identifiers
    assert_eq!(
        Parser::new("variable").expression(Precedence::Assign).unwrap().item,
        Expression::literal(Token::Identifier("variable".to_string()))
    );
}

#[test]
fn test_error_invalid_prefix() {
    // Testing invalid prefix expression
    let result = Parser::new("+").expression(Precedence::Assign);
    assert!(result.is_err());
    match result {
        Err(err) => assert_eq!(err.item, ParseError::ExpectedExpression),
        _ => panic!("Expected error for invalid prefix expression")
    }
}

#[test]
fn test_error_missing_operand() {
    // Testing binary operation with missing right operand
    let result = Parser::new("1 +").expression(Precedence::Assign);
    assert!(result.is_err());
    match result {
        Err(err) => assert_eq!(err.item, ParseError::ExpectedExpression),
        _ => panic!("Expected error for missing operand")
    }
}

#[test]
fn test_complex_mixed_expressions() {
    // Testing mix of different operators and precedence levels
    assert_eq!(
        Parser::new("-1 * 2 + 3 / -4").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Plus,
            Expression::binary(
                Token::Star,
                Expression::unary(
                    Token::Minus,
                    Expression::literal(Token::Int(1)).at((0, 1)..(0, 2))
                ).at((0, 0)..(0, 2)),
                Expression::literal(Token::Int(2)).at((0, 5)..(0, 6))
            ).at((0, 0)..(0, 6)),
            Expression::binary(
                Token::Slash,
                Expression::literal(Token::Int(3)).at((0, 9)..(0, 10)),
                Expression::unary(
                    Token::Minus,
                    Expression::literal(Token::Int(4)).at((0, 14)..(0, 15))
                ).at((0, 13)..(0, 15))
            ).at((0, 9)..(0, 15))
        )
    );
}

#[test]
fn test_simple_grouping() {
    // Testing (1 + 2) * 3, where parentheses change the default precedence
    assert_eq!(
        Parser::new("(1 + 2) * 3").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Star,
            Expression::binary(
                Token::Plus,
                Expression::literal(Token::Int(1)).at((0, 1)..(0, 2)),
                Expression::literal(Token::Int(2)).at((0, 5)..(0, 6))
            ).at((0, 1)..(0, 6)),
            Expression::literal(Token::Int(3)).at((0, 10)..(0, 11))
        )
    );
}

#[test]
fn test_nested_grouping() {
    // Testing 1 + (2 * (3 - 4))
    let actual = Parser::new("1 + (2 * (3 - 4))").expression(Precedence::Assign).unwrap().item;
    let expected = Expression::binary(
        Token::Plus,
        Expression::literal(Token::Int(1)).at((0, 0)..(0, 1)),
        Expression::binary(
            Token::Star,
            Expression::literal(Token::Int(2)).at((0, 5)..(0, 6)),
            Expression::binary(
                Token::Minus,
                Expression::literal(Token::Int(3)).at((0, 10)..(0, 11)),
                Expression::literal(Token::Int(4)).at((0, 14)..(0, 15))
            ).at((0, 10)..(0, 15))
        ).at((0, 5)..(0, 15))
    );
    assert_eq!(actual, expected);
}

#[test]
fn test_empty_grouping() {
    // Testing empty parentheses - should produce an error
    let result = Parser::new("()").expression(Precedence::Assign);
    let err = result.unwrap_err();
    assert_eq!(err.item, ParseError::ExpectedExpression);
}

#[test]
fn test_unbalanced_grouping() {
    // Testing unbalanced parentheses - should produce an error
    let result = Parser::new("(1 + 2").expression(Precedence::Assign);
    assert_eq!(result.unwrap_err().item, ParseError::ExpectedButFound(Token::RightParen, Token::EOF));

    let result = Parser::parse("1 ) 2");
    assert_eq!(result.unwrap_err()[0].item, ParseError::ExpectedButFound(Token::Newline, Token::RightParen));
}

#[test]
fn test_unexpected_character() {
    let result = Parser::new("1 ? 2").expression(Precedence::Assign);
    assert_eq!(result.unwrap_err().item, ParseError::LexError(LexerError::UnexpectedCharacter));

    let result = Parser::new("1 + ?").expression(Precedence::Assign);
    assert_eq!(result.unwrap_err().item, ParseError::LexError(LexerError::UnexpectedCharacter));
}

#[test]
fn test_complex_with_grouping() {
    // Testing complex expression with grouping: -((1 + 2) * 3 - 4) / 5
    assert_eq!(
        Parser::new("-((1 + 2) * 3 - 4) / 5").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Slash,
            Expression::unary(
                Token::Minus,
                Expression::binary(
                    Token::Minus,
                    Expression::binary(
                        Token::Star,
                        Expression::binary(
                            Token::Plus,
                            Expression::literal(Token::Int(1)).at((0, 3)..(0, 4)),
                            Expression::literal(Token::Int(2)).at((0, 7)..(0, 8))
                        ).at((0, 3)..(0, 8)),
                        Expression::literal(Token::Int(3)).at((0, 12)..(0, 13))
                    ).at((0, 3)..(0, 13)),
                    Expression::literal(Token::Int(4)).at((0, 16)..(0, 17))
                ).at((0, 3)..(0, 17))
            ).at((0, 0)..(0, 17)),
            Expression::literal(Token::Int(5)).at((0, 21)..(0, 22))
        )
    );
}

#[test]
fn test_assignment() {
    // Testing a = 5
    assert_eq!(
        Parser::new("a = 5").expression(Precedence::Assign).unwrap().item,
        Expression::assign(
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            None,
            Spanned::new(Expression::literal(Token::Int(5)), pos(0, 4), pos(0, 5)),
        )
    );
}

#[test]
fn test_equality() {
    // Testing a == b
    assert_eq!(
        Parser::new("a == b").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::EqEq,
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 5), pos(0, 6)),
        )
    );
}

#[test]
fn test_comparison_operators() {
    // Testing a < b
    assert_eq!(
        Parser::new("a < b").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Lt,
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 4), pos(0, 5)),
        )
    );

    // Testing a <= b
    assert_eq!(
        Parser::new("a <= b").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::LtEq,
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 5), pos(0, 6)),
        )
    );

    // Testing a > b
    assert_eq!(
        Parser::new("a > b").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Gt,
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 4), pos(0, 5)),
        )
    );

    // Testing a >= b
    assert_eq!(
        Parser::new("a >= b").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::GtEq,
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 5), pos(0, 6)),
        )
    );
}

#[test]
fn test_operator_precedence_with_comparison() {
    // Testing a + b > c * d
    // * has highest precedence, then +, then >
    assert_eq!(
        Parser::new("a + b > c * d").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::Gt,
            Spanned::new(
                Expression::binary(
                    Token::Plus,
                    Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
                    Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 4), pos(0, 5))
                ),
                pos(0, 0), pos(0, 5)
            ),
            Spanned::new(
                Expression::binary(
                    Token::Star,
                    Spanned::new(Expression::literal(Token::Identifier("c".to_string())), pos(0, 8), pos(0, 9)),
                    Spanned::new(Expression::literal(Token::Identifier("d".to_string())), pos(0, 12), pos(0, 13))
                ),
                pos(0, 8), pos(0, 13)
            )
        )
    );
}

#[test]
fn test_equality_and_comparison_precedence() {
    // Testing a == b > c
    // > has higher precedence than ==
    assert_eq!(
        Parser::new("a == b > c").expression(Precedence::Assign).unwrap().item,
        Expression::binary(
            Token::EqEq,
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            Spanned::new(
                Expression::binary(
                    Token::Gt,
                    Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 5), pos(0, 6)),
                    Spanned::new(Expression::literal(Token::Identifier("c".to_string())), pos(0, 9), pos(0, 10))
                ),
                pos(0, 5), pos(0, 10)
            )
        )
    );
}

#[test]
fn test_assignment_precedence() {
    // Testing a = b + c
    // + has higher precedence than =
    assert_eq!(
        Parser::new("a = b + c").expression(Precedence::Assign).unwrap().item,
        Expression::assign(
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            None,
            Spanned::new(
                Expression::binary(
                    Token::Plus,
                    Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 4), pos(0, 5)),
                    Spanned::new(Expression::literal(Token::Identifier("c".to_string())), pos(0, 8), pos(0, 9))
                ),
                pos(0, 4), pos(0, 9)
            )
        )
    );

    // Testing a = b = c (right-associative)
    // Since = is right-associative, this should parse as a = (b = c)
    assert_eq!(
        Parser::new("a = b = c").expression(Precedence::Assign).unwrap().item,
        Expression::assign(
            Spanned::new(Expression::literal(Token::Identifier("a".to_string())), pos(0, 0), pos(0, 1)),
            None,
            Spanned::new(
                Expression::assign(
                    Spanned::new(Expression::literal(Token::Identifier("b".to_string())), pos(0, 4), pos(0, 5)),
                    None,
                    Spanned::new(Expression::literal(Token::Identifier("c".to_string())), pos(0, 8), pos(0, 9))
                ),
                pos(0, 4), pos(0, 9)
            )
        )
    );
}

#[test]
fn test_infix_err() {
    let res = Parser::new("1 2 3").expression(Precedence::Assign);
    assert_eq!(res.unwrap_err(), Spanned::new(ParseError::ExpectedOperator, pos(0, 2), pos(0, 3)));
}

#[test]
fn test_multiple_exprs() {
    // Can parse multiple expressions separated by newline
    let res = Parser::parse("1 + 2\n3 + 4").unwrap();
    match res.item {
        Expression::Block(exprs) => assert_eq!(exprs.len(), 2),
        _ => panic!("Expected a block")
    };
    

    // Can recover after an error, and report all errors
    let res = Parser::parse("1 foo 2\n3 + 4\n * 2");
    let errs = res.unwrap_err();
    // println!("{:?}", errs);
    assert_eq!(errs.len(), 2);
}

#[test]
fn test_synchronize_lex_err() {
    // on the first unexpected char, we skip up to the first newline,
    // then keep parsing, parse a valid expr `3 + 4`, then hit another unexpected char
    // and we want to report both, so the user could fix both at once before recompiling
    let res = Parser::parse("1 2 ß bar\n3 + 4\n∂");
    assert_eq!(res.unwrap_err().len(), 2);
}

#[test]
fn test_basic_arrow_function() {
    // Testing x -> x + 1
    assert_eq!(
        Parser::new("x -> x + 1").expression(Precedence::Assign).unwrap().item,
        Expression::function(
            vec![Parameter { name: "x".to_string(), ty: None }],
            Expression::binary(
                Token::Plus,
                Expression::literal(Token::Identifier("x".to_string())).at((0, 5)..(0, 6)),
                Expression::literal(Token::Int(1)).at((0, 9)..(0, 10))
            ).at((0, 5)..(0, 10))
        )
    );
}

#[test]
fn test_arrow_function_with_non_identifier_argument() {
    // Testing 1 -> x, should return an error as argument must be an identifier
    let result = Parser::new("1 -> x").expression(Precedence::Assign);
    match &result.as_ref().unwrap_err().item {
        ParseError::Other(msg) => {
            assert!(msg.contains("Expected identifier for argument"));
        },
        _ => panic!("Expected ParseError::Other, got {:?}", result),
    }
}

#[test]
fn test_arrow_function_chaining() {
    // Testing x -> y -> x + y, arrow functions should be right-associative
    assert_eq!(
        Parser::new("x -> y -> x + y").expression(Precedence::Assign).unwrap().item,
        Expression::function(
            vec![Parameter { name: "x".to_string(), ty: None }],
            Expression::function(
                vec![Parameter { name: "y".to_string(), ty: None }],
                Expression::binary(
                    Token::Plus,
                    Expression::literal(Token::Identifier("x".to_string())).at((0, 10)..(0, 11)),
                    Expression::literal(Token::Identifier("y".to_string())).at((0, 14)..(0, 15))
                ).at((0, 10)..(0, 15))
            ).at((0, 5)..(0, 15))
        )
    );
}

#[test]
fn test_arrow_function_with_assignment() {
    // Testing inc = x -> x + 1, should parse as inc = (x -> x + 1)
    assert_eq!(
        Parser::new("inc = x -> x + 1").expression(Precedence::Assign).unwrap().item,
        Expression::assign(
            Expression::literal(Token::Identifier("inc".to_string())).at((0, 0)..(0, 3)),
            None,
            Expression::function(
               vec![Parameter { name: "x".to_string(), ty: None }],
                Expression::binary(
                    Token::Plus,
                    Expression::literal(Token::Identifier("x".to_string())).at((0, 11)..(0, 12)),
                    Expression::literal(Token::Int(1)).at((0, 15)..(0, 16))
                ).at((0, 11)..(0, 16))
            ).at((0, 6)..(0, 16))
        )
    );
}

#[test]
fn test_basic_call_expression() {
    // Testing f(x)
    assert_eq!(
        Parser::new("f(x)").expression(Precedence::Assign).unwrap(),
        Expression::call(
            Expression::literal(Token::Identifier("f".to_string())).at((0, 0)..(0, 1)),
            vec![Expression::literal(Token::Identifier("x".to_string())).at((0, 2)..(0, 3))]
        ).at((0, 0) .. (0, 4))
    );
}

#[test]
fn test_call_with_literal_argument() {
    // Testing f(123)
    assert_eq!(
        Parser::new("f(123)").expression(Precedence::Assign).unwrap().item,
        Expression::call(
            Expression::literal(Token::Identifier("f".to_string())).at((0, 0)..(0, 1)),
            vec![Expression::literal(Token::Int(123)).at((0, 2)..(0, 5))]
        )
    );
}

#[test]
fn test_call_with_expression_argument() {
    // Testing f(x + 1)
    assert_eq!(
        Parser::new("f(x + 1)").expression(Precedence::Assign).unwrap().item,
        Expression::call(
            Expression::literal(Token::Identifier("f".to_string())).at((0, 0)..(0, 1)),
            vec![Expression::binary(
                Token::Plus,
                Expression::literal(Token::Identifier("x".to_string())).at((0, 2)..(0, 3)),
                Expression::literal(Token::Int(1)).at((0, 6)..(0, 7))
            ).at((0, 2)..(0, 7))]
        )
    );
}

#[test]
fn test_call_as_argument_to_another_call() {
    // Testing f(g(x))
    assert_eq!(
        Parser::new("f(g(x))").expression(Precedence::Assign).unwrap().item,
        Expression::call(
            Expression::literal(Token::Identifier("f".to_string())).at((0, 0)..(0, 1)),
            vec![
                Expression::call(
                Expression::literal(Token::Identifier("g".to_string())).at((0, 2)..(0, 3)),
                vec![Expression::literal(Token::Identifier("x".to_string())).at((0, 4)..(0, 5))]
                ).at((0, 2)..(0, 6))
                ]
            )
        );
}

#[test]
fn test_call_on_result_of_call() {
    // Testing f(x)(y)
    assert_eq!(
        Parser::new("f(x)(y)").expression(Precedence::Assign).unwrap().item,
        Expression::call(
            Expression::call(
                Expression::literal(Token::Identifier("f".to_string())).at((0, 0)..(0, 1)),
                vec![Expression::literal(Token::Identifier("x".to_string())).at((0, 2)..(0, 3))]
            ).at((0, 0)..(0, 4)),
            vec![Expression::literal(Token::Identifier("y".to_string())).at((0, 5)..(0, 6))]
        )
    );
}

#[test]
fn test_arrow_function_returning_call() {
    // Testing x -> f(x)
    assert_eq!(
        Parser::new("x -> f(x)").expression(Precedence::Assign).unwrap().item,
        Expression::function(
            vec![Parameter { name: "x".to_string(), ty: None }],
            Expression::call(
                Expression::literal(Token::Identifier("f".to_string())).at((0, 5)..(0, 6)),
                vec![Expression::literal(Token::Identifier("x".to_string())).at((0, 7)..(0, 8))]
            ).at((0, 5)..(0, 9))
        )
    );
}

#[test]
fn test_immediately_invoked_arrow_expression() {
    // Testing (x -> x + 1)(1), where an arrow function is defined and immediately called
    assert_eq!(
        Parser::new("(x -> x + 1)(1)").expression(Precedence::Assign).unwrap().item,
        Expression::call(
            Expression::function(
                vec![Parameter { name: "x".to_string(), ty: None }],
                Expression::binary(
                    Token::Plus,
                    Expression::literal(Token::Identifier("x".to_string())).at((0, 6)..(0, 7)),
                    Expression::literal(Token::Int(1)).at((0, 10)..(0, 11))
                ).at((0, 6)..(0, 11))
            ).at((0, 1)..(0, 11)),
            vec![Expression::literal(Token::Int(1)).at((0, 13)..(0, 14))]
        )
    );
}

#[test]
fn test_empty_tuple() {
    // Testing []
    let parsed_expr = Parser::new("[]").expression(Precedence::Assign).unwrap();
    assert_eq!(parsed_expr.item, Expression::Tuple(vec![]));
    assert_eq!(parsed_expr.span.start, pos(0,0));
    assert_eq!(parsed_expr.span.end, pos(0,2));
}

#[test]
fn test_simple_tuple_single_element() {
    // Testing [1]
    let parsed_expr = Parser::new("[1]").expression(Precedence::Assign).unwrap();
    assert_eq!(
        parsed_expr.item,
        Expression::Tuple(vec![
            Expression::literal(Token::Int(1)).at((0,1)..(0,2)),
        ])
    );
    assert_eq!(parsed_expr.span.start, pos(0,0));
    assert_eq!(parsed_expr.span.end, pos(0,3));
}

#[test]
fn test_simple_tuple_multiple_elements() {
    // Testing [1, "hello", foo]
    let parsed_expr = Parser::new("[1, \"hello\", foo]").expression(Precedence::Assign).unwrap();
    assert_eq!(
        parsed_expr.item,
        Expression::Tuple(vec![
            Expression::literal(Token::Int(1)).at((0,1)..(0,2)),
            Expression::literal(Token::String("hello".to_string())).at((0,4)..(0,11)),
            Expression::literal(Token::Identifier("foo".to_string())).at((0,13)..(0,16)),
        ])
    );
}

#[test]
fn test_tuple_with_trailing_comma() {
    // Testing [1, 2,]
    let parsed_expr = Parser::new("[1, 2,]").expression(Precedence::Assign).unwrap();
    assert_eq!(
        parsed_expr.item,
        Expression::Tuple(vec![
            Expression::literal(Token::Int(1)).at((0,1)..(0,2)),
            Expression::literal(Token::Int(2)).at((0,4)..(0,5)),
        ])
    );
}

#[test]
fn test_tuple_with_expressions() {
    // Testing [1 + 2, x * 3]
    let parsed_expr = Parser::new("[1 + 2, x * 3]").expression(Precedence::Assign).unwrap();
    assert_eq!(
        parsed_expr.item,
        Expression::Tuple(vec![
            Expression::binary(
                Token::Plus,
                Expression::literal(Token::Int(1)).at((0,1)..(0,2)),
                Expression::literal(Token::Int(2)).at((0,5)..(0,6))
            ).at((0,1)..(0,6)), // Span for 1 + 2
            Expression::binary(
                Token::Star,
                Expression::literal(Token::Identifier("x".to_string())).at((0,8)..(0,9)),
                Expression::literal(Token::Int(3)).at((0,12)..(0,13))
            ).at((0,8)..(0,13)), // Span for x * 3
        ])
    );
}

#[test]
fn test_error_unclosed_tuple() {
    // Testing [1, 2
    let result = Parser::new("[1, 2").expression(Precedence::Assign);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().item,
        ParseError::ExpectedButFound(Token::RightBracket, Token::EOF)
    );
}

#[test]
fn test_error_missing_comma_in_tuple() {
    // Testing [1 2]
    // The Pratt parser's expression loop tries to use `2` as an infix operator and
    // produces an ExpectedOperator error before expression_list checks the separator.
    let result = Parser::parse("[1 2]");
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err()[0].item,
        ParseError::ExpectedOperator
    );
}