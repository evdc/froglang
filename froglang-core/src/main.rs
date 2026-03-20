use std::process;
use froglang_core::frontend::expression::Expression;
use froglang_core::frontend::parser::Parser;
use froglang_core::frontend::typeck::{Type, TypeChecker};
use froglang_core::frontend::{parser::ParseError, tokens::{Spanned, Token}};

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

pub fn repl() {
    let mut rl = DefaultEditor::new().expect("Couldn't open rustyline");
    println!("🐸 froglang repl");

    // Accumulate successfully compiled lines so bindings persist across entries.
    let mut history: Vec<String> = Vec::new();

    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() { continue; }
                let _ = rl.add_history_entry(&line);

                // Build the full program: all prior lines + this one.
                let full_src = history.iter().chain(std::iter::once(&trimmed))
                    .cloned().collect::<Vec<_>>().join("\n");

                match Parser::parse(&full_src) {
                    Err(errs) => print_parse_errors(&errs),
                    Ok(ast) => {
                        let mut tc = TypeChecker::new();
                        match tc.check_and_lower(ast) {
                            Err(e) => println!("Type error: {}", e),
                            Ok(typed) => {
                                let result_ty = typed.item.ty.clone();
                                let mut codegen = froglang_core::codegen::Codegen::new();
                                let main_id = codegen.compile(typed);
                                let ptr = codegen.module.get_finalized_function(main_id);
                                let f: fn() -> i64 = unsafe { std::mem::transmute(ptr) };
                                let bits = f();

                                let value_str = match &result_ty {
                                    Type::Float    => format!("{:?}", f64::from_bits(bits as u64)),
                                    Type::Bool     => format!("{}", bits != 0),
                                    Type::None     => String::new(),
                                    Type::Function { .. } => String::new(),
                                    _              => format!("{}", bits),
                                };

                                if value_str.is_empty() {
                                    println!(":: {}", result_ty);
                                } else {
                                    println!("{} :: {}", value_str, result_ty);
                                }

                                history.push(trimmed);
                            }
                        }
                    }
                }
            },
            Err(ReadlineError::Interrupted) => { println!("^C"); continue; },
            Err(ReadlineError::Eof)         => { println!("Goodbye!"); break; },
            Err(err) => { eprintln!("Error: {}", err); break; },
        }
    }
}

/// Print a Spanned<Expression> as an indented tree, e.g.:
/// ```text
/// 0:0..0:9    Binary(+)
/// 0:0..0:1      Int(1)
/// 0:4..0:9      Binary(*)
/// 0:5..0:6        Int(2)
/// 0:8..0:9        Int(3)
/// ```
fn print_ast_tree(expr: &Spanned<Expression>, depth: usize) {
    let span = format!(
        "{}:{}..{}:{}",
        expr.span.start.line, expr.span.start.col,
        expr.span.end.line,   expr.span.end.col
    );
    let indent = "  ".repeat(depth);
    let label = node_label(expr);
    println!("{:<12}  {}{}", span, indent, label);
    for child in node_children(expr) {
        print_ast_tree(child, depth + 1);
    }
}

fn node_label(expr: &Spanned<Expression>) -> String {
    match &expr.item {
        Expression::Literal(lit) => match &lit.token {
            Token::Int(n)        => format!("Int({})", n),
            Token::Float(f)      => format!("Float({})", f),
            Token::String(s)     => format!("Str({:?})", s),
            Token::Identifier(n) => format!("Ident({})", n),
            Token::True          => "Bool(true)".to_string(),
            Token::False         => "Bool(false)".to_string(),
            t                    => format!("{}", t),
        },
        Expression::Binary(b)      => format!("Binary({})", b.op),
        Expression::Unary(u)       => format!("Unary({})", u.op),
        Expression::Assign(_)      => "Assign".to_string(),
        Expression::Block(stmts)   => format!("Block({} stmts)", stmts.len()),
        Expression::Call(_)        => "Call".to_string(),
        Expression::Function(f)    => {
            let params: Vec<_> = f.params.iter().map(|p| p.name.as_str()).collect();
            format!("Function({})", params.join(", "))
        },
        Expression::Conditional(_) => "Conditional".to_string(),
        Expression::Tuple(elems)   => format!("Tuple({} elems)", elems.len()),
        Expression::Annotated(_)   => "Annotated".to_string(),
    }
}

fn node_children(expr: &Spanned<Expression>) -> Vec<&Spanned<Expression>> {
    match &expr.item {
        Expression::Literal(_)      => vec![],
        Expression::Binary(b)       => vec![&b.left, &b.right],
        Expression::Unary(u)        => vec![&u.expr],
        Expression::Assign(a)       => {
            let mut v: Vec<&Spanned<Expression>> = vec![&a.target];
            if let Some(ty) = &a.typ { v.push(ty); }
            v.push(&a.value);
            v
        },
        Expression::Block(stmts)    => stmts.iter().collect(),
        Expression::Call(c)         => {
            let mut v = vec![c.callable.as_ref()];
            v.extend(c.args.iter());
            v
        },
        Expression::Function(f)     => vec![&f.body],
        Expression::Conditional(c)  => {
            let mut v = vec![c.cond.as_ref(), c.true_branch.as_ref()];
            if let Some(fb) = &c.false_branch { v.push(fb); }
            v
        },
        Expression::Tuple(elems)    => elems.iter().collect(),
        Expression::Annotated(a)    => vec![&a.expr, &a.ty],
    }
}

fn print_parse_errors(errors: &[Spanned<ParseError>]) {
    println!("Parse errors:");
    for (i, error) in errors.iter().enumerate() {
        println!("  {}. {:?}", i + 1, error);
    }
    if errors.len() > 1 {
        println!("Note: {} errors found, showing all.", errors.len());
    }
}

fn check(src: &str) {
    match Parser::parse(src) {
        Err(errs) => {
            print_parse_errors(&errs);
            process::exit(1);
        }
        Ok(ast) => {
            let mut tc = TypeChecker::new();
            match tc.check_and_lower(ast) {
                Ok(typed) => println!(":: {}", typed.item.ty),
                Err(e)    => { println!("Type error: {}", e); process::exit(1); }
            }
        }
    }
}

fn run(src: &str) {
    match Parser::parse(src) {
        Err(errs) => {
            print_parse_errors(&errs);
            process::exit(1);
        }
        Ok(ast) => {
            let mut tc = TypeChecker::new();
            match tc.check_and_lower(ast) {
                Err(e) => { println!("Type error: {}", e); process::exit(1); }
                Ok(typed) => {
                    let result_ty = typed.item.ty.clone();
                    let mut codegen = froglang_core::codegen::Codegen::new();
                    let main_id = codegen.compile(typed);
                    let ptr = codegen.module.get_finalized_function(main_id);
                    let f: fn() -> i64 = unsafe { std::mem::transmute(ptr) };
                    let t0 = std::time::Instant::now();
                    let bits = f();
                    let elapsed = t0.elapsed();
                    if result_ty == Type::Float {
                        println!("{:?}", f64::from_bits(bits as u64));
                    } else {
                        println!("{}", bits);
                    }
                    eprintln!("({:?})", elapsed);
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.as_slice() {
        // frog run <expr-or-file>
        [_, cmd, input] if cmd == "run" => {
            let src = if std::path::Path::new(input).is_file() {
                match std::fs::read_to_string(input) {
                    Ok(s)  => s,
                    Err(e) => { eprintln!("Error reading {}: {}", input, e); process::exit(1); }
                }
            } else {
                input.clone()
            };
            run(&src);
        }
        // frog check <expr-or-file>
        [_, cmd, input] if cmd == "check" => {
            let src = if std::path::Path::new(input).is_file() {
                match std::fs::read_to_string(input) {
                    Ok(s)  => s,
                    Err(e) => { eprintln!("Error reading {}: {}", input, e); process::exit(1); }
                }
            } else {
                input.clone()
            };
            check(&src);
        }
        // unknown subcommand
        [_, cmd, ..] if !cmd.starts_with('-') && cmd != "check" && cmd != "run" => {
            eprintln!("Unknown subcommand '{}'. Usage: frog [check|run <expr|file>]", cmd);
            process::exit(1);
        }
        // no args → REPL
        _ => repl(),
    }
}
