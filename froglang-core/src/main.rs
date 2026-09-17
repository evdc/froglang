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

/// The type checker `run` checks against, plus the host functions behind its
/// stdlib names. `check` and `build` start from this rather than a bare
/// `TypeChecker::new()`: the stdlib prelude declares `ErrMsg`/`IndexError`/
/// `KeyError` (which `get` returns) and its host functions add the `str`/`fs`
/// names, so without it they reject programs `frog run` accepts.
fn stdlib_checker() -> (froglang_core::frontend::typeck::TypeChecker, Vec<froglang_core::host::HostFn>) {
    let builder = froglang_core::stdlib::install(FrogState::builder());
    let hosts = builder.hosts().to_vec();
    match builder.build() {
        Ok(state) => (state.tc, hosts),
        Err(e) => { println!("{}", e); process::exit(1); }
    }
}

fn check(src: &str, path: Option<&std::path::Path>) {
    use froglang_core::frontend::modules;
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

    let (mut tc, _) = stdlib_checker();
    let mut typed = match tc.check_and_lower(ast) {
        Ok(typed) => typed,
        Err(e)    => { println!("Type error: {}", e); process::exit(1); }
    };
    // `check` should not accept a program `run` rejects. Function values
    // are decided by a pass that runs *after* lowering (it needs every
    // type substituted), so checking them means running it — which is
    // also why `monomorphize_generics` has to come first, exactly as in
    // `FrogState::eval_with_base`.
    if let Err(e) = tc.monomorphize_generics(&mut typed)
        .and_then(|()| tc.lower_function_values(&mut typed))
    {
        println!("Type error: {}", e);
        process::exit(1);
    }
    println!(":: {}", typed.item.ty);
}

/// AOT-compile `src`/`path` to a native executable at `out` (`plans/AOT.md`,
/// G8). Emits the object and links it with `cc` against the runtime staticlib
/// (`libfroglang_core.a`), which every program needs: the emitted `main` runs
/// the program through `frog_rt_main`.
fn build(src: &str, path: Option<&std::path::Path>, out: &std::path::Path) {
    use froglang_core::frontend::modules;
    use froglang_core::frontend::expression::Expression;
    use froglang_core::frontend::tokens::{Span, Spanned};
    use froglang_core::codegen::ObjectCodegen;

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let base = path.map(|p| p.to_path_buf()).unwrap_or_else(|| cwd.join("<build>"));

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

    let (mut tc, hosts) = stdlib_checker();
    let mut typed = match tc.check_and_lower(ast) {
        Ok(typed) => typed,
        Err(e)    => { println!("Type error: {}", e); process::exit(1); }
    };
    // Same lowering pipeline the JIT path runs before codegen — see
    // `FrogState::eval_with_base`.
    if let Err(e) = tc.monomorphize_generics(&mut typed)
        .and_then(|()| tc.lower_function_values(&mut typed))
        .and_then(|()| tc.desugar_notation(&mut typed))
    {
        println!("Type error: {}", e);
        process::exit(1);
    }
    froglang_core::frontend::liveness::number_nodes(&mut typed);

    let codegen = match ObjectCodegen::new_object(&hosts) {
        Ok(c) => c,
        Err(e) => { println!("{}", e); process::exit(1); }
    };
    let obj_bytes = codegen.build_object(typed, tc.struct_defs(), tc.union_defs());

    let obj_path = out.with_extension("o");
    if let Err(e) = std::fs::write(&obj_path, &obj_bytes) {
        eprintln!("frog: could not write {}: {}", obj_path.display(), e);
        process::exit(1);
    }

    // Link. Every program needs the runtime staticlib, if only for
    // `frog_rt_main`. Its path is discovered relative to the compiler
    // binary's target dir; overridable with FROG_RUNTIME_LIB for out-of-tree
    // builds. Checked up front: without it `cc` fails with a wall of
    // undefined `frog_*` symbols that doesn't say what's actually missing.
    let runtime_lib = std::env::var_os("FROG_RUNTIME_LIB")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok().and_then(|exe| {
            exe.parent().map(|d| d.join("libfroglang_core.a"))
        }));
    let runtime_lib = match runtime_lib.filter(|p| p.exists()) {
        Some(lib) => lib,
        None => {
            eprintln!("frog: runtime library libfroglang_core.a not found next to the compiler; \
                       build it or set FROG_RUNTIME_LIB");
            process::exit(1);
        }
    };

    let mut cmd = std::process::Command::new("cc");
    cmd.arg(&obj_path).arg(&runtime_lib).arg("-o").arg(out);
    match cmd.status() {
        Ok(s) if s.success() => {
            println!("wrote {}", out.display());
        }
        Ok(s) => { eprintln!("frog: linker (cc) failed with status {}", s); process::exit(1); }
        Err(e) => { eprintln!("frog: could not run linker (cc): {}", e); process::exit(1); }
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
        // frog build <expr-or-file> [-o <out>]
        [_, cmd, input, rest @ ..] if cmd == "build" => {
            let path = std::path::Path::new(input);
            let is_file = path.is_file();
            let out = match rest {
                [flag, out] if flag == "-o" => std::path::PathBuf::from(out),
                [] => if is_file {
                    path.file_stem().map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::path::PathBuf::from("a.out"))
                } else {
                    std::path::PathBuf::from("a.out")
                },
                _ => {
                    eprintln!("Usage: frog build <expr|file> [-o <out>]");
                    process::exit(1);
                }
            };
            if is_file { build("", Some(path), &out); } else { build(input, None, &out); }
        }
        // unknown subcommand
        [_, cmd, ..] if !cmd.starts_with('-') && cmd != "check" && cmd != "run" && cmd != "build" => {
            eprintln!("Unknown subcommand '{}'. Usage: frog [check|run|build <expr|file>]", cmd);
            process::exit(1);
        }
        // no args → REPL
        _ => repl(),
    }
}

