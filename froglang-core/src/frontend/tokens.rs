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

    /// The singleton `None` value — see `Type::None`'s doc comment. Needed
    /// as an explicit literal so a `T?` (`T | None`)-typed expression can
    /// actually be constructed (`func maybe(): Int? = none`), not just
    /// arise structurally (a bare `return`, a `for` loop's own type).
    #[prefix(Grammar::literal)]
    #[lex("none")]
    None,

    /// The two non-finite floats, as lowercase keyword literals parallel to
    /// `none`/`true`/`false`. `1.0/0.0` has always *printed* `inf`, with no
    /// way to write it back — a hole in the notation (plans/DATA.md stage
    /// 2). `-inf` falls out of unary minus, so it needs no token of its own.
    #[prefix(Grammar::literal)]
    #[lex("inf")]
    Inf,
    #[prefix(Grammar::literal)]
    #[lex("nan")]
    Nan,

    #[infix(Grammar::binary, Precedence::Term)]
    #[lex("+")]
    Plus,

    #[prefix(Grammar::unary)]
    #[infix(Grammar::binary, Precedence::Term)]
    #[lex("-")]
    Minus,

    #[prefix(Grammar::unary)]
    #[infix(Grammar::not_in, Precedence::Comparison)]
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

    /// `mut name = expr` — a fresh, reassignable declaration — or `mut
    /// name` marking a call argument as the target of a `mut` parameter
    /// (`bump(mut a)`). `Grammar::mut_prefix` disambiguates by what
    /// follows the identifier — see `MUTABILITY.md`.
    #[lex("mut")]
    #[prefix(Grammar::mut_prefix, Precedence::Assign)]
    Mut,

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

    #[prefix(Grammar::return_expr)]
    #[lex("return")]
    Return,

    #[prefix(Grammar::for_expr)]
    #[lex("for")]
    For,
    #[infix(Grammar::binary, Precedence::Comparison)]
    #[lex("in")]
    In,
    #[lex("do")]
    Do,

    #[prefix(Grammar::data_decl)]
    #[lex("data")]
    Data,

    /// `annotation name(field: Type = default, ...)` — `plans/DATA.md`
    /// Stage 6. Only ever legal in prefix (statement-start) position, like
    /// `data`/`trait`.
    #[prefix(Grammar::annotation_decl)]
    #[lex("annotation")]
    Annotation,

    #[prefix(Grammar::error_decl)]
    #[lex("error")]
    Error,

    #[prefix(Grammar::trait_decl)]
    #[lex("trait")]
    Trait,

    #[prefix(Grammar::impl_decl)]
    #[lex("provides")]
    Provides,

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
    /// Postfix `e!` — panic-on-error (`ERRORS.md` Phase 5). An infix rule
    /// that ignores its right operand, exactly like `?`/`(`/`[` below.
    #[infix(Grammar::postfix_unwrap, Precedence::Call)]
    #[lex("!")]
    Exclamation,
    /// Postfix `e?` — error propagation (`ERRORS.md` Phase 5). The *type*
    /// grammar's own `T?` (`Grammar::type_expr`) is hand-written and does
    /// not consult this parse-rule table, so the two spellings don't
    /// conflict despite sharing a token.
    #[infix(Grammar::postfix_try, Precedence::Call)]
    #[lex("?")]
    Question,
    #[lex(";")]
    Semicolon,

    /// `#name(...)` — annotation sigil, `plans/DATA.md` Stage 6. No prefix
    /// or infix rule of its own: it's only ever consumed explicitly, by
    /// `Grammar::annotation_use`, from a handful of call sites that check
    /// for it before falling into ordinary expression parsing (never as
    /// part of the general Pratt table).
    #[lex("#")]
    Hash,

    /// `x catch y` / `x catch [e] -> body` — see `Grammar::catch_expr`.
    #[infix(Grammar::catch_expr, Precedence::Catch)]
    #[lex("catch")]
    Catch,

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
            Token::Newline => writeln!(f),
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
