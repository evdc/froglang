use std::collections::HashMap;
use std::path::Path;

use crate::codegen::JitCodegen;
use crate::frontend::parser::ParseError;
use crate::frontend::modules;
use crate::frontend::typeck::{Type, TypeChecker};
use crate::frontend::tokens::Spanned;
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};
use crate::frontend::expression::Expression;
use crate::runtime::gc::{GcHeap, ACTIVE_HEAP};
use crate::runtime;

// ── Public value type ─────────────────────────────────────────────────────────

/// A froglang value returned across the embedding boundary.
/// Strings and lists are deep-copied out of the GC heap on return.
#[derive(Debug, Clone)]
pub enum FrogValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    List(Vec<FrogValue>),
    /// In insertion order (`runtime::gc::FrogDict`'s doc comment). Like
    /// `List` above, a struct-typed value decodes wrong (only leaf 0 of
    /// its flattened layout) — the same pre-existing `FrogValue`
    /// limitation `List`'s identical `frog_list_get(.., 0)` already has,
    /// not something new here.
    Dict(Vec<(FrogValue, FrogValue)>),
    None,
}

impl FrogValue {
    pub fn from_bits(bits: i64, ty: &Type, _heap: &GcHeap) -> Self {
        if let Some(inner_ty) = ty.as_list_elem() {
            let len = runtime::ffi::frog_list_len(bits) as usize;
            let elems = (0..len)
                .map(|i| {
                    let elem = runtime::ffi::frog_list_get(bits, i as i64, 0);
                    FrogValue::from_bits(elem, inner_ty, _heap)
                })
                .collect();
            return FrogValue::List(elems);
        }
        if let Some((key_ty, val_ty)) = ty.as_dict_kv() {
            let len = runtime::dict::frog_dict_len(bits) as usize;
            let pairs = (0..len)
                .map(|i| {
                    let k = runtime::dict::frog_dict_slot(bits, i as i64, 0);
                    let v = runtime::dict::frog_dict_slot(bits, i as i64, 1);
                    (FrogValue::from_bits(k, key_ty, _heap), FrogValue::from_bits(v, val_ty, _heap))
                })
                .collect();
            return FrogValue::Dict(pairs);
        }
        match ty {
            Type::Int  => FrogValue::Int(bits),
            Type::Float => FrogValue::Float(f64::from_bits(bits as u64)),
            Type::Bool  => FrogValue::Bool(bits != 0),
            Type::Str   => {
                let s = unsafe {
                    runtime::gc::frog_str_as_str(bits as *const runtime::gc::FrogStr)
                };
                FrogValue::Str(s.to_owned())
            },
            // Struct values aren't yet representable in the embedding API's
            // `FrogValue` (they're multi-slot in the JIT ABI — see
            // `struct_fields` in codegen/mod.rs — while every other
            // `FrogValue` variant round-trips through exactly one `i64`).
            // Not reachable from `compile_and_run`/test code as long as
            // struct values only ever appear as locals, not as a bare
            // top-level result or REPL binding — see the struct-support plan.
            // Struct/enum values aren't yet representable in the embedding
            // API's `FrogValue` — see the comment above for structs; an
            // enum value is a single `i64` (a `FrogVariant` pointer) but
            // decoding it generically would need the enum's field layout,
            // which isn't threaded through here.
            // `Never` is never a value's actual runtime type — nothing of
            // that type is ever produced — but the match must stay exhaustive.
            // A `Named` reaching here is always a plain struct (`List` was
            // handled above), so it falls into the same "not yet
            // representable" bucket.
            Type::None | Type::Function { .. } | Type::Union(_) | Type::TypeVar { .. } | Type::Named { .. } | Type::Never => {
                FrogValue::None
            },
        }
    }

    /// A display string for this value (without the type annotation).
    pub fn display_str(&self) -> String {
        match self {
            FrogValue::Int(n)    => format!("{}", n),
            // Frog notation, not Rust's — a REPL result is a value the user
            // may well paste back in. See `crate::notation`.
            FrogValue::Float(f)  => crate::notation::float_repr(*f),
            FrogValue::Bool(b)   => format!("{}", b),
            FrogValue::Str(s)    => crate::notation::escape_str(s),
            FrogValue::List(v)   => format!(
                "[{}]",
                v.iter().map(|e| e.display_str()).collect::<Vec<_>>().join(", ")
            ),
            FrogValue::Dict(pairs) => if pairs.is_empty() {
                "[:]".to_string()
            } else {
                format!(
                    "[{}]",
                    pairs.iter().map(|(k, v)| format!("{}: {}", k.display_str(), v.display_str())).collect::<Vec<_>>().join(", ")
                )
            },
            FrogValue::None      => String::new(),
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum FrogError {
    Parse(Vec<Spanned<ParseError>>),
    /// A module-resolution failure (bad import path, cycle, unknown
    /// export, etc) — see `frontend::modules::ModuleError`.
    Module(String),
    Type(String),
    /// Codegen hit an internal panic (e.g. an unsupported construct the type
    /// checker currently lets through — see `codegen::mod`'s `unimplemented!`
    /// and `panic!` sites). Recovered via `catch_unwind` in `eval`, which
    /// also rolls back the `Codegen`-internal state that panic would
    /// otherwise have corrupted, so this `FrogState` stays usable afterward.
    Codegen(String),
}

impl std::fmt::Display for FrogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrogError::Parse(errs) => {
                // A single lex error routinely cascades into several
                // downstream parse errors at the same (or nearby) span —
                // dedup identical (span, error) pairs so the user sees each
                // distinct problem once, not three copies of it.
                let mut seen: Vec<&Spanned<ParseError>> = Vec::new();
                for e in errs {
                    if !seen.iter().any(|s| s.span == e.span && s.item == e.item) {
                        seen.push(e);
                    }
                }
                for (i, e) in seen.iter().enumerate() {
                    if i > 0 { writeln!(f)?; }
                    write!(f, "{}: {}", e.span.start, e.item)?;
                }
                Ok(())
            },
            FrogError::Module(msg)  => write!(f, "Module error: {}", msg),
            FrogError::Type(msg)    => write!(f, "Type error: {}", msg),
            FrogError::Codegen(msg) => write!(f, "Codegen error: {}", msg),
        }
    }
}

/// Extract a human-readable message from a `catch_unwind` payload. Panics
/// via `panic!("{}", ...)` / `.expect(...)` carry a `&'static str` or
/// `String`; anything else (a custom payload type) falls back to a generic
/// message rather than failing to report the error at all.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "codegen panicked with a non-string payload".to_string()
    }
}

// ── Source map ───────────────────────────────────────────────────────────────

/// The filename and source text `eval`/`eval_file` compiled as one entry —
/// `plans/DATA.md` Stage 3's "`FrogState` retaining entry sources". Indexed
/// by `entry_id` (`FrogState::entry_sources[entry_id]`), the same number
/// `Codegen::source_map`'s `FnSourceInfo::entry_id` refers back to, and the
/// same number mangled into that entry's JIT symbols
/// (`{name}__frogfn{entry_id}` in `codegen::Codegen::compile_entry`).
#[derive(Debug, Clone)]
pub struct EntrySource {
    pub filename: String,
    pub source: String,
}

// ── FrogState ─────────────────────────────────────────────────────────────────

/// An independent froglang interpreter instance.
/// All state — heap, type-checker, JIT module, variable environment — is owned here.
/// Multiple `FrogState`s can coexist on different threads with no coordination.
pub struct FrogState {
    pub heap:         GcHeap,
    pub tc:           TypeChecker,
    pub codegen:      JitCodegen,
    /// One `i64` per flattened leaf field of the binding's type (see
    /// `struct_fields` in `codegen/mod.rs`) — a single element for every
    /// non-struct type, matching how it always worked before structs
    /// existed; more than one for a struct-typed binding.
    pub env:          HashMap<String, Vec<i64>>,
    pub env_types:    HashMap<String, Type>,
    pub string_arena: Vec<Vec<u8>>,
    pub entry_count:  usize,
    /// One entry per successful `eval`/`eval_file` call, in `entry_id`
    /// order — see `EntrySource`. A failed entry (type error, codegen panic)
    /// never gets one, matching `entry_count`'s own "only successful entries
    /// consume a number" behavior below.
    pub entry_sources: Vec<EntrySource>,
}

impl Default for FrogState {
    fn default() -> Self {
        FrogState::builder().build().expect("FrogState::builder().build() with no host functions cannot fail")
    }
}

impl FrogState {
    /// Look up which entry a `Codegen::source_map` `FnSourceInfo` came from
    /// — `plans/DATA.md` Stage 3. `None` only if `entry_id` is somehow out
    /// of range, which shouldn't happen: every `FnSourceInfo` is pushed by a
    /// `compile_entry` call whose `entry_id` becomes a valid
    /// `entry_sources` index the moment that same call succeeds.
    pub fn entry_source(&self, entry_id: usize) -> Option<&EntrySource> {
        self.entry_sources.get(entry_id)
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Start building a `FrogState` with host (Rust) functions registered —
    /// see `plans/EMBEDDING.md` and `crate::host`.
    pub fn builder() -> FrogStateBuilder {
        FrogStateBuilder {
            hosts: Vec::new(),
            prelude: Vec::new(),
            data_audits: Vec::new(),
            dict_backend: std::sync::Arc::new(crate::runtime::dict::HashbrownBackend),
        }
    }

    /// `FrogState::builder()` with the stdlib (`crate::stdlib`) installed —
    /// see `plans/STDLIB.md`. `FrogState::new()` deliberately doesn't do
    /// this itself: it's what the existing test suite calls expecting
    /// today's minimal (`print`/`push`/`panic`/`gc_dump`-only) surface, so
    /// the stdlib is opt-in. The CLI (`main.rs`) opts in.
    pub fn with_stdlib() -> Result<Self, FrogError> {
        crate::stdlib::install(FrogState::builder()).build()
    }

    /// Set `ACTIVE_HEAP` to this state's heap, call `func_ptr` with the
    /// out-buffer pointer, then clear it.
    fn call_jit(&mut self, func_ptr: fn(i64) -> i64, out_ptr: i64, out_gc_slots: Vec<usize>) -> i64 {
        // `func_ptr` writes each top-level binding into `out_ptr` as soon as
        // it is created, and the roots below are only rebuilt after this
        // returns — so for the length of the call that buffer is the only
        // thing keeping some of those values reachable. See
        // `GcHeap::push_scanned_span`.
        self.heap.push_scanned_span(out_ptr as *const i64, out_gc_slots);
        ACTIVE_HEAP.with(|p| p.set(&mut self.heap as *mut GcHeap));
        // Publish a `FrogCtx` a host call can fetch via `frog_ctx_current`
        // (`runtime::host`) — built from raw pointers into `self.heap`/
        // `self.tc`'s tables, valid only for the duration of this call
        // (`FrogCtx::new`'s safety doc). Struct/union defs never move once
        // registered (`TypeChecker::struct_defs`/`union_defs`), so this is
        // sound even though `compile_entry` above may have just grown them.
        let mut host_ctx = unsafe {
            crate::runtime::host::FrogCtx::new(
                &mut self.heap as *mut GcHeap,
                self.tc.struct_defs() as *const _,
                self.tc.union_defs() as *const _,
            )
        };
        let result = crate::runtime::host::with_active_ctx(&mut host_ctx, || func_ptr(out_ptr));
        ACTIVE_HEAP.with(|p| p.set(std::ptr::null_mut()));
        self.heap.pop_scanned_span();
        result
    }

    /// Parse, type-check, compile, and run `src` in this state's context.
    /// Returns the result value and its type.
    ///
    /// On a type error, or a codegen panic (an unsupported construct the
    /// type checker currently lets through), the type-checker and codegen
    /// state are both rolled back to exactly how they were before this call,
    /// so a failed `eval` never leaves the `FrogState` unusable for the next
    /// one.
    ///
    /// `src`'s own `import` statements (if any) are resolved relative to
    /// the current working directory — this is the REPL/no-file entry
    /// point. See `eval_file` for a file-backed entry, where imports
    /// resolve relative to that file's own directory instead.
    pub fn eval(&mut self, src: &str) -> Result<(FrogValue, Type), FrogError> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        self.eval_with_base(src, &cwd.join("<repl>"))
    }

    /// Read, parse, type-check, compile, and run the file at `path`.
    /// `import` statements in it (and transitively, in every file it
    /// imports) resolve relative to each importing file's own directory.
    pub fn eval_file(&mut self, path: &Path) -> Result<(FrogValue, Type), FrogError> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| FrogError::Module(format!("could not read '{}': {}", path.display(), e)))?;
        self.eval_with_base(&src, path)
    }

    fn eval_with_base(&mut self, src: &str, base_path: &Path) -> Result<(FrogValue, Type), FrogError> {
        // Captured up front so every `FrogError::Type` render below can name
        // where the offending span actually lives — `plans/DATA.md` Stage 3.
        let filename = base_path.display().to_string();

        let stmts = modules::resolve_source(src, base_path).map_err(|e| FrogError::Module(e.to_string()))?;
        let span = match (stmts.first(), stmts.last()) {
            (Some(first), Some(last)) => first.span.merge(last.span),
            _ => crate::frontend::tokens::Span::new((0, 0), (0, 0)),
        };
        let ast = Spanned::from(Expression::Block(stmts), span);

        let cp = self.tc.checkpoint();
        let mut typed = match self.tc.check_and_lower_entry(ast) {
            Ok(t) => t,
            Err(e) => {
                self.tc.restore(cp);
                return Err(FrogError::Type(crate::diagnostics::render_span(&filename, src, e.span, &e.item.msg)));
            }
        };
        if let Err(e) = self.tc.validate_codegen_constraints(&typed) {
            self.tc.restore(cp);
            return Err(FrogError::Type(crate::diagnostics::render_span(&filename, src, e.span, &e.item.msg)));
        }
        // `TRAITS.md` Stage 3b: real monomorphization. Clones and
        // specializes every generic call site's declaration into a
        // distinctly-named instantiation (mutating `typed` in place —
        // stripping any raw, un-substituted generic declaration and
        // inserting its compiled instantiations instead, with every call
        // site rewritten to the matching mangled name) rather than
        // rejecting a second concrete type the way Stage 2's gate did.
        if let Err(e) = self.tc.monomorphize_generics(&mut typed) {
            self.tc.restore(cp);
            return Err(FrogError::Type(crate::diagnostics::render_span(&filename, src, e.span, &e.item.msg)));
        }
        // Tier 1 function values: hoist every lambda and nested `func` to
        // the top level, turn captures into parameters, and specialize
        // every higher-order callee on the function it was passed. Must
        // run after monomorphization (a generic higher-order function is
        // specialized per *type* first, then per function argument) and
        // before `desugar_notation`, whose synthesized `Var` callables
        // this pass must not see.
        if let Err(e) = self.tc.lower_function_values(&mut typed) {
            self.tc.restore(cp);
            return Err(FrogError::Type(crate::diagnostics::render_span(&filename, src, e.span, &e.item.msg)));
        }
        // `plans/DATA.md` stage 5: expand every `repr(...)` placeholder
        // left by `TypeChecker::lower_call`'s `is_repr` arm into its
        // per-type notation. Must run after monomorphization (every type
        // in the tree is fully substituted only now — see
        // `TypeChecker::desugar_notation`'s own doc comment) and before
        // `number_nodes` (so the nodes it synthesizes get numbered too).
        if let Err(e) = self.tc.desugar_notation(&mut typed) {
            self.tc.restore(cp);
            return Err(FrogError::Type(crate::diagnostics::render_span(&filename, src, e.span, &e.item.msg)));
        }
        // Stamp every node with a fresh id — including whatever
        // `monomorphize_generics`/`desugar_notation` just cloned or
        // synthesized in, which starts out unnumbered (`NodeId`'s doc
        // comment) — before codegen's liveness-dependent passes touch the
        // tree. Must run after both, not before, so newly-inserted bodies
        // get real ids too.
        crate::frontend::liveness::number_nodes(&mut typed);

        // `TRAITS.md` Part 7: `Linear` violations are type errors, and must
        // be caught here — before codegen, which has no way to report a
        // clean `Spanned<TypeError>` (it only ever panics on a bad tree).
        {
            let stmts: &[Spanned<TypedExpr>] = match &typed.item.kind {
                TypedExprKind::Block(s) => s.as_slice(),
                _ => std::slice::from_ref(&typed),
            };
            let prior_exit_live: crate::frontend::liveness::NameSet = self.env_types.keys().cloned().collect();
            if let Err(e) = crate::frontend::linear::check(stmts, &prior_exit_live, &self.tc) {
                self.tc.restore(cp);
                return Err(FrogError::Type(crate::diagnostics::render_span(&filename, src, e.span, &e.item.msg)));
            }
        }

        let result_ty = typed.item.ty.clone();

        // `bindings` is every top-level `let`/assignment made in this entry
        // (in source order) — not just the last one, and not conflated with
        // this entry's own result value (see `build_main_body` in codegen
        // for how `out_ptr` is populated).
        //
        // Wrapped in `catch_unwind`: codegen still panics internally on
        // unsupported constructs (see codegen/mod.rs) rather than returning
        // a `Result`, so this is what stands between one bad `eval` call and
        // a permanently broken `FrogState` (see `Codegen::reset_builder_ctx`
        // for why the panic itself would otherwise corrupt reusable state).
        let func_ids_snap = self.codegen.checkpoint_func_ids();
        // `plans/DATA.md` Stage 3: `compile_entry`'s Pass 1 appends to
        // `Codegen::source_map` before Pass 2 can panic, so a failed entry
        // must roll that back too, same reasoning as `func_ids_snap`.
        let source_map_snap = self.codegen.checkpoint_source_map();
        let entry_count = self.entry_count;
        let compile_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.codegen.compile_entry(
                typed,
                &mut self.string_arena,
                entry_count,
                &self.env,
                &self.env_types,
                self.tc.struct_defs(),
                self.tc.union_defs(),
            )
        }));

        let (main_id, bindings) = match compile_result {
            Ok(pair) => pair,
            Err(panic_payload) => {
                self.codegen.restore_func_ids(func_ids_snap);
                self.codegen.restore_source_map(source_map_snap);
                self.codegen.reset_builder_ctx();
                self.tc.restore(cp);
                return Err(FrogError::Codegen(panic_message(&*panic_payload)));
            }
        };
        // `entry_sources[entry_count]` now lines up with every
        // `FnSourceInfo::entry_id == entry_count` this call just pushed —
        // both are keyed by the same `entry_count` captured above.
        self.entry_sources.push(EntrySource { filename, source: src.to_string() });
        self.entry_count += 1;

        let ptr = self.codegen.module.get_finalized_function(main_id);
        let func_ptr: fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };

        // Each binding occupies `struct_fields(ty, structs).len()` slots
        // (1 for every non-struct type) — see `build_main_body`'s doc
        // comment in codegen/mod.rs for the write side of this protocol.
        let total_slots: usize = bindings.iter()
            .map(|(_, ty)| crate::codegen::struct_fields(ty, self.tc.struct_defs()).len())
            .sum();
        let mut out_buf: Vec<i64> = vec![0i64; total_slots];
        let out_ptr = out_buf.as_mut_ptr() as i64;
        let out_gc_slots = crate::codegen::gc_slots_of_bindings(&bindings, self.tc.struct_defs());
        let bits = self.call_jit(func_ptr, out_ptr, out_gc_slots);

        let mut cursor = 0usize;
        for (name, ty) in &bindings {
            let width = crate::codegen::struct_fields(ty, self.tc.struct_defs()).len();
            self.env.insert(name.clone(), out_buf[cursor..cursor + width].to_vec());
            self.env_types.insert(name.clone(), ty.clone());
            cursor += width;
        }

        // Rebuild the GC's explicit root set from scratch every time, from
        // exactly what's currently live: this entry's own result (which
        // needs to survive long enough for `FrogValue::from_bits` below)
        // plus every heap-typed binding still in `env`. The old code only
        // ever pushed roots and never popped them, so every string or list
        // any entry had ever produced — including ones since shadowed or
        // rebound — stayed alive for the process's lifetime.
        self.heap.clear_roots();
        // `Type::Union` belongs here alongside `Str`/`List`: a union value
        // is a GC-visible word (`codegen::is_heap_ty`) exactly like the
        // other two. It used to be missing, so the result of an entry that
        // produced a `data`-union value was left unrooted for as long as
        // `from_bits` needed it below.
        //
        // Not a bare `matches!` on `result_ty`, though: `bits` is only ever
        // *leaf 0* of the result's flattened value (`build_main_body`'s
        // return is always a single i64), and whether that leaf is a
        // scannable column is a property of the column, not of the whole
        // type — route through `heap_roots_in_leaves` so it is decided the
        // same way the bindings loop below decides it.
        //
        // `get(..1)` rather than `[..1]`: a zero-leaf result type (`Type::None`
        // flattens to one leaf, but a struct with no fields flattens to none)
        // would otherwise panic on the slice.
        let result_leaf0 = crate::codegen::struct_fields(&result_ty, self.tc.struct_defs());
        for v in crate::codegen::heap_roots_in_leaves(&[bits], result_leaf0.get(..1).unwrap_or_default()) {
            self.heap.push_root(v, true);
        }
        for (name, ty) in &self.env_types {
            if let Some(vals) = self.env.get(name) {
                let leafs = crate::codegen::struct_fields(ty, self.tc.struct_defs());
                // Not a plain per-leaf `matches!` on the type: which
                // flattened leaves are GC-scannable columns is
                // `struct_fields`'s business, not something to re-derive
                // here. See `codegen::heap_roots_in_leaves`.
                for v in crate::codegen::heap_roots_in_leaves(vals, &leafs) {
                    self.heap.push_root(v, true);
                }
            }
        }

        self.heap.maybe_collect();

        let value = FrogValue::from_bits(bits, &result_ty, &self.heap);
        Ok((value, result_ty))
    }
}

// ── FrogStateBuilder ─────────────────────────────────────────────────────────

/// Names the frontend matches syntactically rather than by scope lookup
/// (`typeck.rs`'s `"print"`/`"push"` special cases, plus `panic` and its
/// reserved `!`-desugaring alias) — registering a host function under one
/// of these would silently never be called, since the special case wins
/// before the generic `func_ids` lookup ever runs. Rejected at `build()`
/// with a clear error instead.
const RESERVED_NAMES: &[&str] = &["print", "push", "len", "get", "keys", "values", "remove", "panic", "panic!builtin", "gc_dump", "repr", "read", "json"];

/// Builds a `FrogState` with host (Rust) functions registered before the
/// JIT module exists — required because `JITBuilder::symbol` only accepts
/// new symbols at construction, not afterward (see `plans/EMBEDDING.md`).
pub struct FrogStateBuilder {
    hosts:   Vec<crate::host::HostFn>,
    prelude: Vec<String>,
    data_audits: Vec<DataAudit>,
    dict_backend: std::sync::Arc<dyn crate::runtime::dict::DictBackend>,
}

/// What `FrogStateBuilder::data::<T>()` records for `build()`'s layout
/// audit — see that method's doc comment.
struct DataAudit {
    rust_type_name: &'static str,
    frog_type: Type,
    leaves: Vec<Type>,
}

impl FrogStateBuilder {
    /// Register a host function, callable from frog source under
    /// `host.name`. See `#[frog_fn]` (`froglang_macros`) for the ordinary
    /// way to build a `HostFn`.
    pub fn func(mut self, host: crate::host::HostFn) -> Self {
        self.hosts.push(host);
        self
    }

    /// Evaluate `src` before any user code — the mechanism for declaring
    /// host `data` types (`error IoError(msg: Str)`) a host function's
    /// signature refers to, since it reuses the ordinary parser/typeck path
    /// rather than a separate Rust-side type-definition API.
    pub fn prelude(mut self, src: impl Into<String>) -> Self {
        self.prelude.push(src.into());
        self
    }

    /// Register a Rust struct or enum that marshals to/from a frog `data`
    /// type — see `#[derive(FrogData)]`/`#[derive(FrogUnion)]`
    /// (`froglang_macros`), which implement `FrogDecl` alongside
    /// `ToFrog`/`FromFrog`. `T::frog_decl()`, if `Some`, is appended to the
    /// prelude (an ordinary `.prelude(...)` call under the hood — the
    /// generated `data`/`error` declaration reuses the same parser/typeck
    /// path); a `#[frog(declared)]` type returns `None` and must already be
    /// declared by an earlier `.prelude(...)`/`.data(...)` call.
    ///
    /// `T::leaves()` is computed compositionally (`FromFrog`/`ToFrog`'s doc
    /// comments) with no `StructDefs` lookup — that is what keeps
    /// marshalling AOT-clean, but it means a mistake (a field reordered on
    /// one side and not the other, a hand-written impl that forgot to
    /// override `leaves()` for a compound type) would otherwise surface as
    /// a silent slot-layout desync at the first call. `build()` audits
    /// every type registered here against the real, registered
    /// `codegen::struct_fields` output once the prelude has run, and fails
    /// loudly if they disagree — see `build()`.
    pub fn data<T: crate::host::ToFrog + crate::host::FrogDecl>(mut self) -> Self {
        if let Some(decl) = T::frog_decl() {
            self.prelude.push(decl);
        }
        self.data_audits.push(DataAudit {
            rust_type_name: std::any::type_name::<T>(),
            frog_type: T::frog_type(),
            leaves: T::leaves(),
        });
        self
    }

    /// Swap the index every `Dict` this `FrogState` allocates uses —
    /// `runtime::dict::HashbrownBackend` (the default) unless overridden
    /// here. The seam is deliberately narrow: a `DictBackend` only ever
    /// sees a hash and candidate entry indices, never a froglang value,
    /// the GC, or the word encoding — see `runtime::dict`'s module doc
    /// comment.
    pub fn dict_backend(mut self, backend: std::sync::Arc<dyn crate::runtime::dict::DictBackend>) -> Self {
        self.dict_backend = backend;
        self
    }

    /// Finish building. Fails if two host functions share a name, or a
    /// host function claims a name the frontend already treats specially
    /// (`RESERVED_NAMES`) — both are configuration errors in the embedder,
    /// not something `eval` should discover later as a confusing shadowing
    /// failure.
    pub fn build(self) -> Result<FrogState, FrogError> {
        let mut seen = std::collections::HashSet::new();
        for host in &self.hosts {
            if RESERVED_NAMES.contains(&host.name) {
                return Err(FrogError::Type(format!(
                    "'{}' is a reserved builtin name and can't be registered as a host function", host.name
                )));
            }
            if !seen.insert(host.name) {
                return Err(FrogError::Type(format!(
                    "host function '{}' is registered more than once", host.name
                )));
            }
        }

        // Symbols and their uniform (ctx, args, out) import signatures are
        // declared before any frog type exists — see `Codegen::new_with_hosts`,
        // which also rejects a host name that collides with a runtime
        // primitive's `func_ids` key.
        let codegen = JitCodegen::new_with_hosts(&self.hosts).map_err(FrogError::Type)?;

        let mut heap = GcHeap::new();
        heap.dict_backend = self.dict_backend.clone();
        let mut state = FrogState {
            heap,
            tc:           TypeChecker::new(),
            codegen,
            env:          HashMap::new(),
            env_types:    HashMap::new(),
            string_arena: Vec::new(),
            entry_count:  0,
            entry_sources: Vec::new(),
        };

        // Host `data` types, if any, before anything references them.
        // `ReadError` is already present — `TypeChecker::new()` seeds it
        // directly, the way `Range`/`Iterable`/`Container` are seeded,
        // rather than being evaluated here: evaluating it would consume an
        // `entry_id`/`entry_sources` slot ahead of the embedder's own first
        // entry (`test_source_map.rs` pins the first *user* eval at entry
        // 0), and `read` needs to be unconditionally available the way
        // `print` is, not gated behind a builder call.
        for src in &self.prelude {
            state.eval(src)?;
        }

        // `data::<T>()`'s one-time layout audit: every registered type's
        // compositional `leaves()` must agree with what the frog side
        // actually flattens it to (`codegen::struct_fields`, which already
        // covers both structs and inline unions). A `#[frog(declared)]`
        // type must already be present in `struct_defs` by now — from an
        // earlier `.prelude(...)`/`.data(...)` call — or this reports it as
        // unknown rather than silently marshalling against a layout that
        // doesn't exist.
        //
        // The same comparison serves two callers, so it lives in one
        // closure: `what` names the thing being audited in the error.
        let check_leaves = |what: &str, frog_type: &Type, claimed: &[Type]| -> Result<(), FrogError> {
            let real_leaves: Vec<Type> = crate::codegen::struct_fields(frog_type, state.tc.struct_defs())
                .into_iter()
                .map(|(_, t)| t)
                .collect();
            if real_leaves != claimed {
                return Err(FrogError::Type(format!(
                    "{} (frog type {}): leaves() reports {:?} but the registered frog \
                     layout is {:?} — declaration and marshalling disagree. If this type wasn't \
                     declared via #[frog(declared)], check its field order; if it was, make sure \
                     the frog declaration it maps onto was registered earlier (an explicit \
                     .prelude(...) or an earlier .data(...) call).",
                    what, frog_type, claimed, real_leaves,
                )));
            }
            Ok(())
        };

        for audit in &self.data_audits {
            check_leaves(&format!("host type {}", audit.rust_type_name), &audit.frog_type, &audit.leaves)?;
        }

        // Host *signatures* get the same audit, in both directions: a type
        // that only ever appears in a `#[frog_fn]` signature — a
        // `Result<T, E>`, a `Vec<T>`, a `#[frog(declared)]` struct never
        // passed to `.data::<T>()` — is registered nowhere else, so without
        // this its layout desync would surface as silently corrupt slots at
        // the first call rather than as an error here. Parameters are
        // checked against `FromFrog::leaves()` and the result against
        // `ToFrog::leaves()` (the descriptor carries both), so a hand-written
        // pair that disagrees between the two directions is caught too.
        for host in &self.hosts {
            if host.param_leaves.len() != host.params.len() {
                return Err(FrogError::Type(format!(
                    "host function '{}': descriptor lists {} parameter types but {} parameter leaf \
                     lists — a hand-written HostFn must fill `param_leaves` from the same \
                     `FromFrog::leaves()` calls `#[frog_fn]` emits.",
                    host.name, host.params.len(), host.param_leaves.len(),
                )));
            }
            for (i, (pty, pleaves)) in host.params.iter().zip(&host.param_leaves).enumerate() {
                check_leaves(&format!("host function '{}' parameter {}", host.name, i), pty, pleaves)?;
            }
            check_leaves(&format!("host function '{}' return type", host.name), &host.ret, &host.ret_leaves)?;
        }

        // Install each host function's frog-visible name and type into the
        // checker's global scope — after the prelude, so a signature built
        // against a prelude-declared struct/union name (once that's
        // supported — see `plans/EMBEDDING.md`'s follow-ups) would resolve.
        state.tc.add_ctx(self.hosts.iter().map(|h| {
            (h.name.to_string(), Type::Function { params: h.params.clone(), result: Box::new(h.ret.clone()) })
        }));

        Ok(state)
    }
}
