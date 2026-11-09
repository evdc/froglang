//! Parameter conventions: borrow or consume (plans/NOGC.md §1.2, §6 step 1).
//!
//! For each non-`mut` parameter of each `func` in a program, decide whether the
//! callee only reads it (`Borrow`: a caller may pass its value without giving
//! up its own copy) or lets it escape (`Consume`: the value is returned, bound,
//! stored, or put into a literal, so the caller must hand over an owned value).
//! Escape is judged from the body, then propagated through call sites by a
//! fixpoint: a parameter passed on to a consuming parameter consumes too.
//!
//! Codegen reads the result through `publish`: a borrowed argument stays its
//! caller's to free, and a borrowed parameter is never freed by its callee.
//! `FROG_DUMP_CONVENTIONS` prints the result.
//!
//! Scope: one program (a file, or one REPL entry). A callee an earlier REPL
//! entry defined is resolved through that entry's `Signatures`, which
//! `FrogState` keeps. Names defined twice in one entry are ambiguous and are
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

/// Each analysed function's per-parameter conventions, by name. What a later
/// REPL entry needs to call a function an earlier entry compiled: the callee
/// already frees exactly the parameters it consumes, so its callers must agree.
pub type Signatures = HashMap<String, Vec<Convention>>;

#[derive(Debug, Clone, Default)]
pub struct Conventions {
    /// Every uniquely-named function, sorted by name.
    pub functions: Vec<FnSummary>,
    /// Names defined more than once. They are not analysed, and every call to one consumes.
    pub ambiguous: Vec<String>,
    /// Call arguments (any expression) passed to a borrowing position. The
    /// caller keeps ownership of these values and drops them itself.
    pub borrowed_args: HashSet<NodeId>,
    /// Per analysed function, keyed by its body's node id: whether each
    /// parameter is borrowed. A `mut` parameter is never borrowed.
    pub borrowed_params: HashMap<NodeId, Vec<bool>>,
}

impl Conventions {
    /// The convention of parameter `index` of the function `name`, if known.
    pub fn convention(&self, name: &str, index: usize) -> Option<Convention> {
        self.functions.iter().find(|f| f.name == name)?.params.get(index).map(|p| p.conv)
    }

    /// Whether parameter `index` of the function whose body is `body` is borrowed.
    pub fn is_borrowed_param(&self, body: NodeId, index: usize) -> bool {
        self.borrowed_params.get(&body).and_then(|v| v.get(index)).copied().unwrap_or(false)
    }

    /// Record this program's functions in `sigs`, for later entries: an
    /// analysed function replaces any earlier one of its name, and an
    /// ambiguous name is forgotten, so calls to it consume.
    pub fn record_signatures(&self, sigs: &mut Signatures) {
        for f in &self.functions {
            sigs.insert(f.name.clone(), f.params.iter().map(|p| p.conv).collect());
        }
        for name in &self.ambiguous {
            sigs.remove(name);
        }
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
    /// Set by `publish`, read by codegen.
    static BORROWED_ARGS: RefCell<HashSet<NodeId>> = RefCell::new(HashSet::new());
    static BORROWED_PARAMS: RefCell<HashMap<NodeId, Vec<bool>>> = RefCell::new(HashMap::new());
}

/// Make `conv`'s borrowed call arguments and parameters visible to codegen on this thread.
pub fn publish(conv: &Conventions) {
    BORROWED_ARGS.with(|s| *s.borrow_mut() = conv.borrowed_args.clone());
    BORROWED_PARAMS.with(|s| *s.borrow_mut() = conv.borrowed_params.clone());
}

/// Whether the expression `id` is a call argument that its callee only borrows.
pub fn is_borrowed_arg(id: NodeId) -> bool {
    BORROWED_ARGS.with(|s| s.borrow().contains(&id))
}

/// Whether parameter `index` of the function whose body node is `body` is borrowed
/// (see `Conventions::is_borrowed_param`). Unknown functions own their parameters.
pub fn is_borrowed_param(body: NodeId, index: usize) -> bool {
    BORROWED_PARAMS.with(|s| s.borrow().get(&body).and_then(|v| v.get(index)).copied().unwrap_or(false))
}

/// Builtins that codegen special-cases by name and that only read their
/// arguments, so a call to one borrows them. Runtime primitives and host
/// functions are added by the caller (`Codegen::imported_callees`).
pub fn builtin_borrowers() -> HashSet<String> {
    ["print", "len", "gc_dump", "panic", "keys", "values", "to_list"]
        .into_iter().map(String::from).collect()
}

/// Analyse a whole typed program (after `number_nodes`) and infer every
/// function's parameter conventions.
///
/// `external_borrowers` names the callees that are not defined in `program`
/// but are known to only read their arguments (host functions and builtins).
/// `prior` holds the functions earlier REPL entries defined. Any other name
/// that is not defined in `program` is treated as consuming.
pub fn infer(program: &Spanned<TypedExpr>, external_borrowers: &HashSet<String>, prior: &Signatures) -> Conventions {
    let mut defs = Vec::new();
    collect_defs(program, &mut defs);

    let mut seen = HashSet::new();
    let mut ambiguous_names = HashSet::new();
    for d in &defs {
        if !seen.insert(d.name.clone()) { ambiguous_names.insert(d.name.clone()); }
    }
    let mut table = Table {
        fns: HashMap::new(),
        ambiguous: ambiguous_names.clone(),
        external_borrowers: external_borrowers.clone(),
        prior,
    };
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
    let borrowed_params: HashMap<NodeId, Vec<bool>> = defs.iter()
        .filter(|d| !table.ambiguous.contains(&d.name))
        .map(|d| (d.body.item.id, d.params.iter().enumerate()
            .map(|(i, (_, is_mut))| !is_mut && table.fns[&d.name][i].1 == Convention::Borrow)
            .collect()))
        .collect();

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

    Conventions { functions, ambiguous, borrowed_args, borrowed_params }
}

/// A named `func` found in the program.
struct FnDef<'a> {
    name:   String,
    params: Vec<(String, bool)>,
    body:   &'a Spanned<TypedExpr>,
}

fn collect_defs<'a>(expr: &'a Spanned<TypedExpr>, out: &mut Vec<FnDef<'a>>) {
    if let TypedExprKind::Assign { name, value, .. } = &expr.item.kind {
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
struct Table<'p> {
    /// Each uniquely-named function: per parameter, whether it is `mut` and its convention.
    fns: HashMap<String, Vec<(bool, Convention)>>,
    ambiguous: HashSet<String>,
    /// Callees defined outside the program that only read their arguments.
    external_borrowers: HashSet<String>,
    /// Functions earlier REPL entries defined.
    prior: &'p Signatures,
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
    // `push` is the one builtin that keeps its value argument.
    if name == "push" && index == 1 {
        return ArgFlow::Escapes("push");
    }
    // Not defined in this program. A function an earlier entry defined keeps
    // the conventions it was compiled with.
    if let Some(params) = table.prior.get(name) {
        return match params.get(index) {
            Some(Convention::Consume) => ArgFlow::Escapes("call-arg"),
            _ => ArgFlow::Borrows,
        };
    }
    // A known host function or builtin borrows: host arguments are
    // marshalled, which copies them. Anything else is unknown, so it consumes.
    if table.external_borrowers.contains(name) {
        return ArgFlow::Borrows;
    }
    ArgFlow::Escapes("call-arg")
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
    table: &'t Table<'t>,
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
                            self.borrowed_args.push(e.item.id);
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
