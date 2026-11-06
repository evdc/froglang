//! Parameter conventions: borrow or consume (plans/NOGC.md §1.2, §6 step 1).
//!
//! For each non-`mut` parameter of each `func` in a program, decide whether the
//! callee only reads it (`Borrow`: a caller may pass its value without giving
//! up its own copy) or lets it escape (`Consume`: the value is returned, bound,
//! stored, or put into a literal, so the caller must hand over an owned value).
//! Escape is judged from the body, then propagated through call sites by a
//! fixpoint: a parameter passed on to a consuming parameter consumes too.
//!
//! This is analysis only. Codegen is unchanged. `FROG_DUMP_CONVENTIONS` prints
//! the result, and `FROG_COPY_STATS` counts, under site `call-arg-borrow`, the
//! copies a borrowing callee would make unnecessary.
//!
//! Scope: one program (a file, or one REPL entry). A callee defined in another
//! entry is unknown here and treated as a host function, so the result is only
//! sound for whole-program analysis. Names defined twice are ambiguous and are
//! treated as consuming.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::frontend::tokens::Spanned;
use crate::frontend::typed_ast::{Arg, NodeId, TypedExpr, TypedExprKind};
use crate::frontend::typeck::TypeChecker;

/// What a parameter's value does in its callee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// Only read, or passed on to other borrowing positions.
    Borrow,
    /// Returned, bound, stored, or passed to a consuming position.
    Consume,
}

/// Site index used by `FROG_COPY_STATS` for a call argument that a borrowing
/// callee would not need copied. One past `liveness::CopySite`'s indices.
pub const BORROWED_SITE: i64 = 6;

/// Added to a site index to mark an extraction (`xs[i]`) rather than a `Var`
/// read. Sites `0..=6` are `Var` reads and `7..=13` their extraction twins.
pub const EXTRACT_SITE_BASE: i64 = 7;

/// Why a value escapes: the flow that moves it out of a borrow.
type Reason = &'static str;

#[derive(Debug, Clone)]
pub struct ParamSummary {
    pub name:   String,
    pub is_mut: bool,
    pub conv:   Convention,
    /// The first flow that made a `Consume` parameter escape.
    pub reason: Option<Reason>,
}

#[derive(Debug, Clone)]
pub struct FnSummary {
    pub name:   String,
    pub params: Vec<ParamSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct Conventions {
    /// Every uniquely-named function, sorted by name.
    pub functions: Vec<FnSummary>,
    /// Names defined more than once. They are not analysed, and every call to one consumes.
    pub ambiguous: Vec<String>,
    /// `Var` nodes passed as a call argument to a borrowing position.
    pub borrowed_args: HashSet<NodeId>,
}

impl Conventions {
    /// The convention of parameter `index` of the function `name`, if known.
    pub fn convention(&self, name: &str, index: usize) -> Option<Convention> {
        self.functions.iter().find(|f| f.name == name)?.params.get(index).map(|p| p.conv)
    }

    /// Human-readable dump for `FROG_DUMP_CONVENTIONS`.
    pub fn dump(&self) -> String {
        let mut out = String::from("parameter conventions (plans/NOGC.md §1.2): Borrow = only read, Consume = escapes\n");
        for f in &self.functions {
            let params: Vec<String> = f.params.iter().map(|p| match (p.is_mut, p.conv) {
                (true, _) => format!("{}: mut", p.name),
                (false, Convention::Borrow) => format!("{}: Borrow", p.name),
                (false, Convention::Consume) => format!("{}: Consume ({})", p.name, p.reason.unwrap_or("?")),
            }).collect();
            out += &format!("  func {}({})\n", f.name, params.join(", "));
        }
        for name in &self.ambiguous {
            out += &format!("  func {} — defined more than once, not analysed\n", name);
        }
        out += &format!("  borrowed call arguments: {}\n", self.borrowed_args.len());
        out
    }
}

thread_local! {
    /// The borrowed call-argument nodes of the program being compiled on this thread.
    /// Set by `publish`, read by codegen's copy statistics.
    static BORROWED_ARGS: RefCell<HashSet<NodeId>> = RefCell::new(HashSet::new());
}

/// Make `conv`'s borrowed call arguments visible to codegen on this thread.
pub fn publish(conv: &Conventions) {
    BORROWED_ARGS.with(|s| *s.borrow_mut() = conv.borrowed_args.clone());
}

/// Whether the `Var` node `id` is a call argument that a borrowing callee would not need copied.
pub fn is_borrowed_arg(id: NodeId) -> bool {
    BORROWED_ARGS.with(|s| s.borrow().contains(&id))
}

/// Analyse a whole typed program (after `number_nodes`) and infer every
/// function's parameter conventions.
pub fn infer(program: &Spanned<TypedExpr>) -> Conventions {
    let mut defs = Vec::new();
    collect_defs(program, &mut defs);

    let mut seen = HashSet::new();
    let mut ambiguous_names = HashSet::new();
    for d in &defs {
        if !seen.insert(d.name.clone()) { ambiguous_names.insert(d.name.clone()); }
    }
    let mut table = Table { fns: HashMap::new(), ambiguous: ambiguous_names.clone() };
    for d in &defs {
        if table.ambiguous.contains(&d.name) { continue; }
        table.fns.insert(d.name.clone(), d.params.iter().map(|(_, m)| (*m, Convention::Borrow)).collect());
    }

    // The fixpoint. Conventions only ever move Borrow -> Consume, so this terminates.
    let mut reasons: HashMap<(String, usize), Reason> = HashMap::new();
    loop {
        let mut changed = false;
        for d in &defs {
            if table.ambiguous.contains(&d.name) { continue; }
            let escaping: Vec<(usize, Reason)> = {
                let targets: HashSet<String> = d.params.iter()
                    .filter(|(_, m)| !m).map(|(n, _)| n.clone()).collect();
                let mut scan = Scan { table: &table, targets, reasons: HashMap::new(), borrowed_args: Vec::new() };
                scan.scan(d.body, Some("return"));
                d.params.iter().enumerate()
                    .filter(|(_, (_, m))| !m)
                    .filter_map(|(i, (name, _))| scan.reasons.get(name).map(|r| (i, *r)))
                    .collect()
            };
            for (i, reason) in escaping {
                let slot = &mut table.fns.get_mut(&d.name).expect("analysed function is in the table")[i];
                if slot.1 == Convention::Borrow {
                    slot.1 = Convention::Consume;
                    reasons.insert((d.name.clone(), i), reason);
                    changed = true;
                }
            }
        }
        if !changed { break; }
    }

    // One final pass over the whole program records the borrowed call arguments.
    let mut scan = Scan { table: &table, targets: HashSet::new(), reasons: HashMap::new(), borrowed_args: Vec::new() };
    scan.scan(program, None);
    let borrowed_args: HashSet<NodeId> = scan.borrowed_args.into_iter().collect();

    let mut functions: Vec<FnSummary> = defs.iter()
        .filter(|d| !table.ambiguous.contains(&d.name))
        .map(|d| FnSummary {
            name: d.name.clone(),
            params: d.params.iter().enumerate().map(|(i, (name, is_mut))| ParamSummary {
                name:   name.clone(),
                is_mut: *is_mut,
                conv:   table.fns[&d.name][i].1,
                reason: reasons.get(&(d.name.clone(), i)).copied(),
            }).collect(),
        })
        .collect();
    functions.sort_by(|a, b| a.name.cmp(&b.name));
    let mut ambiguous: Vec<String> = ambiguous_names.into_iter().collect();
    ambiguous.sort();

    Conventions { functions, ambiguous, borrowed_args }
}

/// A named `func` found in the program.
struct FnDef<'a> {
    name:   String,
    params: Vec<(String, bool)>,
    body:   &'a Spanned<TypedExpr>,
}

fn collect_defs<'a>(expr: &'a Spanned<TypedExpr>, out: &mut Vec<FnDef<'a>>) {
    if let TypedExprKind::Assign { name, value } = &expr.item.kind {
        if let TypedExprKind::Function { params, body, .. } = &value.item.kind {
            let body: &Spanned<TypedExpr> = body;
            out.push(FnDef {
                name:   name.clone(),
                params: params.iter().map(|(n, _, m)| (n.clone(), *m)).collect(),
                body,
            });
        }
    }
    for child in TypeChecker::children(expr) {
        collect_defs(child, out);
    }
}

/// Per-name signatures used to resolve a call's convention.
struct Table {
    /// Each uniquely-named function: per parameter, whether it is `mut` and its convention.
    fns: HashMap<String, Vec<(bool, Convention)>>,
    ambiguous: HashSet<String>,
}

enum ArgFlow {
    /// The argument's value escapes, for this reason.
    Escapes(Reason),
    /// The argument is only read.
    Borrows,
}

/// How a value passed as argument `index` of `callable` flows.
fn arg_flow(table: &Table, callable: &Spanned<TypedExpr>, index: usize) -> ArgFlow {
    let TypedExprKind::Var(name) = &callable.item.kind else {
        // A call through a value we cannot name is opaque.
        return ArgFlow::Escapes("call-arg");
    };
    if table.ambiguous.contains(name) {
        return ArgFlow::Escapes("call-arg");
    }
    if let Some(params) = table.fns.get(name) {
        return match params.get(index) {
            Some((_, Convention::Consume)) => ArgFlow::Escapes("call-arg"),
            _ => ArgFlow::Borrows,
        };
    }
    // Not defined in this program: a host function or a stdlib builtin.
    // Host arguments are marshalled, which copies them, so they borrow. `push`
    // is the one builtin that keeps its value argument.
    if name == "push" && index == 1 {
        return ArgFlow::Escapes("push");
    }
    ArgFlow::Borrows
}

/// The flow a child of `expr` takes, given `expr`'s own flow. `None` means only read.
fn child_flow(expr: &Spanned<TypedExpr>, child: &Spanned<TypedExpr>, flow: Option<Reason>) -> Option<Reason> {
    use TypedExprKind as K;
    match &expr.item.kind {
        K::Function { .. } | K::Return(_) => Some("return"),
        K::Assign { .. } => Some("let"),
        K::PlaceAssign { value, .. } if std::ptr::eq(child, &**value) => Some("store"),
        K::List(_) | K::Dict(_) | K::StructInit { .. } | K::VariantInit { .. } => Some("literal"),
        K::Comprehension { body, .. } if std::ptr::eq(child, &**body) => Some("literal"),
        // A block's tail and a conditional's branches carry the whole expression's value.
        K::Block(stmts) if stmts.last().is_some_and(|l| std::ptr::eq(child, l)) => flow,
        K::Conditional { cond, .. } if !std::ptr::eq(child, &**cond) => flow,
        K::Widen { .. } => flow,
        // Extraction (`xs[i]`, `p.f`, a narrowed payload) does not carry the
        // container's escape: NOGC §1.3 makes an extracted value a copy, so the
        // container itself is only read. Codegen does not copy extractions yet
        // (MUTABILITY.md stage 7), so this is the design's rule, not today's.
        _ => None,
    }
}

/// Walks a body and records, per target parameter, the first flow that lets its value escape.
struct Scan<'t> {
    table: &'t Table,
    targets: HashSet<String>,
    reasons: HashMap<String, Reason>,
    borrowed_args: Vec<NodeId>,
}

impl Scan<'_> {
    fn scan(&mut self, expr: &Spanned<TypedExpr>, flow: Option<Reason>) {
        if let (TypedExprKind::Var(name), Some(reason)) = (&expr.item.kind, flow) {
            if self.targets.contains(name) {
                self.reasons.entry(name.clone()).or_insert(reason);
            }
        }
        if let TypedExprKind::Call { callable, args } = &expr.item.kind {
            self.scan(callable, None);
            for (i, arg) in args.iter().enumerate() {
                match arg {
                    Arg::Value(e) => match arg_flow(self.table, callable, i) {
                        ArgFlow::Escapes(reason) => self.scan(e, Some(reason)),
                        ArgFlow::Borrows => {
                            if matches!(e.item.kind, TypedExprKind::Var(_) | TypedExprKind::Index { .. }
                                | TypedExprKind::Slice { .. } | TypedExprKind::FieldAccess { .. }) {
                                self.borrowed_args.push(e.item.id);
                            }
                            self.scan(e, None);
                        }
                    },
                    Arg::Mut(_) => {
                        for sub in arg.subexprs() { self.scan(sub, None); }
                    }
                }
            }
            return;
        }
        for child in TypeChecker::children(expr) {
            let child_flow = child_flow(expr, child, flow);
            self.scan(child, child_flow);
        }
    }
}
