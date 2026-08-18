use std::fmt;
use std::fmt::Display;

use froglang_macros::{Lex, ParseRules};

use crate::frontend::grammar::{Grammar, ParseRule};
use crate::frontend::parser::Precedence;


#[derive(Debug, PartialEq, Clone)]
#[derive(ParseRules, Lex)]
pub enum Token {
    #[prefix(Grammar::literal)]
    Float(f64),
    #[prefix(Grammar::literal)]
    Int(i64),
    #[prefix(Grammar::literal)]
    String(String),
    #[prefix(Grammar::literal)]
    Identifier(String),

    #[prefix(Grammar::literal)]
    #[lex("true")]
    True,
    #[prefix(Grammar::literal)]
    #[lex("false")]
    False,

    #[infix(Grammar::binary, Precedence::Term)]
    #[lex("+")]
    Plus,

    #[prefix(Grammar::unary)]
    #[infix(Grammar::binary, Precedence::Term)]
    #[lex("-")]
    Minus,

    #[prefix(Grammar::unary)]
    #[lex("not")]
    Not,

    #[infix(Grammar::binary, Precedence::Factor)]
    #[lex("*")]
    Star,

    #[infix(Grammar::binary, Precedence::Factor)]
    #[lex("/")]
    Slash,

    #[lex("let")]
    #[prefix(Grammar::let_binding, Precedence::Assign)]
    Let,

    #[infix(Grammar::assign, Precedence::Assign)]
    #[lex("=")]
    Assign,

    #[infix(Grammar::binary, Precedence::Equality)]
    #[lex("==")]
    EqEq,

    #[infix(Grammar::binary, Precedence::Equality)]
    #[lex("!=")]
    NotEq,

    #[infix(Grammar::binary, Precedence::Comparison)]
    #[lex("<")]
    Lt,

    #[infix(Grammar::binary, Precedence::Comparison)]
    #[lex(">")]
    Gt,

    #[infix(Grammar::binary, Precedence::Comparison)]
    #[lex("<=")]
    LtEq,

    #[infix(Grammar::binary, Precedence::Comparison)]
    #[lex(">=")]
    GtEq,

    #[infix(Grammar::range, Precedence::Range)]
    #[lex("..")]
    DotDot,

    #[infix(Grammar::binary, Precedence::Or)]
    #[lex("or")]
    Or,

    #[infix(Grammar::binary, Precedence::And)]
    #[lex("and")]
    And,

    #[prefix(Grammar::conditional)]
    #[lex("if")]
    If,
    #[lex("then")]
    Then,
    #[lex("else")]
    Else,

    #[prefix(Grammar::func_decl)]
    #[lex("func")]
    Func,

    #[prefix(Grammar::for_expr)]
    #[lex("for")]
    For,
    #[lex("in")]
    In,
    #[lex("do")]
    Do,

    #[prefix(Grammar::data_decl)]
    #[lex("data")]
    Data,

    #[infix(Grammar::is_pattern, Precedence::Comparison)]
    #[lex("is")]
    Is,

    #[prefix(Grammar::match_expr)]
    #[lex("match")]
    Match,

    #[lex("|")]
    Pipe,

    #[prefix(Grammar::import_decl)]
    #[lex("import")]
    Import,

    #[lex("as")]
    As,

    #[infix(Grammar::field_access, Precedence::Call)]
    #[lex(".")]
    Dot,

    #[infix(Grammar::arrow_func, Precedence::Assign)]
    #[lex("->")]
    Arrow,

    #[infix(Grammar::type_annotation, Precedence::Assign)]
    #[lex(":")]
    Colon,

    #[prefix(Grammar::grouping)]
    #[infix(Grammar::call, Precedence::Call)]
    #[lex("(")]
    LeftParen,

    #[prefix(Grammar::tuple)]
    #[infix(Grammar::index, Precedence::Call)]
    #[lex("[")]
    LeftBracket,

    #[prefix(Grammar::block_expr)]
    #[lex("{")]
    LeftBrace,

    // tokens that should not appear in either prefix or infix expr position
    #[lex(")")]
    RightParen,
    #[lex("]")]
    RightBracket,
    #[lex("}")]
    RightBrace,
    #[lex(",")]
    Comma,
    #[lex("!")]
    Exclamation,
    #[lex(";")]
    Semicolon,

    // Special handling by the lexer - don't use lex() attr
    Newline,
    EOF,
}


impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Int(n) => write!(f, "{}", n),
            Token::Float(n) => write!(f, "{}", n),
            Token::String(s) => write!(f, "\"{}\"", s),
            Token::Identifier(i) => write!(f, "{}", i),
            Token::Newline => write!(f, "\n"),
            Token::EOF => write!(f, "<EOF>"),
            Token::Semicolon => write!(f, ";"),
            _ => self.display(f)
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Position {
    pub line: u32,
    pub col: u32
}

impl Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Span {
    pub start: Position,
    pub end: Position
}

impl Span {
    pub fn merge(self, other: Span) -> Span {
        Span { start: self.start, end: other.end }
    }

    pub fn new(start: (u32, u32), end: (u32, u32)) -> Span {
        Span { start: Position { line: start.0, col: start.1 }, end: Position { line: end.0, col: end.1 }}
    }
}

impl Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({} .. {})", self.start, self.end)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct Spanned<T: std::fmt::Debug> {
    pub span: Span,
    pub item: T
}

impl<T: std::fmt::Debug> Spanned<T> {
    pub fn new(item: T, start: Position, end: Position) -> Self {
        Spanned { span: Span { start, end }, item }
    }

    pub fn map<U: std::fmt::Debug, F: FnOnce(T) -> U>(self, op: F) -> Spanned<U> {
        Spanned { span: self.span, item: op(self.item) }
    }

    pub fn to<U: std::fmt::Debug>(self, item: U) -> Spanned<U> {
        Spanned { span: self.span, item }
    }

    // really this one should be "new" but I am too lazy to change it
    pub fn from(item: T, span: Span) -> Self {
        Spanned { span, item }
    }
}

impl<T: std::fmt::Debug> Display for Spanned<T> {
    fn fmt(&self, f:&mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\t{:?}", self.span, self.item)
    }
}
