use std::iter::Peekable;
use std::str::Chars;

use crate::frontend::tokens::{Token, Position, Spanned};

// The spanned is redundant, but having the outer type still be a Result makes handling much easier.
type LexResult = Result<Spanned<Token>, Spanned<LexerError>>;

#[inline]
fn is_letter(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

#[inline]
fn is_name_char(ch: char) -> bool {
    is_letter(ch) || ch.is_numeric()
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum LexerError {
    UnexpectedCharacter,
    UnterminatedString,
    InvalidNumber,
    /// An escape the lexer has no meaning for. This used to silently keep
    /// the escaped character (`"\q"` was `"q"`), which made a wrong `repr`
    /// unobservable — see `crate::notation`.
    InvalidEscape,
    /// `\u{...}` that isn't a hex scalar value froglang can hold: empty, too
    /// long, unterminated, or a surrogate.
    InvalidUnicodeEscape,
}

impl std::fmt::Display for LexerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LexerError::UnexpectedCharacter => write!(f, "unexpected character"),
            LexerError::UnterminatedString  => write!(f, "unterminated string literal"),
            LexerError::InvalidNumber       => write!(f, "invalid number literal"),
            LexerError::InvalidEscape       => write!(f, "unknown escape sequence in string literal (known: \\n \\t \\r \\\\ \\\" \\0 \\u{{...}})"),
            LexerError::InvalidUnicodeEscape => write!(f, "invalid unicode escape — expected \\u{{...}} with 1-6 hex digits"),
        }
    }
}

#[derive(Clone)]
pub struct Lexer<'a> {
    input: Peekable<Chars<'a>>,
    current_line: u32,
    pub current_col: u32
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Lexer { input: input.chars().peekable(), current_line: 0, current_col: 0 }
    }

    pub fn next_token(&mut self) -> LexResult {
        self.skip_whitespace();
        let start = self.current_pos();

        let token = match self.advance() {
            Some('\n') => {
                // A run of any number of newlines gets collapsed into a single newline token.
                self.current_line += 1;
                self.current_col = 0;
                while self.input.peek() == Some(&'\n') {
                    self.input.next();
                    self.current_line += 1;
                }
                Token::Newline
            }
            Some(c) => match c {
                '"' => Token::String(self.read_string().map_err(|e| self.error_at(e, start))?),
                '0'..='9' => self.read_number(c).map_err(|e| self.error_at(e, start))?,
                '/' => if let Some('/') = self.input.peek() {
                    self.input.next(); // consume second '/'
                    while let Some(&c) = self.input.peek() {
                        if c == '\n' { break; }
                        self.input.next();
                    }
                    return self.next_token(); // skip comment, return next real token
                } else {
                    Token::Slash
                },

                'a'..='z' | 'A'..='Z' | '_' => self.read_name(c),
                _ => match self.read_operator(c) {
                    Some(tok) => tok,
                    None => return Err(self.error_at(LexerError::UnexpectedCharacter, start))
                }
            },
            None => Token::EOF,
        };
        Ok(Spanned::new(token, start, self.current_pos()))
    }

    fn error_at(&self, err: LexerError, start: Position) -> Spanned<LexerError> {
        Spanned::new(err, start, self.current_pos())
    }

    pub fn collect_errors(&mut self) -> (Vec<Spanned<Token>>, Vec<Spanned<LexerError>>) {
        let mut tokens = Vec::new();
        let mut errors = Vec::new();
        loop {
            let result = self.next_token();
            match result {
                Ok(token) => {
                    if token.item == Token::EOF {
                        break;
                    }
                    tokens.push(token);
                },
                Err(err) => errors.push(err)
            }
        };
        (tokens, errors)
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.input.next();
        self.current_col += 1;
        c
    }

    #[inline]
    pub fn current_pos(&self) -> Position {
        Position { line: self.current_line, col: self.current_col }
    }

    fn skip_whitespace(&mut self) {
        // Newlines have semantic meaning: they can separate statements. So we don't skip them here.
        while match self.input.peek() {
            Some(ch) => ch.is_whitespace() && ch != &'\n',
            _ => false,
        } {
            self.advance();
        };
    }

    fn read_name(&mut self, ch: char) -> Token {
        let mut name = String::new();
        name.push(ch);
        while let Some(c) = self.input.peek() {
            if is_name_char(*c) {
                name.push(self.advance().unwrap())      // safe, because we just peeked it
            } else {
                break;
            }
        }
        Token::from_keyword(&name).unwrap_or(Token::Identifier(name))
    }
    fn read_operator(&mut self, ch: char) -> Option<Token> {
        let single = ch.to_string();
        // Try to form a two-character operator
        if let Some(&next) = self.input.peek() {
            if !is_name_char(next) && !next.is_whitespace() && next != '\n' {
                let two = format!("{}{}", ch, next);
                if Token::from_operator(&two).is_some() {
                    self.advance();
                    return Token::from_operator(&two);
                }
            }
        }
        Token::from_operator(&single)
    }

    fn read_number(&mut self, ch: char) -> Result<Token, LexerError> {
        let mut n = String::new();
        n.push(ch);
        let mut float = false;
        while let Some(c) = self.input.peek() {
            // Allowing . lets us parse floats.
            // If invalid e.g. `12.34.56` punt to rust's String.parse<f64> and let it error
            if c.is_numeric() {
                n.push(self.advance().unwrap())      // safe, because we just peeked it
            } else if c == &'.' {
                // Only a decimal point if followed by a digit — `1..5` is
                // `Int(1)`, `DotDot`, `Int(5)`, not a malformed float. A
                // one-token lookahead (via a cheap iterator clone) is enough
                // to tell the two apart without disturbing `self.input`.
                let mut lookahead = self.input.clone();
                lookahead.next();
                if !matches!(lookahead.peek(), Some(d) if d.is_numeric()) {
                    break;
                }
                float = true;
                n.push(self.advance().unwrap());
            } else if c == &'e' || c == &'E' {
                // An exponent, but only when one actually follows: `e` is a
                // name character, so `1e100` and `1e-3` are one float while
                // `1 else x` and a hypothetical `1eggs` must keep their `e`
                // for `read_name`. Two characters of lookahead, since the
                // sign may sit between the `e` and the first digit.
                let mut lookahead = self.input.clone();
                lookahead.next();
                let signed = matches!(lookahead.peek(), Some('+') | Some('-'));
                if signed { lookahead.next(); }
                if !matches!(lookahead.peek(), Some(d) if d.is_numeric()) {
                    break;
                }
                float = true;
                n.push(self.advance().unwrap());             // 'e'
                if signed { n.push(self.advance().unwrap()); }
                while matches!(self.input.peek(), Some(d) if d.is_numeric()) {
                    n.push(self.advance().unwrap());
                }
                break;      // the exponent ends the literal
            } else {
                break;
            }
        }
        if float {
            let f = n.parse::<f64>().map_err(|_err| LexerError::InvalidNumber)?;
            return Ok(Token::Float(f));
        } else {
            let i = n.parse::<i64>().map_err(|_err| LexerError::InvalidNumber)?;
            return Ok(Token::Int(i));
        }
    }

    /// The body of a `\u{...}` escape, with the `\u` already consumed. 1–6
    /// hex digits, the range `char` itself accepts — a surrogate or a value
    /// past `char::MAX` is rejected rather than replaced, since a silent
    /// substitution is the failure this escape exists to remove.
    fn read_unicode_escape(&mut self) -> Result<char, LexerError> {
        if self.advance() != Some('{') {
            return Err(LexerError::InvalidUnicodeEscape);
        }
        let mut digits = String::new();
        loop {
            match self.advance() {
                Some('}') => break,
                Some(c) if c.is_ascii_hexdigit() && digits.len() < 6 => digits.push(c),
                Some(_) => return Err(LexerError::InvalidUnicodeEscape),
                None => return Err(LexerError::UnterminatedString),
            }
        }
        u32::from_str_radix(&digits, 16).ok()
            .and_then(char::from_u32)
            .ok_or(LexerError::InvalidUnicodeEscape)
    }

    fn read_string(&mut self) -> Result<String, LexerError> {
        let mut s = String::new();
        // A bad escape doesn't abandon the literal: the scan runs to the
        // closing quote and reports the first error afterwards. Returning
        // early would leave the lexer positioned *inside* the string, so
        // one mistyped escape would cascade into an unterminated-string
        // error plus whatever its remaining characters happened to lex as.
        let mut error: Option<LexerError> = None;
        while let Some(c) = self.input.peek() {
            if *c == '"' {
                self.advance();      // consume the closing "
                return match error { Some(e) => Err(e), None => Ok(s) };
            }
            let ch = self.advance().unwrap();   // safe, just peeked
            if ch == '\\' {
                let escaped = self.advance().ok_or(LexerError::UnterminatedString)?;
                let decoded = match escaped {
                    'n' => Ok('\n'),
                    't' => Ok('\t'),
                    'r' => Ok('\r'),
                    '\\' => Ok('\\'),
                    '"' => Ok('"'),
                    '0' => Ok('\0'),
                    'u' => self.read_unicode_escape(),
                    // An unknown escape used to keep the literal character,
                    // which is the mechanism that made a mis-escaped `repr`
                    // silent (`\u{7}` read back as `u{7}`). It is a lex
                    // error now — see `crate::notation`.
                    _ => Err(LexerError::InvalidEscape),
                };
                match decoded {
                    Ok(c) => s.push(c),
                    // Unterminated input can't be resynced past, so it ends
                    // the scan; a malformed escape only records.
                    Err(LexerError::UnterminatedString) => return Err(LexerError::UnterminatedString),
                    Err(e) => { error.get_or_insert(e); },
                }
            } else {
                s.push(ch);
            }
        };
        // If we exited the while let here, it's because we encountered a None (end of input) before a closing quote
        Err(LexerError::UnterminatedString)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::tokens::Span;

    fn lex_and_collect(input: &str) -> (Vec<Spanned<Token>>, Vec<Spanned<LexerError>>) {
        let mut lexer = Lexer::new(input);
        lexer.collect_errors()
    }

    #[test]
    fn test_identifiers() {
        let input = "foo bar_baz _qux";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Identifier("foo".to_string()),
            Token::Identifier("bar_baz".to_string()),
            Token::Identifier("_qux".to_string()),
        ]);
    }
    #[test]
    fn test_identifier_and_keyword() {
        let input = "if foo then baz else bux";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::If,
            Token::Identifier("foo".to_string()),
            Token::Then,
            Token::Identifier("baz".to_string()),
            Token::Else,
            Token::Identifier("bux".to_string()),
        ]);
    }

    #[test]
    fn test_numbers() {
        let input = "42 3.14 0.123 100.0";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Int(42),
            Token::Float(3.14),
            Token::Float(0.123),
            Token::Float(100.0),
        ]);
    }

    /// plans/DATA.md stage 2: `print(1e100)` emitted a form the lexer
    /// couldn't read back. Note `2e5` is a `Float` even with no decimal
    /// point — an exponent is what makes it one.
    #[test]
    fn test_exponent_literals() {
        let input = "1e100 1E5 1.5e-3 2e+2 0e0";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty(), "{:?}", errors);
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Float(1e100),
            Token::Float(1e5),
            Token::Float(1.5e-3),
            Token::Float(2e2),
            Token::Float(0.0),
        ]);
    }

    /// `e` is a name character, so the exponent scan has to be sure one
    /// really follows before it consumes anything — otherwise a keyword or
    /// identifier butted against a number loses its first letter.
    #[test]
    fn test_e_is_only_an_exponent_when_digits_follow() {
        let (spans, errors) = lex_and_collect("1 else 2 exit");
        assert!(errors.is_empty(), "{:?}", errors);
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Int(1),
            Token::Else,
            Token::Int(2),
            Token::Identifier("exit".to_string()),
        ]);
        // `1e` with nothing usable after it is a number and a name, not an
        // error and not a silently-truncated float.
        let (spans, errors) = lex_and_collect("1e");
        assert!(errors.is_empty(), "{:?}", errors);
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![Token::Int(1), Token::Identifier("e".to_string())]);
    }

    #[test]
    fn test_inf_and_nan_literals() {
        let (spans, errors) = lex_and_collect("inf nan -inf");
        assert!(errors.is_empty(), "{:?}", errors);
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![Token::Inf, Token::Nan, Token::Minus, Token::Inf]);
    }

    #[test]
    fn test_unicode_escape() {
        let (spans, errors) = lex_and_collect(r#""a\u{7}b" "\u{1F438}" "\u{0}""#);
        assert!(errors.is_empty(), "{:?}", errors);
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::String("a\u{7}b".to_string()),
            Token::String("🐸".to_string()),
            Token::String("\0".to_string()),
        ]);
    }

    /// An unknown escape used to keep the escaped character, so `"\q"` was
    /// `"q"` — the mechanism that let a mis-escaped `repr` be silently
    /// wrong. It is an error now.
    #[test]
    fn test_unknown_escape_is_an_error() {
        let (_, errors) = lex_and_collect(r#""a\qb""#);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].item, LexerError::InvalidEscape);
    }

    #[test]
    fn test_malformed_unicode_escape_is_an_error() {
        for input in [r#""\u7}""#, r#""\u{}""#, r#""\u{zz}""#, r#""\u{D800}""#, r#""\u{1234567}""#] {
            let (_, errors) = lex_and_collect(input);
            assert_eq!(errors.len(), 1, "{}", input);
            assert_eq!(errors[0].item, LexerError::InvalidUnicodeEscape, "{}", input);
        }
    }

    #[test]
    fn test_unary_minus() {
        let input = "42 -34";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        assert_eq!(spans[0].item, Token::Int(42));
        assert_eq!(spans[0].span, Span { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 2 } });
        assert_eq!(spans[1].item, Token::Minus);
        assert_eq!(spans[1].span, Span { start: Position { line: 0, col: 3 }, end: Position { line: 0, col: 4 } });
        assert_eq!(spans[2].item, Token::Int(34));
        assert_eq!(spans[2].span, Span { start: Position { line: 0, col: 4 }, end: Position { line: 0, col: 6 } });   
    }

    #[test]
    fn test_strings() {
        let input = r#""hello" "world" "with spaces""#;
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::String("hello".to_string()),
            Token::String("world".to_string()),
            Token::String("with spaces".to_string()),
        ]);
    }

    #[test]
    fn test_comments() {
        let input = "// This is a comment\nfoo // Another comment";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        // Comments are skipped by the lexer; the newline after the first comment is preserved
        assert_eq!(tokens, vec![
            Token::Newline,
            Token::Identifier("foo".to_string()),
        ]);
    }

    #[test]
    fn test_error_unexpected_character() {
        let input = "foo @ bar";
        let (spans, errors) = lex_and_collect(input);
        assert_eq!(spans.len(), 2); // "foo" and "bar"
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].item, LexerError::UnexpectedCharacter);
        println!("{:#?}", errors[0].span);
        assert!(matches!(errors[0].span, Span { start: Position { line: 0, col: 4 }, end: Position { line: 0, col: 5 } }))
    }

    #[test]
    fn test_mixed_operators() {
        // bc of how operators are now implemented, this lexes as <foo> <+-> <bar> not <foo> <+> <-> <bar>
        // this is Fine maybe? just a syntax rule that operators have to be surrounded by whitespace
        let input = "foo+-bar";
        let (spans, errors) = lex_and_collect(input);
        println!("{:?}", spans);
        println!("{:?}", errors);
    }

    #[test]
    fn test_error_unterminated_string() {
        let input = "\"unterminated";
        let (spans, errors) = lex_and_collect(input);
        assert!(spans.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0].item, LexerError::UnterminatedString));
    }

    #[test]
    fn test_error_invalid_number() {
        let input = "123.456.789";
        let (spans, errors) = lex_and_collect(input);
        assert!(spans.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0].item, LexerError::InvalidNumber));
    }

    #[test]
    fn test_multiple_errors() {
        let input = r#"valid_token @ 123.456.789 "unterminated "#;
        let (spans, errors) = lex_and_collect(input);
        println!("{:?}", spans);
        println!("{:?}", errors);
        assert_eq!(spans.len(), 1); // "valid_token"
        assert_eq!(errors.len(), 3);
        assert!(matches!(errors[0].item, LexerError::UnexpectedCharacter));
        assert!(matches!(errors[1].item, LexerError::InvalidNumber));
        assert!(matches!(errors[2].item, LexerError::UnterminatedString));
    }

    #[test]
    fn test_span_information() {
        let input = "fou  bar\nbaz";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        assert_eq!(spans[0].span.start, Position { line: 0, col: 0 });
        assert_eq!(spans[0].span.end, Position { line: 0, col: 3 });
        assert_eq!(spans[1].span.start, Position { line: 0, col: 5 });
        assert_eq!(spans[1].span.end, Position { line: 0, col: 8 });
        assert_eq!(spans[2].item, Token::Newline);
        assert_eq!(spans[3].span.start, Position { line: 1, col: 0 });
        assert_eq!(spans[3].span.end, Position { line: 1, col: 3 });
    }

    #[test]
    fn test_whitespace_handling() {
        let input = "  foo \n\n\n  bar  ";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Identifier("foo".to_string()),
            Token::Newline,
            Token::Identifier("bar".to_string()),
        ]);
    }

    #[test]
    fn test_comparison_operators() {
        let input = "= == < <= > >=";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Assign,
            Token::EqEq,
            Token::Lt,
            Token::LtEq,
            Token::Gt,
            Token::GtEq,
        ]);
    }

    #[test]
    fn test_operators_in_expressions() {
        let input = "a = 5\nb == c\nx < y\np <= q\nr > s\nt >= u";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        let tokens: Vec<_> = spans.into_iter().map(|s| s.item).collect();
        assert_eq!(tokens, vec![
            Token::Identifier("a".to_string()),
            Token::Assign,
            Token::Int(5),
            Token::Newline,
            Token::Identifier("b".to_string()),
            Token::EqEq,
            Token::Identifier("c".to_string()),
            Token::Newline,
            Token::Identifier("x".to_string()),
            Token::Lt,
            Token::Identifier("y".to_string()),
            Token::Newline,
            Token::Identifier("p".to_string()),
            Token::LtEq,
            Token::Identifier("q".to_string()),
            Token::Newline,
            Token::Identifier("r".to_string()),
            Token::Gt,
            Token::Identifier("s".to_string()),
            Token::Newline,
            Token::Identifier("t".to_string()),
            Token::GtEq,
            Token::Identifier("u".to_string()),
        ]);
    }

    #[test]
    fn test_operator_spans() {
        let input = "a = b == c < d <= e > f >= g";
        let (spans, errors) = lex_and_collect(input);
        assert!(errors.is_empty());
        
        // Check specific spans for operators
        assert_eq!(spans[1].span, Span::new((0,2), (0,3))); // "="
        assert_eq!(spans[3].span, Span::new((0,6), (0,8))); // "=="
        assert_eq!(spans[5].span, Span::new((0,11), (0,12))); // "<"
        assert_eq!(spans[7].span, Span::new((0,15), (0,17))); // "<="
        assert_eq!(spans[9].span, Span::new((0,20), (0,21))); // ">"
        assert_eq!(spans[11].span, Span::new((0,24), (0,26))); // ">="
        
        // Check token types match expected operators
        assert_eq!(spans[1].item, Token::Assign);
        assert_eq!(spans[3].item, Token::EqEq);
        assert_eq!(spans[5].item, Token::Lt);
        assert_eq!(spans[7].item, Token::LtEq);
        assert_eq!(spans[9].item, Token::Gt);
        assert_eq!(spans[11].item, Token::GtEq);
    }
}