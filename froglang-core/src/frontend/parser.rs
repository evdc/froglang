// src/parser.rs

use crate::frontend::grammar::Grammar;
use crate::frontend::lexer::{Lexer, LexerError};
use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::expression::Expression;

// remember, errors don't have to track the token/expr they found instead of what they expected;
// they come with a Span, which accomplishes the same thingß
#[derive(Debug, PartialEq, Clone)]
pub enum ParseError {
    ExpectedButFound(Token, Token),
    ExpectedExpression, // aka no prefix rule
    ExpectedOperator,   // aka no infix rule
    ExpectedIdentifier,
    LexError(LexerError),
    Other(String)
}

// similar pattern to LexResult
pub type ParseResult = Result<Spanned<Expression>, Spanned<ParseError>>;


#[repr(u8)]
#[derive(Debug, PartialEq, PartialOrd, Clone, Copy)]
pub enum Precedence {
    None = 0,
    Assign,
    TypeAnnotation,
    Or,
    And,
    Equality,
    Comparison,
    Term,
    Factor,
    Unary,
    Call,
    Primary
}

impl Precedence {
    pub fn next(self) -> Self {
        match self {
            Precedence::None => Precedence::Assign,
            Precedence::Assign => Precedence::TypeAnnotation,
            Precedence::TypeAnnotation => Precedence::Or,
            Precedence::Or => Precedence::And,
            Precedence::And => Precedence::Equality,
            Precedence::Equality => Precedence::Comparison,
            Precedence::Comparison => Precedence::Term,
            Precedence::Term => Precedence::Factor,
            Precedence::Factor => Precedence::Unary,
            Precedence::Unary => Precedence::Call,
            Precedence::Call => Precedence::Primary,
            Precedence::Primary => Precedence::Primary,
        }
    }
}

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    pub current_token: Spanned<Token>,
    errors: Vec<Spanned<ParseError>>,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Self {
        let mut lexer = Lexer::new(input);
        // PANICS on the edge case that the first token is a lex error
        let current_token = lexer.next_token().unwrap();
        //let zero_pos = Position { line: 0, col: 0 };
        //let current_token = Spanned::new(Token::EOF, zero_pos, zero_pos);
        Parser {
            lexer,
            current_token,
            errors: Vec::new(),
        }
    }

    pub fn parse(input: &'a str) -> Result<Spanned<Expression>, Vec<Spanned<ParseError>>> {
        let mut p = Parser::new(input);
        let block = p.block().unwrap();
        if p.errors.is_empty() {
            Ok(block)
        } else {
            Err(p.errors)
        }
    }

    pub fn block(&mut self) -> ParseResult {
        let mut exprs = vec![];
        let start = self.current_token.span;
        while !self.check(&Token::EOF) {
            let maybe_expr = self.statement();
            match maybe_expr {
                Ok(expr) => exprs.push(expr),
                Err(err) => {
                    self.errors.push(err);
                    self.synchronize();     // leaves us pointing at a newline
                    let _ = self.advance(); // consume that
                }
            }
        }
        Ok(Spanned::new(Expression::Block(exprs), start.start, self.current_token.span.end))
    }

    pub fn statement(&mut self) -> ParseResult {
        // at the start, only expression statements are supported
        let expr = self.expression(Precedence::Assign)?;
        // the final newline should be able to be elided
        if self.check(&Token::EOF) {
            return Ok(expr);
        }
        self.consume(Token::Newline)?;
        Ok(expr)
    }

    pub fn expression_list(&mut self, separator: &Token, terminator: &Token) -> Vec<Spanned<Expression>> {
        let mut exprs = Vec::new();
    
        // Handle empty list: if current token is already the terminator (e.g., "[]")
        if self.check(terminator) {
            return exprs; // Return empty vector
        }

        loop {
            // Check for terminator or EOF *before* trying to parse an expression.
            if self.check(terminator) || self.check(&Token::EOF) {
                break;
            }
    
            let maybe_expr = self.expression(Precedence::Assign);
            match maybe_expr {
                Ok(expr) => exprs.push(expr),
                Err(err) => {
                    // An error occurred while parsing an expression.
                    self.errors.push(err);
                    // return exprs;
                }
            };

            // After an expression, expect a separator or the terminator.
            if self.check(terminator) {
                // Next token is the terminator (e.g., ']'), which is valid
                break;
            } else if !self.check(separator) {
                // Next token is neither the separator nor the terminator - this is an error!
                let err = self.current_token.clone().map(|t| 
                    ParseError::ExpectedButFound(separator.clone(), t)
                );
                self.errors.push(err);
                // Don't return early - let the outer parsing function handle this error
                return exprs;
            }
    
            // Consume the separator and continue
            let _ = self.advance();
        }
        exprs
    }

    pub fn expression(&mut self, precedence: Precedence) -> ParseResult {        
        let mut token = self.advance()?;

        let rule = Grammar::get_parse_rule(&token);
        let mut left = (rule.prefix)(self, token)?;

        loop {
            let rule = Grammar::get_parse_rule(&self.current_token);
            if precedence > rule.precedence { break; }
            token = self.advance()?;
            left = (rule.infix)(self, token, left, rule.precedence)?;
        }

        Ok(left)
    }

    fn synchronize(&mut self) {
        // After an error, advance to the next statement or scope boundary, ignoring tokens/errors along the way,
        // then resume parsing.
        loop {
            match self.lexer.next_token() {
                Ok(t) => {
                    match t.item {
                        Token::EOF | Token::Newline | Token::Comma => { 
                            self.current_token = t;
                            break; 
                        },
                        _ => (),
                    };
                },
                Err(err) => { 
                    let new_err = err.map(|e| ParseError::LexError(e));
                    self.errors.push(new_err);
                }
            }
        }
    }

    pub fn advance(&mut self) -> Result<Spanned<Token>, Spanned<ParseError>> {
        // we can skip the clone by doing a mem::replace with a dummy token perhaps
        // *** If this fn encounters a lexer error, it should skip to the next valid *token*
        // but not to the *end of the expression* aka synchronize. do that at higher level!
        let prev = self.current_token.clone();
        loop {
            // println!("token: {}", self.current_token);
            match self.lexer.next_token() {
                Ok(t) => {
                    self.current_token = t;
                    break;
                },
                Err(err) => {
                    // n.b. we do NOT call synchronize here! just log the error and move on
                    let new_err = err.map(|e| ParseError::LexError(e));
                    return Err(new_err)
                }
            };
        };
        Ok(prev)
    }

    pub fn consume(&mut self, expected: Token) -> Result<Spanned<Token>, Spanned<ParseError>> {
        let found = self.advance()?;
        if found.item == expected {
            Ok(found)
        } else {
            let err = found.map(|t| ParseError::ExpectedButFound(expected, t));
            self.errors.push(err.clone());
            Err(err)
        }
    }

    pub fn identifier(&mut self) -> Result<Spanned<Token>, Spanned<ParseError>> {
        // this is separate from consume so we don't have to construct a dummy String to pass in
        let found = self.advance()?;
        match found.item {
            Token::Identifier(_) => return Ok(found),
            _ => {
                let err = found.map(|_| ParseError::ExpectedIdentifier);
                self.errors.push(err.clone());
                return Err(err);
            }
        };
    }
 
    #[inline]
    pub fn check(&mut self, expected: &Token) -> bool {
        self.current_token.item == *expected
    }
}
