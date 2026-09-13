use froglang_core::codegen::Codegen;
use froglang_core::frontend::expression::Expression;
use froglang_core::frontend::modules::{self, ModuleError};
use froglang_core::frontend::tokens::{Span, Spanned};
use froglang_core::frontend::typeck::TypeChecker;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── helpers ──────────────────────────────────────────────────────────────────

fn module_path(name: &str) -> PathBuf {
    PathBuf::from(format!("{}/tests/programs/modules/{}", env!("CARGO_MANIFEST_DIR"), name))
}

/// Resolve, type-check, compile, and run the entry file at `path`,
/// returning the raw i64 result — mirrors
/// `froglang_core::codegen::compile_and_run`, but goes through the module
/// resolver first instead of a bare `Parser::parse`.
fn run_module_entry_at(path: &Path) -> i64 {
    let stmts = modules::resolve_file(path).expect("module resolution failed");
    let span = match (stmts.first(), stmts.last()) {
        (Some(first), Some(last)) => first.span.merge(last.span),
        _ => Span::new((0, 0), (0, 0)),
    };
    let ast = Spanned::from(Expression::Block(stmts), span);

    let mut tc = TypeChecker::new();
    let mut typed = tc.check_and_lower(ast).expect("type error");
    // The same three passes, in the same order, as `FrogState::eval_with_base`
    // — this helper used to stop at `check_and_lower`, which meant a module
    // could not use anything that lowers to a placeholder (`repr`, `json`,
    // interpolation) or anything the function-value pass provides (a nested
    // `func`, a function reading a top-level `let`) without panicking in
    // codegen rather than failing as a test.
    tc.monomorphize_generics(&mut typed).expect("monomorphization error");
    tc.lower_function_values(&mut typed).expect("function value error");
    tc.desugar_notation(&mut typed).expect("notation error");

    let mut codegen = Codegen::new();
    let (main_id, bindings) = codegen.compile_entry(
        typed, 0, &HashMap::new(), &HashMap::new(), tc.struct_defs(), tc.union_defs(),
    );

    let ptr = codegen.module.get_finalized_function(main_id);
    let f: fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    // See `compile_and_run` in codegen/mod.rs: `out_ptr` needs one i64 slot
    // per flattened leaf of every top-level binding, not just a bare 0.
    let total_slots: usize = bindings.iter()
        .map(|(_, ty)| froglang_core::codegen::struct_fields(ty, tc.struct_defs()).len())
        .sum();
    let mut out_buf: Vec<i64> = vec![0i64; total_slots];
    f(out_buf.as_mut_ptr() as i64)
}

fn run_module_entry(name: &str) -> i64 {
    run_module_entry_at(&module_path(name))
}

fn resolve_err(name: &str) -> ModuleError {
    let path = module_path(name);
    match modules::resolve_file(&path) {
        Ok(_) => panic!("expected module resolution to fail for {}", name),
        Err(e) => e,
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

/// `import "./utils.frog" { add, origin }` — calling an imported function
/// and constructing an imported struct type, both unqualified.
/// add(p.x, add(3, 4)) = add(0, 7) = 7.
#[test]
fn test_named_import() {
    assert_eq!(run_module_entry("named_import.frog"), 7);
}

/// A `${...}` holds ordinary references, so the rewriter has to descend
/// into one: `label` is imported, and `scale` — read from inside an
/// interpolation in the library — is module-local and mangled. Either one
/// left unrewritten is an unbound-variable error rather than a wrong
/// answer. len("x:n=20") = 6.
#[test]
fn test_interpolation_across_a_module_boundary() {
    assert_eq!(run_module_entry("interp_import.frog"), 6);
}

/// `import "./utils.frog" as utils` — qualified access via `utils.add`,
/// `utils.origin`. add(p.y, add(10, 20)) = add(0, 30) = 30.
#[test]
fn test_qualified_import() {
    assert_eq!(run_module_entry("qualified_import.frog"), 30);
}

/// A struct type (`Point`) defined in one module, constructed and
/// field-accessed from the importing module. add(3, 4) = 7.
#[test]
fn test_struct_type_across_modules() {
    assert_eq!(run_module_entry("struct_across_modules.frog"), 7);
}

/// Diamond import: `diamond_a.frog` and `diamond_b.frog` both import
/// `shared.frog`; the entry imports both of them. `shared.frog` must be
/// resolved/renamed exactly once (no duplicate-declaration error).
/// result_a = 100*2+1 = 201, result_b = 100*2+2 = 202, sum = 403.
#[test]
fn test_diamond_import() {
    assert_eq!(run_module_entry("diamond.frog"), 403);
}

/// `cycle_a.frog` imports `cycle_b.frog` which imports `cycle_a.frog` —
/// must be a clean compile error, not a stack overflow/hang.
#[test]
fn test_import_cycle_is_rejected() {
    let err = resolve_err("cycle_a.frog");
    assert!(matches!(err, ModuleError::Cycle { .. }), "expected Cycle, got {:?}", err);
}

/// A function parameter named `add` shadows the imported `add` within that
/// function's own body; the outer `add(1, 2)` call is unaffected.
/// outside = add(1,2) = 3; inside = shadow_add(10) = 10+1 = 11;
/// outside*100 + inside = 311.
#[test]
fn test_local_shadows_import() {
    assert_eq!(run_module_entry("shadowing.frog"), 311);
}

/// Importing a name a module doesn't export is a clean compile error.
#[test]
fn test_unknown_export_is_rejected() {
    let dir = module_path("utils.frog").parent().unwrap().to_path_buf();
    let entry = dir.join("__test_unknown_export_entry.frog");
    std::fs::write(&entry, "import \"./utils.frog\" { does_not_exist }\n0\n").unwrap();
    let err = modules::resolve_file(&entry);
    std::fs::remove_file(&entry).ok();
    assert!(matches!(err, Err(ModuleError::UnknownExport { .. })), "expected UnknownExport, got {:?}", err);
}

/// An `import` nested inside a function body is a clean compile error, not
/// a panic — `import` is only meaningful at a file's direct top level.
#[test]
fn test_nested_import_is_rejected() {
    let dir = module_path("utils.frog").parent().unwrap().to_path_buf();
    let entry = dir.join("__test_nested_import_entry.frog");
    std::fs::write(&entry, "func f(): Int = { import \"./utils.frog\" { add }\n0 }\nf()\n").unwrap();
    let err = modules::resolve_file(&entry);
    std::fs::remove_file(&entry).ok();
    assert!(matches!(err, Err(ModuleError::ImportNotAtTopLevel { .. })), "expected ImportNotAtTopLevel, got {:?}", err);
}

/// Sanity check: a single-file program with no imports at all still
/// resolves and runs identically to before the module system existed.
#[test]
fn test_no_imports_unaffected() {
    let dir = module_path("utils.frog").parent().unwrap().to_path_buf();
    let entry = dir.join("__test_no_imports_entry.frog");
    std::fs::write(&entry, "let x = 40\nx + 2\n").unwrap();
    let result = run_module_entry_at(&entry);
    std::fs::remove_file(&entry).ok();
    assert_eq!(result, 42);
}

/// A trait declared in one module, implemented there *and* in the importing
/// module, then used both ways (`TRAITS.md` Stage 5).
///
/// The point of the test is that a trait name is module-scoped like a type
/// name — so it gets mangled in `collect_names_in`/`rewrite`, and every
/// `provides` clause and prefix form has to follow it — while *impls* stay
/// globally coherent, which they do for free once the names are unique.
/// `Circle(r=3).doubled()` exercises a default body across a module boundary
/// too: 9*2 + 4 + 4 = 26. It also pins the scoping *between* two default
/// bodies: `shifted`'s `let bump` must not stop `doubled`'s `bump()` from
/// being mangled to the module's own function.
#[test]
fn test_trait_across_modules() {
    assert_eq!(run_module_entry("trait_across_modules.frog"), 26);
}
