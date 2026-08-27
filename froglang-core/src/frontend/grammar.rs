use crate::frontend::{expression::{DataDeclExpr, Expression, FieldDecl, ImportExpr, ImportKind, MatchArm, MatchExpr, Mutability, Parameter, Pattern, VariantDecl}, parser::{ParseError, ParseResult, Parser, Precedence}, tokens::{Span, Spanned, Token}, type_expr::TypeExpr};

/// Result of parsing a type annotation. Parallel to `ParseResult`, but over
/// the type grammar (`crate::frontend::type_expr`) rather than `Expression`.
pub type TypeParseResult = Result<Spanned<TypeExpr>, Spanned<ParseError>>;

pub type PrefixFnType = fn(&mut Parser, Spanned<Token>) -> ParseResult;
pub type InfixFnType = fn(&mut Parser, Spanned<Token>, Spanned<Expression>, Precedence) -> ParseResult;

#[derive(Debug)]
pub struct ParseRule {
    /// `None` if this token cannot start an expression (a statement
    /// separator, a closing delimiter, EOF, or a keyword like `else`/`then`
    /// that only means something inside an enclosing construct).
    pub prefix: Option<PrefixFnType>,
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
    
    /// `let name = expr` / `mut name = expr` — a fresh declaration. Which
    /// keyword introduced this call decides the binding's `Mutability`
    /// (`token.item` is `Token::Let` or `Token::Mut`; see `MUTABILITY.md`).
    /// Continue parsing a declaration whose leading keyword (`token`) and
    /// name have already been consumed: an optional `: Type`, `=`, then an
    /// expression. Shared by `let_binding` (which always takes this path)
    /// and `mut_prefix` (which takes it only once it's seen this *is* a
    /// declaration, not a call-argument mutation marker).
    fn finish_declaration(parser: &mut Parser, token: &Spanned<Token>, name: Spanned<Token>, mutability: Mutability) -> ParseResult {
        let ty = if parser.check(&Token::Colon) {
            parser.advance()?;
            Some(Self::type_expr(parser)?)
        } else { None };
        parser.consume(Token::Assign)?;
        parser.skip_newlines();

        let expr: Spanned<Expression> = parser.expression(Precedence::Assign)?;
        Ok(Spanned {
            span: token.span.merge(expr.span),
            item: Expression::assign(
                name.map(Expression::literal),
                ty,
                expr,
                Some(mutability))
        })
    }

    pub fn let_binding(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        // later(?) add destructuring assignment here
        let name = parser.identifier()?;
        Self::finish_declaration(parser, &token, name, Mutability::Immutable)
    }

    /// `mut name = expr` (a declaration) or `mut name` (marks an existing
    /// mutable binding as the target of a `mut` parameter at a call site,
    /// `bump(mut a)`) — see `MUTABILITY.md`. Disambiguated by what follows
    /// the identifier: `:` or `=` continues exactly like `let_binding`;
    /// anything else (`,`, `)`, a statement separator) means this is the
    /// call-argument form, which accepts nothing but a bare identifier —
    /// `mut a.b` or `mut f()` have no meaning there, so the identifier
    /// parsed above is already the whole of it.
    pub fn mut_prefix(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let name = parser.identifier()?;
        if parser.check(&Token::Colon) || parser.check(&Token::Assign) {
            return Self::finish_declaration(parser, &token, name, Mutability::Mutable);
        }
        let name_span = name.span;
        Ok(Spanned {
            span: token.span.merge(name_span),
            item: Expression::mut_arg(name.map(Expression::literal)),
        })
    }

    /// Valid targets: a bare identifier (`x = ...`), or a chain of `.field`
    /// and `[index]` steps rooted at one (`a.b.c = ...`, `xs[0] = ...`,
    /// `o.xs[0].f = ...`) — a *place*, in `MUTABILITY.md`'s terms. A
    /// non-identifier root (`foo().x = ...`) is rejected here; how many
    /// `[index]` steps a place may contain (today: at most one) is a
    /// semantic rule enforced by `TypeChecker::lower_assign`, not a
    /// syntactic one, so the parser accepts any depth and lets the checker
    /// give the precise error.
    fn is_assignable_place(expr: &Expression) -> bool {
        match expr {
            Expression::FieldAccess(fa) => Self::is_assignable_place(&fa.target.item),
            Expression::Index(idx) => Self::is_assignable_place(&idx.target.item),
            other => other.get_identifier().is_some(),
        }
    }

    pub fn assign(parser: &mut Parser, _token: Spanned<Token>, left: Spanned<Expression>, precedence: Precedence) -> ParseResult {
        if !Self::is_assignable_place(&left.item) {
            return Err(left.to(ParseError::InvalidAssignmentTarget));
        }
        let right = parser.expression(precedence)?;
        Ok(Spanned {
            span: left.span.merge(right.span),
            // `decl: None` — this is a plain `target = value`, resolved
            // by `TypeChecker::lower_assign` against an existing binding
            // rather than introducing one.
            item: Expression::assign(left, None, right, None)
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
            // `mut name: T` — see `MUTABILITY.md`. `mut` binds the
            // parameter, not the type, matching a `mut`
            // declaration one level up.
            let mutable = if parser.check(&Token::Mut) {
                parser.advance()?;
                true
            } else {
                false
            };
            let param_tok = parser.identifier()?;
            let param_name = match &param_tok.item {
                Token::Identifier(s) => s.clone(),
                _ => unreachable!(),
            };
            let ty = if parser.check(&Token::Colon) {
                parser.advance()?;
                Some(Self::type_expr(parser)?)
            } else {
                None
            };
            params.push(Parameter { name: param_name, ty, mutable });
            if parser.check(&Token::Comma) {
                parser.advance()?;
            }
        }
        parser.consume(Token::RightParen)?;

        let return_type = if parser.check(&Token::Colon) {
            parser.advance()?;
            Some(Self::type_expr(parser)?)
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
            // A named `func` declaration binds like `let` — immutable, not
            // reassignable — matching an ordinary `let f = x -> ...`.
            item: Expression::assign(name_expr, None, func_expr, Some(Mutability::Immutable)),
        })
    }

    /// `return`, or `return expr`. Whether a value follows is decided by
    /// asking the grammar table itself: if the next token has no prefix
    /// rule (can't start an expression — a statement separator, a closing
    /// delimiter, EOF, or a keyword like `else`/`then` that only means
    /// something as part of an enclosing construct), this is a bare
    /// `return`. This is more robust than a hand-maintained token list —
    /// every keyword that can legally follow a value-less `return` for the
    /// same reason (`if c then return else 0`, `return }`) is covered
    /// automatically, with no risk of the list drifting as tokens are added.
    pub fn return_expr(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        let has_value = Self::get_parse_rule(&parser.current_token).prefix.is_some();
        if has_value {
            let value = parser.expression(Precedence::Assign)?;
            let span = token.span.merge(value.span);
            Ok(Spanned { span, item: Expression::return_value(Some(value)) })
        } else {
            Ok(Spanned { span: token.span, item: Expression::return_value(None) })
        }
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
        let params = params.iter().map(|name| Parameter { name: name.to_string(), ty: None, mutable: false }).collect();
        let body = parser.expression(Precedence::Assign)?;
        Ok(Spanned { 
            span: left.span.merge(body.span), 
            item: Expression::function(params, body)
        })
    }

    /// `e?` — postfix, no right operand to parse (mirrors `call`/`index`'s
    /// shape but consumes nothing further). See `TypeChecker::lower_try`.
    pub fn postfix_try(_parser: &mut Parser, t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        Ok(Spanned { span: left.span.merge(t.span), item: Expression::Try(Box::new(left)) })
    }

    /// `e!` — postfix, see `postfix_try` above and `TypeChecker::lower_unwrap`.
    pub fn postfix_unwrap(_parser: &mut Parser, t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        Ok(Spanned { span: left.span.merge(t.span), item: Expression::Unwrap(Box::new(left)) })
    }

    /// `value catch handler`. The handler is parsed at `Precedence::Assign`
    /// (not the incoming `catch`-relative precedence) specifically so a
    /// `[e] -> body` lambda handler — whose own `->` sits at `Precedence::Assign`
    /// — parses its body fully, the same way `let`'s value and `->`'s own
    /// body do (`let_binding`, `arrow_func`).
    pub fn catch_expr(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let handler = parser.expression(Precedence::Assign)?;
        Ok(Spanned {
            span: left.span.merge(handler.span),
            item: Expression::Catch { value: Box::new(left), handler: Box::new(handler) }
        })
    }

    pub fn type_annotation(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _p: Precedence) -> ParseResult {
        // Parse the type expression at TypeAnnotation precedence (one level above Assign).
        // This prevents `->` (Assign precedence) from firing here, so `f : Int -> Int`
        // is a parse error — the user must parenthesise: `f : (Int -> Int)`.
        // Parenthesised types work because `(` triggers grouping, which parses its
        // interior at Assign level where `->` fires normally.
        let ty = Self::type_expr(parser)?;
        Ok(Spanned { span: left.span.merge(ty.span), item: Expression::annotated(left, ty) })
    }

    // ── The type grammar ────────────────────────────────────────────────
    //
    // Types get their own hand-written recursive-descent parser rather than
    // riding the expression parse-rule table. Two reasons: a type is not an
    // expression (`|` means union here and nothing at all there), and the
    // annotation sites that need it most — `func` params, `func` return
    // types, `data` fields — never called `Parser::expression` in the first
    // place, they called `Parser::identifier` and accepted exactly one token.
    //
    // Loosest to tightest: union (`|`), postfix (`?`), atom (name, `Name(..)`
    // application, or a parenthesised group / function type).

    /// The entry point every annotation site uses. Rejects a trailing bare
    /// `->` so `f: Int -> Int` still reports `FunctionTypeNeedsParens`
    /// rather than silently annotating `f` as `Int`.
    pub fn type_expr(parser: &mut Parser) -> TypeParseResult {
        let ty = Self::type_union(parser)?;
        if parser.check(&Token::Arrow) {
            return Err(ty.to(ParseError::FunctionTypeNeedsParens));
        }
        Ok(ty)
    }

    /// `A | B | C`. A newline is allowed after each `|` so a long union can
    /// be written with trailing pipes, matching how `data ... is ...` already
    /// permits multi-line variant lists.
    fn type_union(parser: &mut Parser) -> TypeParseResult {
        let first = Self::type_postfix(parser)?;
        if !parser.check(&Token::Pipe) {
            return Ok(first);
        }
        let mut members = vec![first];
        while parser.check(&Token::Pipe) {
            parser.advance()?;
            parser.skip_newlines();
            members.push(Self::type_postfix(parser)?);
        }
        let span = members[0].span.merge(members[members.len() - 1].span);
        Ok(Spanned::from(TypeExpr::Union(members), span))
    }

    /// `T?`, `T??` (idempotent, but parsed rather than rejected here — the
    /// flattening is `normalize`'s job).
    fn type_postfix(parser: &mut Parser) -> TypeParseResult {
        let mut ty = Self::type_atom(parser)?;
        while parser.check(&Token::Question) {
            let q = parser.advance()?;
            let span = ty.span.merge(q.span);
            ty = Spanned::from(TypeExpr::Optional(Box::new(ty)), span);
        }
        Ok(ty)
    }

    /// Everything to the right of an `->` inside a function type. `->` is
    /// right-associative, so `(Int -> Int -> Int)` is
    /// `Int -> (Int -> Int)` — a curried function, matching how the
    /// expression-level `->` already associated.
    fn type_arrow_tail(parser: &mut Parser) -> TypeParseResult {
        let first = Self::type_union(parser)?;
        if !parser.check(&Token::Arrow) {
            return Ok(first);
        }
        parser.advance()?;
        let rest = Self::type_arrow_tail(parser)?;
        let span = first.span.merge(rest.span);
        Ok(Spanned::from(TypeExpr::Func(vec![first], Box::new(rest)), span))
    }

    /// Consume a closing `>` for a `<...>` binder/type-argument list,
    /// special-casing the no-space hazard `X<Y>=5` — the lexer reads its
    /// last two characters as one `Token::GtEq`, not `Gt` followed by
    /// `Assign`, since it can't know at lex time that a `>` is closing a
    /// bracket rather than being a comparison operator. When the current
    /// token is `GtEq`, this splits it in place: the closing `>`'s span is
    /// returned, and `parser.current_token` is overwritten in-place to
    /// become the leftover `=` (as `Token::Assign`), covering exactly the
    /// second character's span, so whoever parses next sees it as an
    /// ordinary token. `X<Y> = ...` (with a space) never hits this path —
    /// it already lexes as separate `Gt`/`Assign` tokens.
    fn expect_close_angle(parser: &mut Parser) -> Result<Span, Spanned<ParseError>> {
        if parser.check(&Token::Gt) {
            let t = parser.consume(Token::Gt)?;
            return Ok(t.span);
        }
        if parser.check(&Token::GtEq) {
            let old = parser.current_token.span;
            let split = crate::frontend::tokens::Position { line: old.start.line, col: old.start.col + 1 };
            let gt_span = Span { start: old.start, end: split };
            parser.current_token = Spanned { span: Span { start: split, end: old.end }, item: Token::Assign };
            return Ok(gt_span);
        }
        parser.consume(Token::Gt).map(|t| t.span)
    }

    /// `<A, B>` — a `data` declaration's type-parameter binder list, bare
    /// names only (no bounds syntax yet, e.g. `<T: Ord>` — a later stage
    /// per `TRAITS.md`'s "Bounds" section). Assumes the opening `<` has not
    /// yet been consumed; returns the names in declared order and the
    /// closing `>`'s span.
    fn type_param_list(parser: &mut Parser) -> Result<(Vec<String>, Span), Spanned<ParseError>> {
        parser.consume(Token::Lt)?;
        let mut names = Vec::new();
        loop {
            let tok = parser.identifier()?;
            match &tok.item {
                Token::Identifier(s) => names.push(s.clone()),
                _ => unreachable!("Parser::identifier only returns Token::Identifier"),
            }
            if parser.check(&Token::Comma) {
                parser.advance()?;
                continue;
            }
            break;
        }
        let closing = Self::expect_close_angle(parser)?;
        Ok((names, closing))
    }

    /// `Name`, `Name<A, B>`, `(T)`, or `(A, B -> C)`.
    fn type_atom(parser: &mut Parser) -> TypeParseResult {
        if parser.check(&Token::LeftParen) {
            let open = parser.advance()?;
            let mut items = vec![Self::type_union(parser)?];
            while parser.check(&Token::Comma) {
                parser.advance()?;
                items.push(Self::type_union(parser)?);
            }
            if parser.check(&Token::Arrow) {
                parser.advance()?;
                let result = Self::type_arrow_tail(parser)?;
                let close = parser.consume(Token::RightParen)?;
                let span = open.span.merge(close.span);
                return Ok(Spanned::from(TypeExpr::Func(items, Box::new(result)), span));
            }
            let close = parser.consume(Token::RightParen)?;
            let span = open.span.merge(close.span);
            if items.len() != 1 {
                // `(Int, Str)` with no `->` — a tuple type, which doesn't
                // exist yet. Say so rather than silently dropping members.
                return Err(Spanned::from(
                    ParseError::Other("Expected `->` after a parenthesised parameter list — tuple types are not supported".to_string()),
                    span,
                ));
            }
            let inner = items.pop().expect("length checked above");
            // Re-span to include the parens, so a later error points at the
            // whole group rather than at its interior.
            return Ok(Spanned::from(inner.item, span));
        }

        let name_tok = parser.identifier()?;
        let mut name = match &name_tok.item {
            Token::Identifier(s) => s.clone(),
            _ => unreachable!("Parser::identifier only returns Token::Identifier"),
        };
        // Dotted names are kept as one string: `utils.Point` (a type reached
        // through a qualified import, collapsed by `frontend::modules` before
        // type checking) and, later, `Shape.Circle`. Resolution treats the
        // whole dotted string as the name.
        let mut name_span = name_tok.span;
        while parser.check(&Token::Dot) {
            parser.advance()?;
            let part = parser.identifier()?;
            match &part.item {
                Token::Identifier(s) => { name.push('.'); name.push_str(s); }
                _ => unreachable!("Parser::identifier only returns Token::Identifier"),
            }
            name_span = name_span.merge(part.span);
        }
        if parser.check(&Token::Lt) {
            parser.advance()?;
            let mut args = vec![Self::type_union(parser)?];
            while parser.check(&Token::Comma) {
                parser.advance()?;
                args.push(Self::type_union(parser)?);
            }
            let close_span = Self::expect_close_angle(parser)?;
            let span = name_span.merge(close_span);
            return Ok(Spanned::from(TypeExpr::Apply(name, args), span));
        }
        Ok(Spanned::from(TypeExpr::Name(name), name_span))
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
        // into the ordinary comma-separated element list below. Newlines
        // right after `[` are insignificant here (same as everywhere else
        // inside brackets, see `expression_list`), so skip them before the
        // `for` lookahead — otherwise `[\n for x in xs do ...]` silently
        // falls through to list-literal parsing instead of erroring or
        // being recognized as a comprehension.
        parser.skip_newlines();
        if parser.check(&Token::For) {
            let for_tok = parser.advance()?; // consume `for`
            let for_loop = Grammar::for_expr(parser, for_tok)?;
            parser.skip_newlines();
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

    /// `(name: Type, ...)` (named fields) or `(Type, ...)` (positional
    /// "tuple struct" fields, ERRORS.md's positional-args followup) —
    /// every field requires a type annotation (unlike function params,
    /// where it's optional). Shared by struct fields, enum common fields,
    /// and enum variant fields. Assumes the opening `(` has not yet been
    /// consumed; returns the field list and the closing paren's span.
    ///
    /// Which shape a given field is takes one token of lookahead past a
    /// leading identifier: `Foo(x: Int)`'s `x` is a field name (followed
    /// by `:`), but `Foo(Node)`'s `Node` is a bare type (not followed by
    /// `:`) — indistinguishable until that next token is seen, since a
    /// type name is itself an identifier. `Parser::snapshot`/`restore`
    /// resolves it: consume the identifier, check for `:`, and if it's
    /// absent, roll back and parse a type expression from scratch instead
    /// (needed for a positional field whose type is more than one token,
    /// e.g. `List[Int]` or `A | B`).
    ///
    /// A single field list must be all-named or all-positional — never
    /// mixed, per the same reasoning `Grammar::data_decl`'s positional
    /// story is built on: a field list's own calling convention (named
    /// args vs. positional args, see `TypeChecker::lower_record_args`)
    /// has to be unambiguous from the declaration alone.
    fn field_list(parser: &mut Parser) -> Result<(Vec<FieldDecl>, Span), Spanned<ParseError>> {
        parser.consume(Token::LeftParen)?;
        let mut fields = Vec::new();
        while !parser.check(&Token::RightParen) && !parser.check(&Token::EOF) {
            let snapshot = parser.snapshot();
            let name = if let Token::Identifier(s) = parser.current_token.item.clone() {
                parser.advance()?;
                if parser.check(&Token::Colon) {
                    parser.advance()?;
                    Some(s)
                } else {
                    parser.restore(snapshot);
                    None
                }
            } else {
                None
            };
            let ty = Self::type_expr(parser)?;
            fields.push(FieldDecl { name, ty });
            if parser.check(&Token::Comma) {
                parser.advance()?;
            }
        }
        let closing = parser.consume(Token::RightParen)?;
        let named = fields.iter().filter(|f| f.name.is_some()).count();
        if named != 0 && named != fields.len() {
            return Err(Spanned::new(
                ParseError::Other("a field list must be all named (`x: T`) or all positional (`T`), not a mix".to_string()),
                closing.span.start, closing.span.end,
            ));
        }
        Ok((fields, closing.span))
    }

    /// `data Name(field: Type, ...)` for a struct, or
    /// `data Name(common: Type, ...) is A(...) | B(...) | ...` for an enum
    /// (the common-field parens are optional in either form; empty ⇒ none).
    /// A multi-line variant list may use either a trailing `|` at the end
    /// of each line or a leading `|` at the start of the next (see
    /// `Parser::peek_past_newlines_is`, which disambiguates the latter from
    /// an ordinary statement-ending newline).
    pub fn data_decl(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        Self::data_decl_body(parser, token, false)
    }

    /// `error Name(...)` / `error Name(...) is A | B` — sugar for `data`
    /// with an implicit `provides Error`. An explicit trailing `provides`
    /// clause may still list additional traits alongside it.
    pub fn error_decl(parser: &mut Parser, token: Spanned<Token>) -> ParseResult {
        Self::data_decl_body(parser, token, true)
    }

    fn data_decl_body(parser: &mut Parser, token: Spanned<Token>, implicit_error: bool) -> ParseResult {
        let name_tok = parser.identifier()?;
        let name = match &name_tok.item {
            Token::Identifier(s) => s.clone(),
            _ => unreachable!(),
        };

        let mut end = name_tok.span;
        let type_params = if parser.check(&Token::Lt) {
            let (names, closing) = Self::type_param_list(parser)?;
            end = closing;
            names
        } else {
            Vec::new()
        };
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

                if parser.peek_past_newlines_is(&Token::Pipe) {
                    parser.advance()?;
                    parser.skip_newlines();
                    continue;
                }
                break;
            }
        }

        let mut provides = Vec::new();
        if parser.check(&Token::Provides) {
            parser.advance()?;
            loop {
                let trait_tok = parser.identifier()?;
                let trait_name = match &trait_tok.item {
                    Token::Identifier(s) => s.clone(),
                    _ => unreachable!(),
                };
                end = trait_tok.span;
                provides.push(trait_name);
                if parser.check(&Token::Comma) {
                    parser.advance()?;
                    continue;
                }
                break;
            }
        }
        if implicit_error && !provides.iter().any(|p| p == "Error") {
            provides.push("Error".to_string());
        }

        Ok(Spanned {
            span: token.span.merge(end),
            item: Expression::DataDecl(DataDeclExpr { name, type_params, fields, variants, provides })
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

        Ok((Pattern { path, variant, binds, resolved_member: None }, Span { start: first_tok.span.start, end: end.end }))
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
