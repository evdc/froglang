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
    Expression, FieldAccessExpr, ImportKind, LiteralExpr,
};
use crate::frontend::parser::{ParseError, Parser};
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
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleError::Io { path, message } => {
                write!(f, "could not read module '{}': {}", path.display(), message)
            }
            ModuleError::Parse { path, errors } => {
                write!(f, "parse error(s) in module '{}':", path.display())?;
                for e in errors { write!(f, " {:?}", e)?; }
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
        }
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
            Expression::Function(f) => walk(&f.body.item, path),
            Expression::Call(c) => {
                walk(&c.callable.item, path)?;
                for a in &c.args { walk(&a.item, path)?; }
                Ok(())
            }
            Expression::Tuple(es) | Expression::Block(es) => {
                for e in es { walk(&e.item, path)?; }
                Ok(())
            }
            Expression::Annotated(a) => { walk(&a.expr.item, path)?; walk(&a.ty.item, path) }
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
            Expression::DataDecl(_) | Expression::Literal(_) => Ok(()),
        }
    }
    for s in stmts {
        // The statement itself may legitimately be an Import (that's the
        // valid top-level case) — only its *children* are checked.
        match &s.item {
            Expression::Import(_) => {}
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
        }
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
                rewrite(&mut ty.item, subst, qualified, shadow, track_let_shadow);
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
                    rewrite(&mut ty.item, subst, qualified, shadow, track_let_shadow);
                }
            }
            if let Some(rt) = &mut func.return_type {
                rewrite(&mut rt.item, subst, qualified, shadow, track_let_shadow);
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

        Expression::Tuple(es) | Expression::Block(es) => {
            for e in es {
                rewrite(&mut e.item, subst, qualified, shadow, track_let_shadow);
            }
        }

        Expression::Annotated(a) => {
            rewrite(&mut a.expr.item, subst, qualified, shadow, track_let_shadow);
            rewrite(&mut a.ty.item, subst, qualified, shadow, track_let_shadow);
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
            }
            for p in &mut d.fields {
                if let Some(ty) = &mut p.ty {
                    rewrite(&mut ty.item, subst, qualified, shadow, track_let_shadow);
                }
            }
        }

        Expression::Import(_) => unreachable!("Import nodes are stripped before rewrite runs"),
    }
}
