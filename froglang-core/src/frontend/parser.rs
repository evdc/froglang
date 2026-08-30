// src/parser.rs

use crate::frontend::grammar::Grammar;
use crate::frontend::lexer::{Lexer, LexerError};
use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::expression::Expression;

// remember, errors don't have to track the token/expr they found instead of what they expected;
// they come with a Span, which accomplishes the same thing
#[derive(Debug, PartialEq, Clone)]
pub enum ParseError {
    ExpectedButFound(Token, Token),
    ExpectedExpression, // aka no prefix rule
    ExpectedOperator,   // aka no infix rule
    ExpectedIdentifier,
    LexError(LexerError),
    /// Emitted when a function type appears in annotation position without parentheses,
    /// e.g. `f : Int -> Int` or `let f: Int -> Int = ...`.
    /// Use parentheses: `f : (Int -> Int)` or `let f: (Int -> Int) = ...`.
    FunctionTypeNeedsParens,
    /// Emitted when the left-hand side of `=` isn't a plain identifier,
    /// e.g. `1 = 2` or `(a + b) = 3`.
    InvalidAssignmentTarget,
    Other(String)
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::ExpectedButFound(expected, found) =>
                write!(f, "expected {}, found {}", expected, found),
            ParseError::ExpectedExpression => write!(f, "expected an expression"),
            ParseError::ExpectedOperator    => write!(f, "expected an operator"),
            ParseError::ExpectedIdentifier  => write!(f, "expected an identifier"),
            ParseError::LexError(e)         => write!(f, "{}", e),
            ParseError::FunctionTypeNeedsParens =>
                write!(f, "a function type needs parentheses, e.g. `(A -> B)`"),
            ParseError::InvalidAssignmentTarget =>
                write!(f, "invalid assignment target"),
            ParseError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

// similar pattern to LexResult
pub type ParseResult = Result<Spanned<Expression>, Spanned<ParseError>>;


#[repr(u8)]
#[derive(Debug, PartialEq, PartialOrd, Clone, Copy)]
pub enum Precedence {
    None = 0,
    Assign,
    TypeAnnotation,
    /// `x catch y` — a new rung between `TypeAnnotation` and `Range`: it
    /// binds looser than `Range`/`Or`/`And`/etc. but tighter than `Assign`,
    /// so `let x = f() catch 0` parses `f() catch 0` as the let's value.
    Catch,
    Range,
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
            Precedence::TypeAnnotation => Precedence::Catch,
            Precedence::Catch => Precedence::Range,
            Precedence::Range => Precedence::Or,
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
        let mut errors = Vec::new();
        // If the very first token is a lex error, record it and seed EOF so
        // parsing can proceed (and immediately halt) instead of panicking.
        let current_token = match lexer.next_token() {
            Ok(t) => t,
            Err(err) => {
                let span = err.span;
                errors.push(err.map(ParseError::LexError));
                Spanned::new(Token::EOF, span.start, span.end)
            }
        };
        Parser {
            lexer,
            current_token,
            errors,
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
            // Skip blank lines (including comment-only lines, which emit a bare Newline)
            while self.check(&Token::Newline) {
                let _ = self.advance();
            }
            if self.check(&Token::EOF) { break; }
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

    /// A comma-(or other-`separator`-)delimited expression list, bounded by
    /// an explicit `terminator` token (e.g. `(...)`'s `)`, `[...]`'s `]`).
    /// Because the terminator is unambiguous, newlines around elements are
    /// always safe to skip here — unlike top-level statements, nothing
    /// inside an open paren/bracket could be mistaken for the start of a
    /// new statement — so this list (and therefore any call args, struct
    /// constructor args, and list/tuple literals) may freely span multiple
    /// lines.
    pub fn expression_list(&mut self, separator: &Token, terminator: &Token) -> Vec<Spanned<Expression>> {
        let mut exprs = Vec::new();
        self.skip_newlines();

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

            self.skip_newlines();

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
            self.skip_newlines();
        }
        exprs
    }

    pub fn expression(&mut self, precedence: Precedence) -> ParseResult {
        let mut token = self.advance()?;

        let rule = Grammar::get_parse_rule(&token);
        let prefix = rule.prefix.unwrap_or(Grammar::prefix_error);
        let mut left = prefix(self, token)?;

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
                    if matches!(t.item, Token::EOF | Token::Newline | Token::Comma) {
                        self.current_token = t;
                        break;
                    }
                },
                Err(err) => self.errors.push(err.map(ParseError::LexError)),
            }
        }
    }

    pub fn advance(&mut self) -> Result<Spanned<Token>, Spanned<ParseError>> {
        // we can skip the clone by doing a mem::replace with a dummy token perhaps
        // *** If this fn encounters a lexer error, it should skip to the next valid *token*
        // but not to the *end of the expression* aka synchronize. do that at higher level!
        let prev = self.current_token.clone();
        // n.b. we do NOT call synchronize here! just report the error and let
        // the caller decide.
        self.current_token = self.lexer.next_token().map_err(|e| e.map(ParseError::LexError))?;
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
            Token::Identifier(_) => Ok(found),
            _ => {
                let err = found.map(|_| ParseError::ExpectedIdentifier);
                self.errors.push(err.clone());
                Err(err)
            }
        }
    }
 
    #[inline]
    pub fn check(&mut self, expected: &Token) -> bool {
        self.current_token.item == *expected
    }

    pub fn skip_newlines(&mut self) {
        while self.check(&Token::Newline) {
            let _ = self.advance();
        }
    }

    /// Both statement separators, as `Grammar::block_expr`'s own loop treats
    /// them — for constructs with a brace-delimited list of declarations
    /// (`Grammar::trait_decl`) that need the same tolerance without
    /// `block_expr`'s expression semantics.
    pub fn skip_newlines_and_semicolons(&mut self) {
        while self.check(&Token::Newline) || self.check(&Token::Semicolon) {
            let _ = self.advance();
        }
    }

    /// Snapshot parser state (current token + lexer position) for a later
    /// `restore` — a general one-token(-or-more)-of-lookahead-with-rollback
    /// primitive, e.g. for `Grammar::field_list`'s "was that identifier a
    /// field name or the start of a bare positional type?" check.
    pub fn snapshot(&self) -> (Lexer<'a>, Spanned<Token>) {
        (self.lexer.clone(), self.current_token.clone())
    }

    pub fn restore(&mut self, snapshot: (Lexer<'a>, Spanned<Token>)) {
        self.lexer = snapshot.0;
        self.current_token = snapshot.1;
    }

    /// Look past any newlines to see whether `expected` follows, without
    /// committing to skipping them: if it does, the newlines (and any
    /// leading whitespace-only lines) are consumed and this returns `true`;
    /// if it doesn't, parser state is rolled back to exactly where it
    /// started (still sitting on the first `Newline`) and this returns
    /// `false`. Used where a newline is ambiguous between "this statement
    /// continues on the next line" and "this statement just ended" — e.g. a
    /// `data ... is` variant list, where a *leading* `|` on the next line
    /// should continue the list, but any other token means the declaration
    /// is over and the newline is an ordinary statement separator that must
    /// be left for the caller to consume.
    pub fn peek_past_newlines_is(&mut self, expected: &Token) -> bool {
        if !self.check(&Token::Newline) {
            return self.check(expected);
        }
        let saved_lexer = self.lexer.clone();
        let saved_token = self.current_token.clone();
        let saved_errors_len = self.errors.len();
        self.skip_newlines();
        if self.check(expected) {
            true
        } else {
            self.lexer = saved_lexer;
            self.current_token = saved_token;
            self.errors.truncate(saved_errors_len);
            false
        }
    }
}
