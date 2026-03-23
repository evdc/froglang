use std::process;
use std::time::Instant;
use froglang_core::frontend::{parser::ParseError, tokens::Spanned};
use froglang_core::state::{FrogState, FrogValue};

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

pub fn repl() {
    let mut rl = DefaultEditor::new().expect("Couldn't open rustyline");
    println!("🐸 froglang repl");

    let mut state = FrogState::new();

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
    use froglang_core::frontend::parser::Parser;
    use froglang_core::frontend::typeck::TypeChecker;
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
    let mut state = FrogState::new();
    match state.eval(src) {
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

