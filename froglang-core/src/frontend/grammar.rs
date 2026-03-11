use crate::frontend::{expression::{Expression, Parameter}, parser::{ParseError, ParseResult, Parser, Precedence}, tokens::{Spanned, Token}};

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
        // todo - for now, types can only be a name. We will want type-level expressions eventually.
        let ty = if parser.check(&Token::Colon) {
            parser.advance()?;
            Some(parser.identifier()?)
        } else { None };
        parser.consume(Token::Assign)?;

        let expr: Spanned<Expression> = parser.expression(Precedence::Assign)?;
        Ok(Spanned {
            span: token.span.merge(expr.span),
            item: Expression::assign(
                name.map(Expression::literal),
                ty.map(|t| t.map(Expression::literal)), 
                expr)
        })
    }

    pub fn assign(parser: &mut Parser, _token: Spanned<Token>, left: Spanned<Expression>, precedence: Precedence) -> ParseResult {
        let right = parser.expression(precedence)?;
        // todo - validate assignment target in here
        Ok(Spanned {
            span: left.span.merge(right.span),
            item: Expression::assign(left, None, right)
        })
    }

    pub fn grouping(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
        // Sequence of expressions separated by newlines forms a block. A single-expr block is just returned directly
        // TODO: we should allow a leading sep, e.g. `x = ( \n y = 1 \n y + 2 )`

        let exprs = parser.expression_list(&Token::Newline, &Token::RightParen);
        let closing = parser.consume(Token::RightParen)?;
        if exprs.len() == 0 {
            return Err(Spanned::new(ParseError::ExpectedExpression, t.span.start, closing.span.end));
        }
        if exprs.len() == 1 {
            Ok(exprs[0].clone())
        } else {
            Ok(Spanned { 
                span: t.span.merge(closing.span), 
                item: Expression::Block(exprs)
            })
        }
    }

    pub fn conditional(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
        // TODO: we have the opportunity to map each ? to a more specific error here
        // e.g. "Missing conditional expression after `if`"
        let cond = parser.expression(Precedence::Assign)?;
        parser.consume(Token::Then)?;
        let true_branch = parser.expression(Precedence::Assign)?;
        let false_branch = if parser.check(&Token::Else) {
            parser.consume(Token::Else).unwrap(); // just checked it
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

    pub fn arrow_func(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let params = match &left.item {
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

    pub fn type_annotation(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, p: Precedence) -> ParseResult {
        let type_expr = parser.expression(p)?;
        Ok(Spanned { span: left.span.merge(left.span), item: Expression::annotated(left, type_expr) })
    }

    pub fn call(parser: &mut Parser, _t: Spanned<Token>, left: Spanned<Expression>, _prec: Precedence) -> ParseResult {
        let args = parser.expression_list(&Token::Comma, &Token::RightParen);
        let closing = parser.consume(Token::RightParen)?;
        // Need to extend span by to include the closing paren
        Ok(Spanned { span: left.span.merge(closing.span), item: Expression::call(left, args) })
    }

    pub fn tuple(parser: &mut Parser, t: Spanned<Token>) -> ParseResult {
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
}
