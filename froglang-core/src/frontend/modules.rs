// modules.rs
//
// Resolves froglang's module system (`import "./path.frog" { a, b }` /
// `import "./path.frog" as alias`) into a single merged, flat statement
// list that the existing (unmodified) `TypeChecker`/`Codegen` pipeline can
// consume exactly as if it were one file.
//
// Strategy: every file reachable via `import` is parsed, its own top-level
// declarations are renamed with a module-unique prefix, and every free
// reference to an import is rewritten to point at the dependency's mangled
// name — all as a pre-typeck AST rewrite over `Expression`. The entry file
// itself is never renamed, so single-file programs (no imports) behave
// identically to before this feature existed. See the "Modules" plan for
// the full design rationale.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::frontend::expression::{
    AnnotationUse, Expression, FieldAccessExpr, ImportKind, LiteralExpr,
};
use crate::frontend::parser::{ParseError, Parser};
use crate::frontend::type_expr::TypeExpr;
use crate::frontend::tokens::{Spanned, Token};

#[derive(Debug)]
pub enum ModuleError {
    Io { path: PathBuf, message: String },
    Parse { path: PathBuf, errors: Vec<Spanned<ParseError>> },
    Cycle { chain: Vec<PathBuf> },
    UnknownExport { path: PathBuf, name: String },
    NameConflict { path: PathBuf, name: String },
    DuplicateImportBinding { path: PathBuf, name: String },
    /// `import` found nested inside a function body / if / for / etc,
    /// rather than at the direct top level of the file.
    ImportNotAtTopLevel { path: PathBuf },
    /// `#ann import "..."`. Imports are erased by this pass, before the
    /// typechecker (the only thing that knows what an annotation means)
    /// ever runs, so an annotation on one could only be dropped
    /// unvalidated — reject it instead of silently ignoring it.
    AnnotatedImport { path: PathBuf },
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleError::Io { path, message } => {
                write!(f, "could not read module '{}': {}", path.display(), message)
            }
            ModuleError::Parse { path, errors } => {
                write!(f, "parse error(s) in module '{}':", path.display())?;
                // A single lex error routinely cascades into several
                // downstream parse errors at the same (or nearby) span —
                // dedup identical (span, error) pairs so each distinct
                // problem is reported once, not several times.
                let mut seen: Vec<&Spanned<ParseError>> = Vec::new();
                for e in errors {
                    if !seen.iter().any(|s| s.span == e.span && s.item == e.item) {
                        seen.push(e);
                    }
                }
                for e in seen { write!(f, "\n  {}: {}", e.span.start, e.item)?; }
                Ok(())
            }
            ModuleError::Cycle { chain } => {
                write!(f, "import cycle detected: ")?;
                let names: Vec<String> = chain.iter().map(|p| p.display().to_string()).collect();
                write!(f, "{}", names.join(" -> "))
            }
            ModuleError::UnknownExport { path, name } => {
                write!(f, "module '{}' has no top-level name '{}' to import", path.display(), name)
            }
            ModuleError::NameConflict { path, name } => {
                write!(f, "'{}' is both imported and declared at the top level of module '{}'", name, path.display())
            }
            ModuleError::DuplicateImportBinding { path, name } => {
                write!(f, "'{}' is imported more than once in module '{}'", name, path.display())
            }
            ModuleError::ImportNotAtTopLevel { path } => {
                write!(f, "'import' is only allowed at the top level of a file (module '{}')", path.display())
            }
            ModuleError::AnnotatedImport { path } => {
                write!(f, "annotations are not supported on 'import' (module '{}')", path.display())
            }
        }
    }
}

/// Is `expr` an `import`, possibly under one or more `#ann` wrappers?
fn wraps_import(expr: &Expression) -> bool {
    match expr {
        Expression::Import(_) => true,
        Expression::Decorated(d) => wraps_import(&d.target.item),
        _ => false,
    }
}

/// `import` is only meaningful at a file's direct top level (see
/// `resolve_imports`, which only ever scans the outermost statement list) —
/// check for one buried inside a nested `Block`/`Conditional`/`ForLoop`/
/// `Function`/etc and reject it cleanly rather than let it reach the
/// rewriter's `unreachable!()` on `Expression::Import`.
fn check_no_nested_imports(stmts: &[Spanned<Expression>], path: &Path) -> Result<(), ModuleError> {
    fn walk(expr: &Expression, path: &Path) -> Result<(), ModuleError> {
        match expr {
            Expression::Import(_) => Err(ModuleError::ImportNotAtTopLevel { path: path.to_path_buf() }),
            Expression::Unary(u) => walk(&u.expr.item, path),
            Expression::Binary(b) => { walk(&b.left.item, path)?; walk(&b.right.item, path) }
            Expression::Conditional(c) => {
                walk(&c.cond.item, path)?;
                walk(&c.true_branch.item, path)?;
                if let Some(fb) = &c.false_branch { walk(&fb.item, path)?; }
                Ok(())
            }
            Expression::Assign(a) => { walk(&a.target.item, path)?; walk(&a.value.item, path) }
            // A trait's default bodies are the only expressions it holds,
            // and an `import` inside one is as illegal as inside any other
            // function body.
            Expression::TraitDecl(t) => {
                for m in &t.members {
                    if let Some(d) = &m.default { walk(&d.item, path)?; }
                }
                Ok(())
            }
            Expression::ImplDecl(i) => {
                for m in &i.members { walk(&m.item, path)?; }
                Ok(())
            }
            Expression::Function(f) => walk(&f.body.item, path),
            Expression::Call(c) => {
                walk(&c.callable.item, path)?;
                for a in &c.args { walk(&a.item, path)?; }
                Ok(())
            }
            Expression::Tuple(es) | Expression::Block(es) | Expression::Interp(es) => {
                for e in es { walk(&e.item, path)?; }
                Ok(())
            }
            // `a.ty` is a `TypeExpr` — the type grammar has no `import` form.
            Expression::Annotated(a) => walk(&a.expr.item, path),
            Expression::Index(i) => { walk(&i.target.item, path)?; walk(&i.index.item, path) }
            Expression::Slice(s) => {
                walk(&s.target.item, path)?;
                if let Some(start) = &s.start { walk(&start.item, path)?; }
                if let Some(end) = &s.end { walk(&end.item, path)?; }
                Ok(())
            }
            Expression::Range(r) => { walk(&r.start.item, path)?; walk(&r.end.item, path) }
            Expression::ForLoop(fl) => {
                walk(&fl.iterable.item, path)?;
                if let Some(c) = &fl.cond { walk(&c.item, path)?; }
                walk(&fl.body.item, path)
            }
            Expression::Comprehension(inner) => walk(&inner.item, path),
            Expression::FieldAccess(fa) => walk(&fa.target.item, path),
            Expression::Match(m) => {
                walk(&m.subject.item, path)?;
                for arm in &m.arms {
                    if let Some(g) = &arm.guard { walk(&g.item, path)?; }
                    walk(&arm.body.item, path)?;
                }
                if let Some(d) = &m.default { walk(&d.item, path)?; }
                Ok(())
            }
            Expression::IsPattern(ip) => walk(&ip.subject.item, path),
            Expression::Return(value) => match value {
                Some(v) => walk(&v.item, path),
                None => Ok(()),
            },
            Expression::Try(inner) | Expression::Unwrap(inner) => walk(&inner.item, path),
            Expression::Catch { value, handler } => { walk(&value.item, path)?; walk(&handler.item, path) }
            Expression::MutArg(name) => walk(&name.item, path),
            Expression::DataDecl(_) | Expression::AnnotationDecl(_) | Expression::Literal(_) => Ok(()),
            Expression::Decorated(d) => {
                for a in &d.annotations {
                    for arg in &a.args { walk(&arg.item, path)?; }
                }
                walk(&d.target.item, path)
            }
        }
    }
    for s in stmts {
        // The statement itself may legitimately be an Import (that's the
        // valid top-level case) — only its *children* are checked. An
        // annotated import is a `Decorated` wrapping one, which `walk`
        // would report as a *nested* import; it has its own, accurate
        // error instead.
        match &s.item {
            Expression::Import(_) => {}
            Expression::Decorated(d) if wraps_import(&d.target.item) => {
                return Err(ModuleError::AnnotatedImport { path: path.to_path_buf() });
            }
            other => walk(other, path)?,
        }
    }
    Ok(())
}

/// A fully-processed module: its rewritten (renamed) top-level statements,
/// plus the map every importer needs to resolve references into it.
struct ModuleRecord {
    /// Rewritten statement list (Import nodes stripped, own declarations
    /// prefixed, own references to its own imports resolved). Empty for
    /// modules already flattened into an earlier dependency's output.
    stmts: Vec<Spanned<Expression>>,
    /// original top-level export name -> mangled name
    exports: HashMap<String, String>,
}

pub struct ModuleResolver {
    /// canonical path -> resolved record
    cache: HashMap<PathBuf, ModuleRecord>,
    /// canonical paths currently being resolved (for cycle detection)
    in_progress: Vec<PathBuf>,
    /// merged output, dependency-first order; each canonical path emitted once
    order: Vec<PathBuf>,
    counter: usize,
}

/// Resolve `entry_path` and every file it transitively imports into one
/// merged, flat top-level statement list — dependencies first (in
/// first-encountered topological order), then the entry file's own
/// (unrenamed) statements last.
pub fn resolve_file(entry_path: &Path) -> Result<Vec<Spanned<Expression>>, ModuleError> {
    let canonical = canonicalize(entry_path)?;
    let src = std::fs::read_to_string(&canonical)
        .map_err(|e| ModuleError::Io { path: canonical.clone(), message: e.to_string() })?;
    resolve_source(&src, &canonical)
}

/// Same as `resolve_file`, but the entry source is already in memory (e.g.
/// a REPL line or `frog run "<expr>"`) — `entry_path` is only used as the
/// base for resolving that source's own relative imports and does not need
/// to exist on disk.
pub fn resolve_source(src: &str, entry_path: &Path) -> Result<Vec<Spanned<Expression>>, ModuleError> {
    let mut r = ModuleResolver {
        cache: HashMap::new(),
        in_progress: Vec::new(),
        order: Vec::new(),
        counter: 0,
    };

    let block = parse_block(src, entry_path)?;
    let (dep_stmts, entry_stmts) = r.process_entry(block, entry_path)?;

    let mut merged = dep_stmts;
    merged.extend(entry_stmts);
    Ok(merged)
}

fn parse_block(src: &str, path: &Path) -> Result<Vec<Spanned<Expression>>, ModuleError> {
    let parsed = Parser::parse(src)
        .map_err(|errors| ModuleError::Parse { path: path.to_path_buf(), errors })?;
    match parsed.item {
        Expression::Block(stmts) => Ok(stmts),
        other => Ok(vec![Spanned::from(other, parsed.span)]),
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, ModuleError> {
    path.canonicalize()
        .map_err(|e| ModuleError::Io { path: path.to_path_buf(), message: e.to_string() })
}

/// What a module's `import` statement needs, resolved against a dependency
/// it already knows the export map for.
struct ResolvedImport {
    /// local flat-name -> mangled name (named imports)
    named: HashMap<String, String>,
    /// alias -> (field name -> mangled name) (qualified imports)
    qualified: HashMap<String, HashMap<String, String>>,
}

impl ModuleResolver {
    /// Process the *entry* file's already-parsed statement list: resolve
    /// its imports (recursively resolving/renaming each dependency), then
    /// rewrite only the entry's own import-reference sites — never its own
    /// declaration names. Returns (merged dependency statements in
    /// topological order, entry's own rewritten statements).
    fn process_entry(
        &mut self,
        stmts: Vec<Spanned<Expression>>,
        entry_path: &Path,
    ) -> Result<(Vec<Spanned<Expression>>, Vec<Spanned<Expression>>), ModuleError> {
        let dir = entry_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));

        check_no_nested_imports(&stmts, entry_path)?;
        let resolved_imports = self.resolve_imports(&stmts, &dir, entry_path)?;
        let subst = resolved_imports.named;
        let qualified = resolved_imports.qualified;

        let declared = collect_top_level_names(&stmts);
        for name in declared.iter() {
            if subst.contains_key(name) || qualified.contains_key(name) {
                return Err(ModuleError::NameConflict { path: entry_path.to_path_buf(), name: name.clone() });
            }
        }

        let mut rewritten = Vec::with_capacity(stmts.len());
        for s in stmts {
            if matches!(s.item, Expression::Import(_)) { continue; }
            let mut s = s;
            rewrite_top(&mut s.item, &subst, &qualified);
            rewritten.push(s);
        }

        let dep_stmts = self.order.iter()
            .filter_map(|p| self.cache.get(p))
            .flat_map(|r| r.stmts.clone())
            .collect();

        Ok((dep_stmts, rewritten))
    }

    /// Resolve (recursively) every `import` statement found in `stmts`,
    /// building the flat substitution tables the importing module needs.
    fn resolve_imports(
        &mut self,
        stmts: &[Spanned<Expression>],
        dir: &Path,
        importer_path: &Path,
    ) -> Result<ResolvedImport, ModuleError> {
        let mut named = HashMap::new();
        let mut qualified = HashMap::new();

        for s in stmts {
            let imp = match &s.item {
                Expression::Import(i) => i,
                _ => continue,
            };
            let target = dir.join(&imp.path);
            let record = self.resolve_module(&target)?;

            match &imp.kind {
                ImportKind::Named(names) => {
                    for name in names {
                        let mangled = record.exports.get(name).cloned().ok_or_else(|| {
                            ModuleError::UnknownExport { path: target.clone(), name: name.clone() }
                        })?;
                        if named.insert(name.clone(), mangled).is_some() || qualified.contains_key(name) {
                            return Err(ModuleError::DuplicateImportBinding {
                                path: importer_path.to_path_buf(),
                                name: name.clone(),
                            });
                        }
                    }
                }
                ImportKind::Qualified(alias) => {
                    if qualified.insert(alias.clone(), record.exports.clone()).is_some() || named.contains_key(alias) {
                        return Err(ModuleError::DuplicateImportBinding {
                            path: importer_path.to_path_buf(),
                            name: alias.clone(),
                        });
                    }
                }
            }
        }

        Ok(ResolvedImport { named, qualified })
    }

    /// Resolve (parse, recursively resolve its own imports, rename its own
    /// declarations) the module at `path`, memoized by canonical path so a
    /// diamond-imported file is only ever processed once.
    fn resolve_module(&mut self, path: &Path) -> Result<&ModuleRecord, ModuleError> {
        let canonical = canonicalize(path)?;

        if self.cache.contains_key(&canonical) {
            return Ok(self.cache.get(&canonical).unwrap());
        }
        if self.in_progress.contains(&canonical) {
            let mut chain = self.in_progress.clone();
            chain.push(canonical);
            return Err(ModuleError::Cycle { chain });
        }

        self.in_progress.push(canonical.clone());

        let src = std::fs::read_to_string(&canonical)
            .map_err(|e| ModuleError::Io { path: canonical.clone(), message: e.to_string() })?;
        let stmts = parse_block(&src, &canonical)?;

        let dir = canonical.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        check_no_nested_imports(&stmts, &canonical)?;
        let resolved_imports = self.resolve_imports(&stmts, &dir, &canonical)?;
        let import_named = resolved_imports.named;
        let qualified = resolved_imports.qualified;

        let prefix = format!("__mod{}__", self.counter);
        self.counter += 1;

        let declared = collect_top_level_names(&stmts);
        for name in declared.iter() {
            if import_named.contains_key(name) || qualified.contains_key(name) {
                return Err(ModuleError::NameConflict { path: canonical.clone(), name: name.clone() });
            }
        }
        let self_prefix: HashMap<String, String> = declared.iter()
            .map(|n| (n.clone(), format!("{}{}", prefix, n)))
            .collect();

        // subst = self-renaming ∪ named imports (disjoint by the check above)
        let mut subst = self_prefix.clone();
        subst.extend(import_named);

        let mut rewritten = Vec::with_capacity(stmts.len());
        for s in stmts {
            if matches!(s.item, Expression::Import(_)) { continue; }
            let mut s = s;
            rewrite_top(&mut s.item, &subst, &qualified);
            rewritten.push(s);
        }

        // Dependencies were resolved above (via `resolve_imports`), so they're
        // already in `self.order`/`self.cache` ahead of this module — pushing
        // this module's own canonical path below keeps dependency-first order.
        self.in_progress.pop();

        let record = ModuleRecord { stmts: rewritten, exports: self_prefix };
        self.cache.insert(canonical.clone(), record);
        self.order.push(canonical.clone());

        Ok(self.cache.get(&canonical).unwrap())
    }
}

/// Collect every name a module's *own top-level (flat) scope* declares —
/// `let`/`func` assigns and `data` decls — recursing into nested
/// `Block`/`Conditional`/`ForLoop`/etc (since froglang has no block-level
/// scoping — a `let` nested inside a top-level `if` is still a real
/// top-level binding) but never into a `Function`'s body (no closures — a
/// function's own locals are never part of the module's flat top-level
/// scope) and never into `Call` arguments (kwargs are field names, not
/// declarations).
fn collect_top_level_names(stmts: &[Spanned<Expression>]) -> HashSet<String> {
    let mut names = HashSet::new();
    for s in stmts {
        collect_names_in(&s.item, &mut names);
    }
    names
}

fn collect_names_in(expr: &Expression, names: &mut HashSet<String>) {
    match expr {
        Expression::Assign(a) => {
            if let Some(name) = a.target.item.get_identifier() {
                names.insert(name.to_string());
            }
            collect_names_in(&a.value.item, names);
        }
        Expression::DataDecl(d) => {
            names.insert(d.name.clone());
            for v in &d.variants {
                names.insert(v.name.clone());
            }
        }
        // A trait name is module-scoped exactly like a type name — it is
        // what a `provides` clause in another module has to be able to
        // name. Note this arm is *required*, not optional: the `_ => {}`
        // below would otherwise skip it silently and the trait would keep
        // its unmangled name while every reference to it got rewritten.
        Expression::TraitDecl(t) => { names.insert(t.name.clone()); }
        // An impl contributes no module-level name: its members are only ever
        // reachable through the mangled symbols `expand_impls` renames them
        // to, never under their own name. Mangling the trait/type it names is
        // `rewrite`'s job, below.
        Expression::ImplDecl(_) => {}
        // Module-scoped exactly like `DataDecl`/`TraitDecl` above.
        Expression::AnnotationDecl(a) => { names.insert(a.name.clone()); }
        // Must see through the wrapper, not fall into the `_ => {}` below —
        // an annotated top-level `data`/`annotation` declaration's name
        // would otherwise never be registered as one of this module's
        // exports, and every reference to it would be left unmangled.
        Expression::Decorated(d) => collect_names_in(&d.target.item, names),
        Expression::Block(stmts) | Expression::Tuple(stmts) => {
            for s in stmts { collect_names_in(&s.item, names); }
        }
        Expression::Conditional(c) => {
            collect_names_in(&c.cond.item, names);
            collect_names_in(&c.true_branch.item, names);
            if let Some(fb) = &c.false_branch { collect_names_in(&fb.item, names); }
        }
        Expression::ForLoop(fl) => {
            collect_names_in(&fl.iterable.item, names);
            if let Some(c) = &fl.cond { collect_names_in(&c.item, names); }
            collect_names_in(&fl.body.item, names);
        }
        Expression::Comprehension(inner) => collect_names_in(&inner.item, names),
        // Function bodies are their own scope — never contribute to the
        // module's flat top-level names.
        Expression::Function(_) => {}
        // Everything else either can't contain a top-level Assign/DataDecl
        // in valid syntax, or (Call args) intentionally isn't walked here.
        _ => {}
    }
}

/// Rewrite every free identifier reference in `expr` per `subst`
/// (flat name -> mangled name) and `qualified` (alias -> field -> mangled
/// name), starting at top-level scope (no ambient shadow set — see
/// `rewrite_scoped` for the function-body walker, which is where local
/// shadowing actually needs tracking).
fn rewrite_top(
    expr: &mut Expression,
    subst: &HashMap<String, String>,
    qualified: &HashMap<String, HashMap<String, String>>,
) {
    let mut empty_shadow = HashSet::new();
    rewrite(expr, subst, qualified, &mut empty_shadow, false);
}

/// Mangle every type name inside one type annotation.
///
/// The value-side `rewrite` needs a `shadow` set because a local can shadow an
/// imported name; types live in their own namespace and cannot be shadowed by
/// a `let`, so this needs only the substitution maps. Dotted names
/// (`utils.Point`, reaching a type through a qualified import) are resolved
/// against `qualified` first, mirroring how `rewrite` collapses a
/// `FieldAccess` on an alias.
fn rewrite_type_expr(
    ty: &mut Spanned<TypeExpr>,
    subst: &HashMap<String, String>,
    qualified: &HashMap<String, HashMap<String, String>>,
) {
    for name in ty.item.names_mut() {
        rewrite_name(name, subst, qualified);
    }
}

/// Resolve one declaration-namespace name — a type name inside an
/// annotation, or a trait name in a `provides` clause — against this
/// module's substitutions, resolving a dotted `alias.Member` through
/// `qualified` first. Factored out of `rewrite_type_expr` so `provides`
/// gets identical treatment without the type-grammar walk it doesn't need.
fn rewrite_name(
    name: &mut String,
    subst: &HashMap<String, String>,
    qualified: &HashMap<String, HashMap<String, String>>,
) {
    let replacement = match name.split_once('.') {
        Some((alias, member)) => qualified.get(alias).and_then(|m| m.get(member)),
        None => subst.get(name.as_str()),
    };
    if let Some(mangled) = replacement {
        *name = mangled.clone();
    }
}

/// Rewrite every `#name(...)` use's own name (never its dotted arguments'
/// field names, which live in a different, per-annotation namespace) —
/// `plans/DATA.md` Stage 6. Deliberately *not* `rewrite_name`: an
/// annotation's own name can itself be dotted (`db.model`), which is a
/// single flat key in `subst` set by `collect_top_level_names`/
/// `resolve_module`'s `self_prefix`, not an `alias.Member` qualified-import
/// reference — `rewrite_name`'s dot-splitting would look "db" up as an
/// import alias and silently fail to rewrite it.
fn rewrite_annotation_uses(anns: &mut [AnnotationUse], subst: &HashMap<String, String>) {
    for a in anns {
        if let Some(mangled) = subst.get(&a.name) {
            a.name = mangled.clone();
        }
    }
}

/// The shared rewrite walker. `shadow` tracks names currently locally bound
/// (function params, nested `let`s inside a function body, `for` loop
/// variables) that should NOT be substituted. `track_let_shadow` is true
/// only once we're inside a function body: at top level, a plain-identifier
/// `let`/`func` target is never inserted into `shadow` (it's already one of
/// this module's own top-level exports, unconditionally present in
/// `subst` — see `ModuleResolver::resolve_module`'s `NameConflict` check,
/// which guarantees these never collide with an import). Inside a function
/// body, the same target *is* a genuine new local and must shadow.
fn rewrite(
    expr: &mut Expression,
    subst: &HashMap<String, String>,
    qualified: &HashMap<String, HashMap<String, String>>,
    shadow: &mut HashSet<String>,
    track_let_shadow: bool,
) {
    match expr {
        Expression::Literal(lit) => {
            if let Token::Identifier(name) = &lit.token {
                if !shadow.contains(name) {
                    if let Some(mangled) = subst.get(name) {
                        lit.token = Token::Identifier(mangled.clone());
                    }
                }
            }
        }

        Expression::FieldAccess(fa) => {
            if let Some(name) = fa.target.item.get_identifier() {
                if !shadow.contains(name) {
                    if let Some(fields) = qualified.get(name) {
                        if let Some(mangled) = fields.get(&fa.field) {
                            *expr = Expression::Literal(LiteralExpr { token: Token::Identifier(mangled.clone()) });
                            return;
                        }
                    }
                }
            }
            let FieldAccessExpr { target, .. } = fa;
            rewrite(&mut target.item, subst, qualified, shadow, track_let_shadow);
        }

        Expression::Unary(u) => rewrite(&mut u.expr.item, subst, qualified, shadow, track_let_shadow),

        Expression::Binary(b) => {
            rewrite(&mut b.left.item, subst, qualified, shadow, track_let_shadow);
            rewrite(&mut b.right.item, subst, qualified, shadow, track_let_shadow);
        }

        Expression::Conditional(c) => {
            rewrite(&mut c.cond.item, subst, qualified, shadow, track_let_shadow);
            rewrite(&mut c.true_branch.item, subst, qualified, shadow, track_let_shadow);
            if let Some(fb) = &mut c.false_branch {
                rewrite(&mut fb.item, subst, qualified, shadow, track_let_shadow);
            }
        }

        Expression::Assign(a) => {
            rewrite(&mut a.value.item, subst, qualified, shadow, track_let_shadow);
            if let Some(ty) = &mut a.typ {
                rewrite_type_expr(ty, subst, qualified);
            }
            match &mut a.target.item {
                Expression::Literal(LiteralExpr { token: Token::Identifier(name) }) => {
                    if track_let_shadow {
                        shadow.insert(name.clone());
                    } else if let Some(mangled) = subst.get(name) {
                        // Top-level declaration site of one of this
                        // module's own (self-prefixed) exports — rename
                        // the binding itself to match every reference to
                        // it, which already goes through the same `subst`
                        // lookup. Never fires for the entry file, whose
                        // `subst` only ever contains import mappings, not
                        // self-mappings (entry declarations stay bare).
                        *name = mangled.clone();
                    }
                }
                other => rewrite(other, subst, qualified, shadow, track_let_shadow),
            }
        }

        Expression::Function(func) => {
            for p in &mut func.params {
                if let Some(ty) = &mut p.ty {
                    rewrite_type_expr(ty, subst, qualified);
                }
            }
            if let Some(rt) = &mut func.return_type {
                rewrite_type_expr(rt, subst, qualified);
            }
            let mut inner_shadow: HashSet<String> = func.params.iter().map(|p| p.name.clone()).collect();
            rewrite(&mut func.body.item, subst, qualified, &mut inner_shadow, true);
        }

        Expression::Call(c) => {
            rewrite(&mut c.callable.item, subst, qualified, shadow, track_let_shadow);
            for a in &mut c.args {
                // kwarg (`name = value`, struct construction): the target
                // is a field name, never a shadow-introducing binding or a
                // substitution candidate — only rewrite the value.
                if let Expression::Assign(kw) = &mut a.item {
                    rewrite(&mut kw.value.item, subst, qualified, shadow, track_let_shadow);
                } else {
                    rewrite(&mut a.item, subst, qualified, shadow, track_let_shadow);
                }
            }
        }

        // An interpolation is ordinary code and holds ordinary references:
        // `"total: ${helper(x)}"` in an imported module must have `helper`
        // mangled exactly as the same call outside a string would be.
        Expression::Tuple(es) | Expression::Block(es) | Expression::Interp(es) => {
            for e in es {
                rewrite(&mut e.item, subst, qualified, shadow, track_let_shadow);
            }
        }

        Expression::Annotated(a) => {
            rewrite(&mut a.expr.item, subst, qualified, shadow, track_let_shadow);
            rewrite_type_expr(&mut a.ty, subst, qualified);
        }

        Expression::Index(i) => {
            rewrite(&mut i.target.item, subst, qualified, shadow, track_let_shadow);
            rewrite(&mut i.index.item, subst, qualified, shadow, track_let_shadow);
        }

        Expression::Slice(s) => {
            rewrite(&mut s.target.item, subst, qualified, shadow, track_let_shadow);
            if let Some(start) = &mut s.start { rewrite(&mut start.item, subst, qualified, shadow, track_let_shadow); }
            if let Some(end) = &mut s.end { rewrite(&mut end.item, subst, qualified, shadow, track_let_shadow); }
        }

        Expression::Range(r) => {
            rewrite(&mut r.start.item, subst, qualified, shadow, track_let_shadow);
            rewrite(&mut r.end.item, subst, qualified, shadow, track_let_shadow);
        }

        Expression::ForLoop(fl) => {
            rewrite(&mut fl.iterable.item, subst, qualified, shadow, track_let_shadow);
            let already_shadowed = shadow.contains(&fl.var);
            shadow.insert(fl.var.clone());
            if let Some(cond) = &mut fl.cond {
                rewrite(&mut cond.item, subst, qualified, shadow, track_let_shadow);
            }
            rewrite(&mut fl.body.item, subst, qualified, shadow, track_let_shadow);
            if !already_shadowed {
                shadow.remove(&fl.var);
            }
        }

        Expression::Comprehension(inner) => {
            rewrite(&mut inner.item, subst, qualified, shadow, track_let_shadow);
        }

        Expression::DataDecl(d) => {
            if !track_let_shadow {
                if let Some(mangled) = subst.get(&d.name) {
                    d.name = mangled.clone();
                }
                for v in &mut d.variants {
                    if let Some(mangled) = subst.get(&v.name) {
                        v.name = mangled.clone();
                    }
                }
            }
            for p in &mut d.fields {
                rewrite_type_expr(&mut p.ty, subst, qualified);
                rewrite_annotation_uses(&mut p.annotations, subst);
            }
            for v in &mut d.variants {
                for p in &mut v.fields {
                    rewrite_type_expr(&mut p.ty, subst, qualified);
                    rewrite_annotation_uses(&mut p.annotations, subst);
                }
            }
            // A `provides` clause names traits, which are module-scoped
            // (`collect_names_in`) — so `provides shapes.Drawable` and a
            // same-file `provides Drawable` both have to reach the mangled
            // declaration. Routed through the same dotted-name resolution
            // `rewrite_type_expr` uses, since a trait name is spelled like
            // a type name. Impls stay globally coherent for free: mangling
            // makes the names unique, so one global registry needs no
            // module exception (`TRAITS.md`, "Coherence").
            for name in &mut d.provides {
                rewrite_name(name, subst, qualified);
            }
            for m in &mut d.members {
                rewrite(&mut m.item, subst, qualified, shadow, true);
            }
        }

        Expression::TraitDecl(t) => {
            if !track_let_shadow {
                if let Some(mangled) = subst.get(&t.name) {
                    t.name = mangled.clone();
                }
            }
            for m in &mut t.members {
                for p in &mut m.params {
                    if let Some(ty) = &mut p.ty { rewrite_type_expr(ty, subst, qualified); }
                }
                if let Some(rt) = &mut m.return_type { rewrite_type_expr(rt, subst, qualified); }
                // A default body is a function body: its own scope, so
                // `let` bindings inside it shadow rather than rename —
                // matching the `Function` arm above, including the fresh set
                // seeded with the member's own parameters. Fresh per member:
                // sharing one set would let a `let` in one default body
                // suppress mangling for that name in the next member's.
                if let Some(d) = &mut m.default {
                    let mut inner_shadow: HashSet<String> =
                        m.params.iter().map(|p| p.name.clone()).collect();
                    rewrite(&mut d.item, subst, qualified, &mut inner_shadow, true);
                }
            }
        }

        Expression::ImplDecl(i) => {
            for name in &mut i.traits {
                rewrite_name(name, subst, qualified);
            }
            rewrite_type_expr(&mut i.self_ty, subst, qualified);
            // Member bodies are function bodies: their own scope.
            for m in &mut i.members {
                rewrite(&mut m.item, subst, qualified, shadow, true);
            }
        }

        Expression::Match(m) => {
            rewrite(&mut m.subject.item, subst, qualified, shadow, track_let_shadow);
            for arm in &mut m.arms {
                rewrite_pattern(&mut arm.pattern, subst);
                let already_shadowed: Vec<bool> = arm.pattern.binds.iter()
                    .map(|b| shadow.contains(b)).collect();
                for b in &arm.pattern.binds {
                    if b != "_" { shadow.insert(b.clone()); }
                }
                if let Some(g) = &mut arm.guard {
                    rewrite(&mut g.item, subst, qualified, shadow, track_let_shadow);
                }
                rewrite(&mut arm.body.item, subst, qualified, shadow, track_let_shadow);
                for (b, was_shadowed) in arm.pattern.binds.iter().zip(already_shadowed) {
                    if b != "_" && !was_shadowed { shadow.remove(b); }
                }
            }
            if let Some(d) = &mut m.default {
                rewrite(&mut d.item, subst, qualified, shadow, track_let_shadow);
            }
        }

        Expression::IsPattern(ip) => {
            rewrite(&mut ip.subject.item, subst, qualified, shadow, track_let_shadow);
            rewrite_pattern(&mut ip.pattern, subst);
        }

        Expression::Import(_) => unreachable!("Import nodes are stripped before rewrite runs"),

        Expression::Return(value) => {
            if let Some(v) = value {
                rewrite(&mut v.item, subst, qualified, shadow, track_let_shadow);
            }
        }

        Expression::Try(inner) | Expression::Unwrap(inner) => {
            rewrite(&mut inner.item, subst, qualified, shadow, track_let_shadow);
        }

        Expression::Catch { value, handler } => {
            rewrite(&mut value.item, subst, qualified, shadow, track_let_shadow);
            rewrite(&mut handler.item, subst, qualified, shadow, track_let_shadow);
        }

        // The marked name is an ordinary reference for renaming purposes
        // — only *how* it's used at the call site is special, which
        // `TypeChecker::lower_call` handles, not this rewrite.
        Expression::MutArg(name) => rewrite(&mut name.item, subst, qualified, shadow, track_let_shadow),

        Expression::AnnotationDecl(a) => {
            if !track_let_shadow {
                if let Some(mangled) = subst.get(&a.name) {
                    a.name = mangled.clone();
                }
            }
            for p in &mut a.fields {
                rewrite_type_expr(&mut p.ty, subst, qualified);
            }
        }

        // Stripped by `TypeChecker::strip_and_validate_annotations` before
        // any of this runs in practice — module rewriting happens earlier,
        // on the raw parsed AST, so a `Decorated` node can still reach
        // here. Its annotation arguments are literal constants (no
        // identifiers to mangle); only the annotation's own *name* (an
        // `annotation`-namespace reference, mangled the same way its
        // declaration site is above) and the wrapped declaration need it.
        Expression::Decorated(d) => {
            rewrite_annotation_uses(&mut d.annotations, subst);
            rewrite(&mut d.target.item, subst, qualified, shadow, track_let_shadow)
        }
    }
}

/// Rewrite a pattern's (optional) enum-name path and its bare variant name
/// (when unqualified) through `subst`, mirroring how a bare identifier
/// reference is rewritten in `rewrite` above. The pattern's binds are
/// locals, never substitution candidates.
fn rewrite_pattern(pattern: &mut crate::frontend::expression::Pattern, subst: &HashMap<String, String>) {
    if let Some(path) = &mut pattern.path {
        if let Some(mangled) = subst.get(path) {
            *path = mangled.clone();
        }
    } else if let Some(mangled) = subst.get(&pattern.variant) {
        pattern.variant = mangled.clone();
    }
}
