use std::process;
use std::time::Instant;
use froglang_core::state::{FrogState, FrogValue};

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

pub fn repl() {
    let mut rl = DefaultEditor::new().expect("Couldn't open rustyline");
    println!("🐸 froglang repl");

    let mut state = FrogState::with_stdlib().expect("FrogState::with_stdlib() failed to build");

    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() { continue; }
                let _ = rl.add_history_entry(&line);

                // ── REPL meta-commands (`:cmd`) ───────────────────────────
                if trimmed.starts_with(':') {
                    match trimmed.as_str() {
                        ":gc" => {
                            state.heap.dump();
                        }
                        ":help" => {
                            println!("REPL commands:");
                            println!("  :gc    — dump GC heap state to stderr");
                            println!("  :help  — show this message");
                            println!("froglang builtins: print(s), gc_dump()");
                        }
                        other => {
                            println!("Unknown command '{}'. Try :help", other);
                        }
                    }
                    continue;
                }

                // nb. timing here includes compilation
                // (which is fine; for a JIT situation, compile time matters too)
                let t0 = Instant::now();
                let result = state.eval(&trimmed);
                let t1  = Instant::now();
                match result {
                    Err(e) => println!("{}", e),
                    Ok((value, ty)) => {
                        let value_str = value.display_str();
                        if value_str.is_empty() {
                            println!(":: {}", ty);
                        } else {
                            println!("{} :: {}", value_str, ty);
                        }
                    }
                }
                println!("({:?})", t1.duration_since(t0));
            },
            Err(ReadlineError::Interrupted) => { println!("^C"); continue; },
            Err(ReadlineError::Eof)         => { println!("Goodbye!"); break; },
            Err(err) => { eprintln!("Error: {}", err); break; },
        }
    }
}

fn check(src: &str, path: Option<&std::path::Path>) {
    use froglang_core::frontend::modules;
    use froglang_core::frontend::typeck::TypeChecker;
    use froglang_core::frontend::expression::Expression;
    use froglang_core::frontend::tokens::{Span, Spanned};

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let base = path.map(|p| p.to_path_buf()).unwrap_or_else(|| cwd.join("<check>"));

    let stmts = match path {
        Some(p) => modules::resolve_file(p),
        None => modules::resolve_source(src, &base),
    };
    let stmts = match stmts {
        Ok(s) => s,
        Err(e) => { println!("Module error: {}", e); process::exit(1); }
    };
    let span = match (stmts.first(), stmts.last()) {
        (Some(first), Some(last)) => first.span.merge(last.span),
        _ => Span::new((0, 0), (0, 0)),
    };
    let ast = Spanned::from(Expression::Block(stmts), span);

    let mut tc = TypeChecker::new();
    match tc.check_and_lower(ast) {
        Ok(typed) => println!(":: {}", typed.item.ty),
        Err(e)    => { println!("Type error: {}", e); process::exit(1); }
    }
}

fn run(src: &str, path: Option<&std::path::Path>) {
    let mut state = match FrogState::with_stdlib() {
        Ok(s) => s,
        Err(e) => { println!("{}", e); process::exit(1); }
    };
    let result = match path {
        Some(p) => state.eval_file(p),
        None => state.eval(src),
    };
    match result {
        Err(e) => { println!("{}", e); process::exit(1); }
        Ok((value, result_ty)) => {
            match &value {
                FrogValue::None => {},
                FrogValue::Str(s) => println!("{}", s),
                _ => {
                    let s = value.display_str();
                    if !s.is_empty() { println!("{}", s); }
                }
            }
            let _ = result_ty;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.as_slice() {
        // frog run <expr-or-file>
        [_, cmd, input] if cmd == "run" => {
            let path = std::path::Path::new(input);
            if path.is_file() {
                run("", Some(path));
            } else {
                run(input, None);
            }
        }
        // frog check <expr-or-file>
        [_, cmd, input] if cmd == "check" => {
            let path = std::path::Path::new(input);
            if path.is_file() {
                check("", Some(path));
            } else {
                check(input, None);
            }
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

