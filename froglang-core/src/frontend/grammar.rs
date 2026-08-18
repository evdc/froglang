use crate::frontend::{expression::{DataDeclExpr, Expression, ImportExpr, ImportKind, MatchArm, MatchExpr, Parameter, Pattern, VariantDecl}, parser::{ParseError, ParseResult, Parser, Precedence}, tokens::{Span, Spanned, Token}};

pub type PrefixFnType = fn(&mut Parser, Spanned<Token>) -> ParseResult;
pub type InfixFnType = fn(&mut Parser, Spanned<Token>, Spanned<Expression>, Precedence) -> ParseResult;

#[derive(Debug)]
pub struct ParseRule {
    pub prefix: PrefixFnType,
    pub infix: InfixFnType,
    pub precedence: Precedence
}

pub struct Grammar { }

impl Grammar {
    pub fn prefix_error(_parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        Err(Spanned::new(ParseError::ExpectedExpression, token.span.start, token.span.end))
    }

    pub fn infix_error(_parser: &mut Parser, token: Spanned<Token>, _left: Spanned<Expression>, _precedence: Precedence) -> ParseResult {
        Err(Spanned::new(ParseError::ExpectedOperator, token.span.start, token.span.end))
    }

    pub fn literal(_parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        // a literal expression has the same span as its token
        Ok(token.map(|t| Expression::literal(t)))
    }

    pub fn unary(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let expr: Spanned<Expression> = parser.expression(Precedence::Unary)?;
        Ok(Spanned {
            span: token.span.merge(expr.span),
            item: Expression::unary(token.item, expr)
        })
    }

    pub fn binary(parser: &mut Parser, token: Spanned<Token>, left: Spanned<Expression>, precedence: Precedence) -> ParseResult {
        let right = parser.expression(precedence.next())?;
        Ok(Spanned {
            span: left.span.merge(right.span),
            item: Expression::binary(token.item, left, right)
        })
    }
    
    pub fn let_binding(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        // parse a pattern - a name, optional colon&type, then an =, then an expr
        // later(?) add destructuring assignment here
        let name = parser.identifier()?;
        // Parse type annotation at TypeAnnotation precedence so `->` doesn't fire bare;
        // parenthesised function types like `(Int -> Int)` still work via grouping.
        let ty = if parser.check(&Token::Colon) {
            parser.advance()?;
            let ty_expr = parser.expression(Precedence::TypeAnnotation)?;
            // If `->` follows the type annotation, the user wrote `let f: Int -> Int = ...`
            // without parentheses around the function type.
            if parser.check(&Token::Arrow) {
                return Err(ty_expr.to(ParseError::FunctionTypeNeedsParens));
            }
            Some(ty_expr)
        } else { None };
        parser.consume(Token::Assign)?;
        parser.skip_newlines();

        let expr: Spanned<Expression> = parser.expression(Precedence::Assign)?;
        Ok(Spanned {
            span: token.span.merge(expr.span),
            item: Expression::assign(
                name.map(Expression::literal),
                ty,
                expr)
        })
    }

    pub fn assign(parser: &mut Parser, _token: Spanned<Token>, left: Spanned<Expression>, precedence: Precedence) -> ParseResult {
        // Valid targets: a bare identifier (`x = ...`), or a field access
        // whose own target is a bare identifier (`alice.age = ...` — the
        // struct rebind-sugar; see `TypedExprKind::FieldAssign`). Deeper
        // paths (`a.b.c = ...`) and non-identifier bases (`foo().x = ...`)
        // are rejected here — v1 restriction, not a fundamental limit.
        let valid = match &left.item {
            Expression::FieldAccess(fa) => fa.target.item.get_identifier().is_some(),
            other => other.get_identifier().is_some(),
        };
        if !valid {
            return Err(left.to(ParseError::InvalidAssignmentTarget));
        }
        let right = parser.expression(precedence)?;
        Ok(Spanned {
            span: left.span.merge(right.span),
            item: Expression::assign(left, None, right)
        })
    }

    pub fn grouping(parser: &mut Parser, _t: Spanned<Token>) -> ParseResult {
        let expr = parser.expression(Precedence::Assign)?;
        parser.consume(Token::RightParen)?;
        Ok(expr)
    }

    pub fn block_expr(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
        let mut stmts = vec![];
        // skip leading blank lines / semicolons
        while parser.check(&Token::Newline) || parser.check(&Token::Semicolon) {
            let _ = parser.advance();
        }
        while !parser.check(&Token::RightBrace) && !parser.check(&Token::EOF) {
            let expr = parser.expression(Precedence::Assign)?;
            stmts.push(expr);
            // stop if we see `}` next
            if parser.check(&Token::RightBrace) || parser.check(&Token::EOF) { break; }
            // require at least one separator (newline or `;`)
            if !parser.check(&Token::Newline) && !parser.check(&Token::Semicolon) {
                return Err(parser.current_token.clone()
                    .map(|t| ParseError::ExpectedButFound(Token::Newline, t)));
            }
            // consume all consecutive separators
            while parser.check(&Token::Newline) || parser.check(&Token::Semicolon) {
                let _ = parser.advance();
            }
        }
        let closing = parser.consume(Token::RightBrace)?;
        if stmts.is_empty() {
            return Err(Spanned::new(ParseError::ExpectedExpression, t.span.start, closing.span.end));
        }
        if stmts.len() == 1 {
            Ok(stmts.into_iter().next().unwrap())
        } else {
            Ok(Spanned { span: t.span.merge(closing.span), item: Expression::Block(stmts) })
        }
    }

    pub fn conditional(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
        // TODO: we have the opportunity to map each ? to a more specific error here
        // e.g. "Missing conditional expression after `if`"
        let cond = parser.expression(Precedence::Assign)?;
        parser.skip_newlines();
        parser.consume(Token::Then)?;
        parser.skip_newlines();
        let true_branch = parser.expression(Precedence::Assign)?;
        parser.skip_newlines();
        let false_branch = if parser.check(&Token::Else) {
            parser.consume(Token::Else).unwrap(); // just checked it
            parser.skip_newlines();
            Some(parser.expression(Precedence::Assign)?)
        } else {
            None
        };
        let end = match &false_branch {
            Some(expr) => expr.span.end,
            None => true_branch.span.end
        };

        Ok(Spanned::new(
            Expression::conditional(cond, true_branch, false_branch),
            t.span.start,
            end
        ))
    }

    /// Parse a named function declaration: `func name(p1: T1, p2: T2, ...): RetType = body`
    /// Desugars to an assignment: `name = (p1, p2, ...) -> body`
    pub fn func_decl(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let name_tok = parser.identifier()?;
        parser.consume(Token::LeftParen)?;

        let mut params = Vec::new();
        while !parser.check(&Token::RightParen) && !parser.check(&Token::EOF) {
            let param_tok = parser.identifier()?;
            let param_name = match &param_tok.item {
                Token::Identifier(s) => s.clone(),
                _ => unreachable!(),
            };
            let ty = if parser.check(&Token::Colon) {
                parser.advance()?;
                let ty_tok = parser.identifier()?;
                Some(Box::new(ty_tok.map(Expression::literal)))
            } else {
                None
            };
            params.push(Parameter { name: param_name, ty });
            if parser.check(&Token::Comma) {
                parser.advance()?;
            }
        }
        parser.consume(Token::RightParen)?;

        let return_type = if parser.check(&Token::Colon) {
            parser.advance()?;
            let ty_tok = parser.identifier()?;
            Some(ty_tok.map(Expression::literal))
        } else {
            None
        };

        parser.consume(Token::Assign)?;
        parser.skip_newlines();
        let body = parser.expression(Precedence::Assign)?;
        let body_span = body.span;

        let func_expr = Spanned::from(
            Expression::function_with_return(params, body, return_type),
            body_span,
        );
        let name_expr = name_tok.map(Expression::literal);

        Ok(Spanned {
            span: token.span.merge(body_span),
            item: Expression::assign(name_expr, None, func_expr),
        })
    }

    pub fn arrow_func(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let params = match &left.item {
            // `f : Int -> Int` — the annotation absorbed `f : Int`, now `->` fires with
            // `Annotated(f, Int)` as the left-hand side. Tell the user to add parens.
            Expression::Annotated(_) => {
                return Err(left.to(ParseError::FunctionTypeNeedsParens))
            },
            Expression::Literal(lit) => {
                match &lit.token {
                    Token::Identifier(name) => vec![name],
                    _ => {
                        let msg = format!("Expected identifier for argument, got {}", &left.item);
                        return Err(left.to(ParseError::Other(msg)))
                    }
                }
            },
            Expression::Tuple(args) => {
                let mut res = vec![];
                for arg in args {
                    if let Expression::Literal(lit) = &arg.item {
                        match &lit.token {
                            Token::Identifier(name) => res.push(name),
                            _ => {
                                let msg = format!("Expected identifier for argument, got {}", &lit.token);
                                return Err(Spanned::from(ParseError::Other(msg), arg.span))
                            }
                        }
                    } else {
                        let msg = format!("Expected identifier for argument, got {}", &arg.item);
                        return Err(Spanned::from(ParseError::Other(msg), arg.span))
                    }
                }
                res
            },
            _ => {
                let msg = format!("Expected identifier for argument, got {}", &left.item);
                return Err(left.to(ParseError::Other(msg)))
            }
        };
        let params = params.iter().map(|name| Parameter { name: name.to_string(), ty: None }).collect();
        let body = parser.expression(Precedence::Assign)?;
        Ok(Spanned { 
            span: left.span.merge(body.span), 
            item: Expression::function(params, body)
        })
    }

    pub fn type_annotation(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _p: Precedence) -> ParseResult {
        // Parse the type expression at TypeAnnotation precedence (one level above Assign).
        // This prevents `->` (Assign precedence) from firing here, so `f : Int -> Int`
        // is a parse error — the user must parenthesise: `f : (Int -> Int)`.
        // Parenthesised types work because `(` triggers grouping, which parses its
        // interior at Assign level where `->` fires normally.
        let type_expr = parser.expression(Precedence::TypeAnnotation)?;
        Ok(Spanned { span: left.span.merge(type_expr.span), item: Expression::annotated(left, type_expr) })
    }

    pub fn call(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let args = parser.expression_list(&Token::Comma, &Token::RightParen);
        let closing = parser.consume(Token::RightParen)?;
        // Need to extend span by to include the closing paren
        Ok(Spanned { span: left.span.merge(closing.span), item: Expression::call(left, args) })
    }

    pub fn index(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        // Parse each bound one level above `Range` precedence (where `..` is
        // registered) so a bare `..` here is left for us to see and handle
        // directly, rather than being swallowed by the generic `..` infix
        // rule (`Grammar::range`), which requires both operands.
        let start = if parser.check(&Token::DotDot) {
            None
        } else {
            Some(parser.expression(Precedence::Range.next())?)
        };

        if parser.check(&Token::DotDot) {
            parser.advance()?;
            let end = if parser.check(&Token::RightBracket) {
                None
            } else {
                Some(parser.expression(Precedence::Range.next())?)
            };
            let closing = parser.consume(Token::RightBracket)?;
            return Ok(Spanned {
                span: left.span.merge(closing.span),
                item: Expression::slice(left, start, end)
            });
        }

        let index = start.expect("no '..' seen, so the branch above must have parsed an index expression");
        let closing = parser.consume(Token::RightBracket)?;
        Ok(Spanned {
            span: left.span.merge(closing.span),
            item: Expression::index(left, index)
        })
    }

    /// `start..end` used as a standalone expression, e.g. `1..5`. Both
    /// operands are required here (unlike `target[start..end]` slicing,
    /// which allows either to be omitted — see `Grammar::index`) since an
    /// unbounded range has nothing to eagerly materialize into.
    pub fn range(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, precedence: Precedence) -> ParseResult {
        let right = parser.expression(precedence.next())?;
        Ok(Spanned {
            span: left.span.merge(right.span),
            item: Expression::range(left, right)
        })
    }

    pub fn tuple(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
        // `[for x in xs ...]` is a list comprehension, not a list literal —
        // hand off to `for_expr` and wrap the result instead of falling
        // into the ordinary comma-separated element list below.
        if parser.check(&Token::For) {
            let for_tok = parser.advance()?; // consume `for`
            let for_loop = Grammar::for_expr(parser, for_tok)?;
            let closing = parser.consume(Token::RightBracket)?;
            return Ok(Spanned {
                span: t.span.merge(closing.span),
                item: Expression::comprehension(for_loop)
            });
        }

        // n.b. a record expression [a=1, b=2, c=3] parses as a tuple of Assign expressions
        // but we can rewrite it into a record initializer in the compiler, if any of the exprs is an Assign
        // and, I suppose, error if we have a mix
        let exprs = parser.expression_list(&Token::Comma, &Token::RightBracket);
        let closing = parser.consume(Token::RightBracket)?;
        Ok(Spanned {
            span: t.span.merge(closing.span),
            item: Expression::Tuple(exprs)
        })
    }

    /// `for var in iterable (if cond)? do body`. Used both as a bare loop
    /// statement and (wrapped in `[...]`, see `Grammar::tuple`) as the body
    /// of a list comprehension. `do`, like `then` in `if`/`then`/`else`,
    /// is a required delimiter between the header and the body.
    pub fn for_expr(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
        // `t` (the `for` token) is already consumed by the time this runs —
        // both by the normal prefix-rule dispatch in `Parser::expression`,
        // and by `Grammar::tuple`'s special-cased call for `[for ...]`.
        let name_tok = parser.identifier()?;
        let var = match &name_tok.item {
            Token::Identifier(s) => s.clone(),
            _ => unreachable!(),
        };
        parser.consume(Token::In)?;
        let iterable = parser.expression(Precedence::Assign)?;
        let cond = if parser.check(&Token::If) {
            parser.advance()?;
            Some(parser.expression(Precedence::Assign)?)
        } else {
            None
        };
        parser.consume(Token::Do)?;
        let body = parser.expression(Precedence::Assign)?;
        let body_span = body.span;
        Ok(Spanned {
            span: t.span.merge(body_span),
            item: Expression::for_loop(var, iterable, cond, body)
        })
    }

    /// `(field: Type, ...)` — every field requires a type annotation
    /// (unlike function params, where it's optional). Shared by struct
    /// fields, enum common fields, and enum variant fields. Assumes the
    /// opening `(` has not yet been consumed; returns the field list and
    /// the closing paren's span.
    fn field_list(parser: &mut Parser) -> Result<(Vec<Parameter>, Span), Spanned<ParseError>> {
        parser.consume(Token::LeftParen)?;
        let mut fields = Vec::new();
        while !parser.check(&Token::RightParen) && !parser.check(&Token::EOF) {
            let field_tok = parser.identifier()?;
            let field_name = match &field_tok.item {
                Token::Identifier(s) => s.clone(),
                _ => unreachable!(),
            };
            parser.consume(Token::Colon)?;
            let ty_tok = parser.identifier()?;
            fields.push(Parameter { name: field_name, ty: Some(Box::new(ty_tok.map(Expression::literal))) });
            if parser.check(&Token::Comma) {
                parser.advance()?;
            }
        }
        let closing = parser.consume(Token::RightParen)?;
        Ok((fields, closing.span))
    }

    /// `data Name(field: Type, ...)` for a struct, or
    /// `data Name(common: Type, ...) is A(...) | B(...) | ...` for an enum
    /// (the common-field parens are optional in either form; empty ⇒ none).
    /// A multi-line variant list is written with a trailing `|` at the end
    /// of each line (the parser has no lookahead past a newline to safely
    /// know whether a *leading* `|` on the next line is coming).
    pub fn data_decl(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let name_tok = parser.identifier()?;
        let name = match &name_tok.item {
            Token::Identifier(s) => s.clone(),
            _ => unreachable!(),
        };

        let mut end = name_tok.span;
        let fields = if parser.check(&Token::LeftParen) {
            let (fields, closing) = Self::field_list(parser)?;
            end = closing;
            fields
        } else {
            Vec::new()
        };

        let mut variants = Vec::new();
        if parser.check(&Token::Is) {
            parser.advance()?;
            parser.skip_newlines();
            loop {
                let variant_name_tok = parser.identifier()?;
                let variant_name = match &variant_name_tok.item {
                    Token::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };
                end = variant_name_tok.span;
                let variant_fields = if parser.check(&Token::LeftParen) {
                    let (fields, closing) = Self::field_list(parser)?;
                    end = closing;
                    fields
                } else {
                    Vec::new()
                };
                variants.push(VariantDecl { name: variant_name, fields: variant_fields });

                if parser.check(&Token::Pipe) {
                    parser.advance()?;
                    parser.skip_newlines();
                    continue;
                }
                break;
            }
        }

        Ok(Spanned {
            span: token.span.merge(end),
            item: Expression::DataDecl(DataDeclExpr { name, fields, variants })
        })
    }

    /// `Ident` or `Ident.Ident`, optionally followed by `(bind, bind, ...)`
    /// (`_` for a skipped field). Shared by `match` arms and the infix
    /// `is` operator. Returns the pattern and its own span.
    fn pattern(parser: &mut Parser) -> Result<(Pattern, Span), Spanned<ParseError>> {
        let first_tok = parser.identifier()?;
        let first_name = match &first_tok.item {
            Token::Identifier(s) => s.clone(),
            _ => unreachable!(),
        };
        let mut end = first_tok.span;

        let (path, variant) = if parser.check(&Token::Dot) {
            parser.advance()?;
            let variant_tok = parser.identifier()?;
            end = variant_tok.span;
            let variant_name = match &variant_tok.item {
                Token::Identifier(s) => s.clone(),
                _ => unreachable!(),
            };
            (Some(first_name), variant_name)
        } else {
            (None, first_name)
        };

        let mut binds = Vec::new();
        if parser.check(&Token::LeftParen) {
            parser.advance()?;
            while !parser.check(&Token::RightParen) && !parser.check(&Token::EOF) {
                let bind_tok = parser.identifier()?;
                let bind_name = match &bind_tok.item {
                    Token::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };
                binds.push(bind_name);
                if parser.check(&Token::Comma) {
                    parser.advance()?;
                }
            }
            let closing = parser.consume(Token::RightParen)?;
            end = closing.span;
        }

        Ok((Pattern { path, variant, binds }, Span { start: first_tok.span.start, end: end.end }))
    }

    /// `subject is Pattern` — infix on `is`. Legal anywhere as a `Bool`
    /// test; also recognized specially as the entire condition of an `if`
    /// (see `TypeChecker::check_and_lower`'s `Conditional` arm) to bind
    /// the pattern's fields into the `then` branch.
    pub fn is_pattern(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let (pattern, pat_span) = Self::pattern(parser)?;
        Ok(Spanned {
            span: left.span.merge(pat_span),
            item: Expression::is_pattern(left, pattern)
        })
    }

    /// `match subject { is P1 (and guard)? then e1  ...  (else e_default)? }`.
    /// Arms must be newline-separated (a stray `is`/`else` on the same
    /// line as a previous arm's body would otherwise be swallowed by that
    /// body's own expression parsing).
    pub fn match_expr(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let subject = parser.expression(Precedence::Assign)?;
        parser.skip_newlines();
        parser.consume(Token::LeftBrace)?;
        parser.skip_newlines();

        let mut arms = Vec::new();
        let mut default = None;
        while !parser.check(&Token::RightBrace) && !parser.check(&Token::EOF) {
            if parser.check(&Token::Else) {
                parser.advance()?;
                parser.skip_newlines();
                let body = parser.expression(Precedence::Assign)?;
                default = Some(Box::new(body));
            } else {
                parser.consume(Token::Is)?;
                let (pattern, _) = Self::pattern(parser)?;
                let guard = if parser.check(&Token::And) {
                    parser.advance()?;
                    Some(Box::new(parser.expression(Precedence::Assign)?))
                } else {
                    None
                };
                parser.skip_newlines();
                parser.consume(Token::Then)?;
                parser.skip_newlines();
                let body = parser.expression(Precedence::Assign)?;
                arms.push(MatchArm { pattern, guard, body: Box::new(body) });
            }
            parser.skip_newlines();
        }
        let closing = parser.consume(Token::RightBrace)?;

        Ok(Spanned {
            span: token.span.merge(closing.span),
            item: Expression::Match(MatchExpr { subject: Box::new(subject), arms, default })
        })
    }

    /// `import "./path.frog" { a, b }` or `import "./path.frog" as alias`.
    /// See `frontend::modules` for how these are resolved and stripped
    /// before typeck ever runs.
    pub fn import_decl(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let path_tok = parser.advance()?;
        let path = match &path_tok.item {
            Token::String(s) => s.clone(),
            _ => {
                let err = path_tok.map(|_| ParseError::Other("expected a string literal module path after 'import'".to_string()));
                return Err(err);
            }
        };

        if parser.check(&Token::As) {
            parser.advance()?;
            let alias_tok = parser.identifier()?;
            let alias = match &alias_tok.item {
                Token::Identifier(s) => s.clone(),
                _ => unreachable!(),
            };
            return Ok(Spanned {
                span: token.span.merge(alias_tok.span),
                item: Expression::Import(ImportExpr { path, kind: ImportKind::Qualified(alias) }),
            });
        }

        let open = parser.consume(Token::LeftBrace)?;
        let mut names = Vec::new();
        loop {
            if parser.check(&Token::RightBrace) { break; }
            let name_tok = parser.identifier()?;
            match &name_tok.item {
                Token::Identifier(s) => names.push(s.clone()),
                _ => unreachable!(),
            };
            if parser.check(&Token::RightBrace) { break; }
            parser.consume(Token::Comma)?;
        }
        if names.is_empty() {
            let err = open.map(|_| ParseError::Other("import list must name at least one binding".to_string()));
            return Err(err);
        }
        let closing = parser.consume(Token::RightBrace)?;

        Ok(Spanned {
            span: token.span.merge(closing.span),
            item: Expression::Import(ImportExpr { path, kind: ImportKind::Named(names) }),
        })
    }

    /// `target.field` — infix on `.`.
    pub fn field_access(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let field_tok = parser.identifier()?;
        let field = match &field_tok.item {
            Token::Identifier(s) => s.clone(),
            _ => unreachable!(),
        };
        Ok(Spanned {
            span: left.span.merge(field_tok.span),
            item: Expression::field_access(left, field)
        })
    }
}
