use std::collections::HashMap;

use cranelift_codegen::ir::{condcodes::{FloatCC, IntCC}, types, AbiParam, InstBuilder, MemFlags, StackSlot, StackSlotData, StackSlotKind, Value};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};
use crate::frontend::typeck::{Type, numeric_join};
use crate::runtime::ffi;

pub struct Codegen {
    pub module: JITModule,
    func_ids: HashMap<String, FuncId>,
    builder_ctx: FunctionBuilderContext,
}

/// Per-function-compilation context threaded through `compile_expr`.
///
/// `heap_slot`/`heap_cursor` implement a conservative shadow stack: every
/// syntactic subexpression that *produces* a new heap pointer (string
/// allocation, string concat, list allocation, or a call returning
/// `Str`/`List`) is stored into a dedicated stack-slot cell immediately after
/// it is computed, so the GC's mark phase can find it even though it's only
/// live in an SSA register. Functions with no heap-typed subexpressions get
/// `heap_slot: None` and pay no runtime cost.
struct Ctx<'a> {
    func_ids:      &'a HashMap<String, FuncId>,
    module:        &'a mut JITModule,
    string_arena:  &'a mut Vec<Vec<u8>>,
    heap_slot:     Option<StackSlot>,
    heap_cursor:   usize,
    /// Next unused `Variable` index for mutable-local codegen (see
    /// `get_or_declare_var`). Each function-body compile starts a fresh
    /// counter (0-based) — `Variable` indices only need to be unique within
    /// a single `FunctionBuilder`, not globally.
    var_counter:   u32,
}

/// True iff a value of this type is a GC-managed heap pointer.
fn is_heap_ty(ty: &Type) -> bool {
    matches!(ty, Type::Str | Type::List(_))
}

/// Walk `expr` in exactly the recursion pattern `compile_expr` uses (including
/// skipping over nested `Function` bodies, which are compiled separately) and
/// invoke `f` once for every subexpression that allocates a new heap pointer.
fn for_each_heap_producer(expr: &Spanned<TypedExpr>, f: &mut impl FnMut()) {
    match &expr.item.kind {
        TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_)
        | TypedExprKind::BoolLit(_) | TypedExprKind::Var(_) => {},

        TypedExprKind::StrLit(_) => f(),

        TypedExprKind::Unary { expr: inner, .. } => for_each_heap_producer(inner, f),

        TypedExprKind::Binary { op, left, right } => {
            for_each_heap_producer(left, f);
            for_each_heap_producer(right, f);
            // Only Str + Str (concat) allocates; Str == / != Str yields Bool.
            if *op == Token::Plus && left.item.ty == Type::Str {
                f();
            }
        },

        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            for_each_heap_producer(cond, f);
            for_each_heap_producer(true_branch, f);
            if let Some(fb) = false_branch {
                for_each_heap_producer(fb, f);
            }
        },

        TypedExprKind::Call { callable, args } => {
            for_each_heap_producer(callable, f);
            for arg in args { for_each_heap_producer(arg, f); }
            if is_heap_ty(&expr.item.ty) { f(); }
        },

        TypedExprKind::Index { target, index } => {
            for_each_heap_producer(target, f);
            for_each_heap_producer(index, f);
            // A heap-typed element read out of a list isn't a fresh
            // allocation, but it needs its own shadow-stack root all the
            // same: once read, it's only reachable from the containing
            // list, which may itself go unrooted (e.g. a temporary list
            // literal) before this value is done being used.
            if is_heap_ty(&expr.item.ty) { f(); }
        },

        TypedExprKind::Slice { target, start, end } => {
            for_each_heap_producer(target, f);
            if let Some(s) = start { for_each_heap_producer(s, f); }
            if let Some(e) = end { for_each_heap_producer(e, f); }
            // Unlike Index, a slice always allocates a brand-new list.
            f();
        },

        TypedExprKind::Range { start, end } => {
            for_each_heap_producer(start, f);
            for_each_heap_producer(end, f);
            f(); // always allocates the materialized list
        },

        TypedExprKind::Assign { value, .. } => {
            // Mirrors compile_expr's Assign arm, which never visits a
            // Function value (it's compiled separately as a top-level fn).
            if !matches!(value.item.kind, TypedExprKind::Function { .. }) {
                for_each_heap_producer(value, f);
            }
        },

        TypedExprKind::Function { .. } => {},

        TypedExprKind::List(elems) => {
            for e in elems { for_each_heap_producer(e, f); }
            f();
        },

        TypedExprKind::Block(stmts) => {
            for s in stmts { for_each_heap_producer(s, f); }
        },

        TypedExprKind::ForLoop { iterable, cond, body, .. } => {
            for_each_heap_producer(iterable, f);
            // Reading a heap-typed element out of the list each iteration
            // needs its own root, same reasoning as `Index` above — see
            // `compile_for_loop`'s `root_heap_value(bcx, ctx, elem)` call.
            if elem_ty_is_heap(iterable) { f(); }
            if let Some(c) = cond { for_each_heap_producer(c, f); }
            for_each_heap_producer(body, f);
        },

        TypedExprKind::Comprehension { iterable, cond, body, .. } => {
            // The result list is allocated (and rooted) before the loop
            // starts — see `compile_expr`'s `Comprehension` arm.
            f();
            for_each_heap_producer(iterable, f);
            if elem_ty_is_heap(iterable) { f(); }
            if let Some(c) = cond { for_each_heap_producer(c, f); }
            for_each_heap_producer(body, f);
        },
    }
}

/// True iff `iterable`'s element type (`iterable.item.ty` is always
/// `List(elem)` after type checking) is heap-managed.
fn elem_ty_is_heap(iterable: &Spanned<TypedExpr>) -> bool {
    matches!(&iterable.item.ty, Type::List(inner) if is_heap_ty(inner))
}

/// Count the heap-pointer-producing subexpressions in `expr` — the number of
/// shadow-stack slots its compiled function needs.
fn count_heap_slots(expr: &Spanned<TypedExpr>) -> usize {
    let mut n = 0usize;
    for_each_heap_producer(expr, &mut || n += 1);
    n
}

/// Store a freshly-produced heap pointer into the next shadow-stack slot, if
/// this function has one (no-op for functions with no heap-typed values).
fn root_heap_value(bcx: &mut FunctionBuilder, ctx: &mut Ctx, val: Value) {
    if let Some(slot) = ctx.heap_slot {
        let offset = (ctx.heap_cursor * 8) as i32;
        bcx.ins().stack_store(val, slot, offset);
        ctx.heap_cursor += 1;
    }
}

/// If `n > 0`, allocate an `n`-slot stack region and register it as a GC
/// shadow-stack frame via `frog_frame_push`. Returns `None` — and emits no
/// IR at all — for functions with no heap-typed subexpressions.
fn setup_shadow_frame(
    bcx: &mut FunctionBuilder,
    module: &mut JITModule,
    func_ids: &HashMap<String, FuncId>,
    n: usize,
) -> Option<StackSlot> {
    if n == 0 { return None; }
    let slot = bcx.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        (n * 8) as u32,
        3, // 8-byte aligned (align_shift = log2(8))
    ));
    let base = bcx.ins().stack_addr(types::I64, slot, 0);
    let len_val = bcx.ins().iconst(types::I64, n as i64);
    let func_id = func_ids["frog_frame_push"];
    let callee = module.declare_func_in_func(func_id, bcx.func);
    bcx.ins().call(callee, &[base, len_val]);
    Some(slot)
}

/// Pop the shadow-stack frame pushed by `setup_shadow_frame`, if any.
/// Must run on every path out of the function, before `return_`.
fn teardown_shadow_frame(
    bcx: &mut FunctionBuilder,
    module: &mut JITModule,
    func_ids: &HashMap<String, FuncId>,
    slot: Option<StackSlot>,
) {
    if slot.is_none() { return; }
    let func_id = func_ids["frog_frame_pop"];
    let callee = module.declare_func_in_func(func_id, bcx.func);
    bcx.ins().call(callee, &[]);
}

/// Look up `name`'s Cranelift `Variable`, declaring a fresh one (with `ty`'s
/// Cranelift type) the first time this name is bound. Reusing the same
/// `Variable` across reassignments — instead of a raw `Value` in a plain
/// `HashMap`, as this codebase used to do — is what lets Cranelift's own SSA
/// construction (`use_var`/`def_var`) insert the phi nodes a reassignment
/// inside a diverging branch or loop body needs; a raw `Value` computed in
/// one block is only valid in blocks it dominates, which a branch/loop body
/// generally isn't, and reading it back afterward is a codegen-time
/// dominance-verifier crash (see [[project_reassignment_dominance_bug]] —
/// this function exists to fix that class of bug).
fn get_or_declare_var(
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
    name: &str,
    ty: &Type,
) -> Variable {
    if let Some(&v) = vars.get(name) {
        return v;
    }
    let v = Variable::from_u32(ctx.var_counter);
    ctx.var_counter += 1;
    bcx.declare_var(v, cl_type(ty));
    vars.insert(name.to_string(), v);
    v
}

/// Declare-and-bind a name's `Variable` using a plain counter rather than a
/// `Ctx` — used for function-parameter and pre-seeded-REPL-binding setup,
/// which both run before `Ctx` is constructed (see `build_func_body`,
/// `build_main_body`).
fn declare_and_def_var(
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    counter: &mut u32,
    name: &str,
    ty: &Type,
    val: Value,
) {
    let v = Variable::from_u32(*counter);
    *counter += 1;
    bcx.declare_var(v, cl_type(ty));
    vars.insert(name.to_string(), v);
    bcx.def_var(v, val);
}

fn cl_type(ty: &Type) -> types::Type {
    match ty {
        Type::Int   => types::I64,
        Type::Bool  => types::I8,
        Type::Float => types::F64,
        _           => types::I64,
    }
}

/// Widen a value from its source Cranelift type to the target type, if needed.
fn ensure_width(val: Value, from_ty: &Type, to: types::Type, bcx: &mut FunctionBuilder) -> Value {
    let from = cl_type(from_ty);
    if from == to {
        val
    } else if from == types::I8 && to == types::I64 {
        bcx.ins().uextend(types::I64, val)
    } else {
        val
    }
}

/// Emit Cranelift IR to widen `val` from `from_ty` to `to_ty`.
fn coerce_value(val: Value, from_ty: &Type, to_ty: &Type, bcx: &mut FunctionBuilder) -> Value {
    use crate::frontend::typeck::widens_to;
    if from_ty == to_ty { return val; }
    assert!(widens_to(from_ty, to_ty), "no widening from {:?} to {:?}", from_ty, to_ty);
    match (from_ty, to_ty) {
        (Type::Int, Type::Float) => bcx.ins().fcvt_from_sint(types::F64, val),
        _ => unreachable!(),
    }
}

/// Convert a value of `ty` to the raw i64 wire representation used to pass
/// results across the JIT boundary: Float is bitcast, Bool is zero-extended,
/// and Int/Str/List(_) are already i64-shaped (the latter two are pointers).
fn to_i64_repr(bcx: &mut FunctionBuilder, ty: &Type, val: Value) -> Value {
    match ty {
        Type::Float => bcx.ins().bitcast(types::I64, MemFlags::new(), val),
        Type::Bool  => bcx.ins().uextend(types::I64, val),
        _           => val,
    }
}

/// Inverse of `to_i64_repr`: convert a raw i64 wire value (e.g. read back out
/// of a `FrogList`'s flat i64 buffer via `frog_list_get`) into `ty`'s native
/// Cranelift representation.
fn from_i64_repr(bcx: &mut FunctionBuilder, ty: &Type, val: Value) -> Value {
    match ty {
        Type::Float => bcx.ins().bitcast(types::F64, MemFlags::new(), val),
        Type::Bool  => bcx.ins().ireduce(types::I8, val),
        _           => val,
    }
}

/// Declare a runtime import function in the module and insert its FuncId.
fn declare_rt(
    module: &mut JITModule,
    func_ids: &mut HashMap<String, FuncId>,
    sym_name: &str,   // name in the JIT symbol table / linker
    key: &str,        // key in func_ids (may differ to create aliases like "print")
    params: &[types::Type],
    ret: Option<types::Type>,
) {
    let mut sig = module.make_signature();
    for &p in params { sig.params.push(AbiParam::new(p)); }
    if let Some(r) = ret { sig.returns.push(AbiParam::new(r)); }
    let id = module.declare_function(sym_name, Linkage::Import, &sig)
        .unwrap_or_else(|e| panic!("declare '{}' failed: {}", sym_name, e));
    func_ids.insert(key.to_string(), id);
}

/// Compile a typed expression into Cranelift IR, returning its SSA value.
///
/// `ctx.string_arena` keeps source `Vec<u8>` buffers alive until the JIT
/// executes; `frog_alloc_str` copies bytes immediately, so the arena only
/// needs to outlive the call to the compiled function.
///
/// Every subexpression that allocates a new heap pointer (see
/// `for_each_heap_producer`) is stored into `ctx`'s shadow-stack slot via
/// `root_heap_value` immediately after being produced, so it stays visible to
/// the GC's mark phase for the remainder of this function's execution.
fn compile_expr(
    expr: &Spanned<TypedExpr>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Value {
    match &expr.item.kind {
        TypedExprKind::IntLit(n) => bcx.ins().iconst(types::I64, *n),

        TypedExprKind::BoolLit(b) => bcx.ins().iconst(types::I8, *b as i64),

        TypedExprKind::FloatLit(f) => bcx.ins().f64const(*f),

        TypedExprKind::StrLit(s) => {
            let bytes = s.as_bytes().to_vec();
            let ptr = bytes.as_ptr() as i64;
            let len = bytes.len() as i64;
            ctx.string_arena.push(bytes);  // keep alive until after JIT call

            let data_val = bcx.ins().iconst(types::I64, ptr);
            let len_val  = bcx.ins().iconst(types::I64, len);

            let func_id = ctx.func_ids["frog_alloc_str"];
            let callee  = ctx.module.declare_func_in_func(func_id, bcx.func);
            let call    = bcx.ins().call(callee, &[data_val, len_val]);
            let result  = bcx.inst_results(call)[0];
            root_heap_value(bcx, ctx, result);
            result
        },

        TypedExprKind::Var(name) => {
            let var = *vars.get(name.as_str())
                .unwrap_or_else(|| panic!("unbound variable in codegen: {}", name));
            bcx.use_var(var)
        },

        TypedExprKind::Unary { op, expr: inner } => {
            let v = compile_expr(inner, bcx, vars, ctx);
            match op {
                Token::Minus => {
                    if inner.item.ty == Type::Float {
                        bcx.ins().fneg(v)
                    } else {
                        let zero = bcx.ins().iconst(types::I64, 0);
                        bcx.ins().isub(zero, v)
                    }
                },
                Token::Not => {
                    let one = bcx.ins().iconst(types::I8, 1);
                    bcx.ins().bxor(v, one)
                },
                _ => unimplemented!("unary op {:?}", op),
            }
        },

        TypedExprKind::Binary { op, left, right } => {
            // ── String operations (must short-circuit before numeric path) ──
            if left.item.ty == Type::Str {
                let lv = compile_expr(left,  bcx, vars, ctx);
                let rv = compile_expr(right, bcx, vars, ctx);
                return match op {
                    Token::Plus => {
                        let id     = ctx.func_ids["frog_str_concat"];
                        let callee = ctx.module.declare_func_in_func(id, bcx.func);
                        let call   = bcx.ins().call(callee, &[lv, rv]);
                        let result = bcx.inst_results(call)[0];
                        root_heap_value(bcx, ctx, result);
                        result
                    },
                    Token::EqEq | Token::NotEq => {
                        let id     = ctx.func_ids["frog_str_eq"];
                        let callee = ctx.module.declare_func_in_func(id, bcx.func);
                        let call   = bcx.ins().call(callee, &[lv, rv]);
                        let result = bcx.inst_results(call)[0];
                        if *op == Token::NotEq {
                            let one   = bcx.ins().iconst(types::I64, 1);
                            let xored = bcx.ins().bxor(result, one);
                            bcx.ins().ireduce(types::I8, xored)
                        } else {
                            bcx.ins().ireduce(types::I8, result)
                        }
                    },
                    Token::Lt | Token::Gt | Token::LtEq | Token::GtEq => {
                        let id     = ctx.func_ids["frog_str_cmp"];
                        let callee = ctx.module.declare_func_in_func(id, bcx.func);
                        let call   = bcx.ins().call(callee, &[lv, rv]);
                        let cmp    = bcx.inst_results(call)[0];
                        let zero   = bcx.ins().iconst(types::I64, 0);
                        let cc = match op {
                            Token::Lt   => IntCC::SignedLessThan,
                            Token::Gt   => IntCC::SignedGreaterThan,
                            Token::LtEq => IntCC::SignedLessThanOrEqual,
                            Token::GtEq => IntCC::SignedGreaterThanOrEqual,
                            _ => unreachable!(),
                        };
                        bcx.ins().icmp(cc, cmp, zero)
                    },
                    _ => unimplemented!("string binary op {:?}", op),
                };
            }

            // ── Logical and/or (must short-circuit — `right` can have side
            // effects, e.g. `print`, and must not run when `left` already
            // decides the result) ────────────────────────────────────────
            if *op == Token::And || *op == Token::Or {
                let lv = compile_expr(left, bcx, vars, ctx);

                let rhs_bb   = bcx.create_block();
                let merge_bb = bcx.create_block();
                bcx.append_block_param(merge_bb, types::I8);

                if *op == Token::And {
                    // `false and right` == false, without evaluating `right`.
                    let zero = bcx.ins().iconst(types::I8, 0);
                    bcx.ins().brif(lv, rhs_bb, &[], merge_bb, &[zero]);
                } else {
                    // `true or right` == true, without evaluating `right`.
                    let one = bcx.ins().iconst(types::I8, 1);
                    bcx.ins().brif(lv, merge_bb, &[one], rhs_bb, &[]);
                }

                bcx.switch_to_block(rhs_bb);
                bcx.seal_block(rhs_bb);
                let rv = compile_expr(right, bcx, vars, ctx);
                bcx.ins().jump(merge_bb, &[rv]);

                bcx.switch_to_block(merge_bb);
                bcx.seal_block(merge_bb);
                return bcx.block_params(merge_bb)[0];
            }

            // ── Numeric operations ──────────────────────────────────────────
            let lv = compile_expr(left,  bcx, vars, ctx);
            let rv = compile_expr(right, bcx, vars, ctx);
            let op_ty = numeric_join(&left.item.ty, &right.item.ty).unwrap_or_else(|| left.item.ty.clone());
            let lv = coerce_value(lv, &left.item.ty, &op_ty, bcx);
            let rv = coerce_value(rv, &right.item.ty, &op_ty, bcx);
            let is_float = op_ty == Type::Float;
            match op {
                Token::Plus  => if is_float { bcx.ins().fadd(lv, rv) } else { bcx.ins().iadd(lv, rv) },
                Token::Minus => if is_float { bcx.ins().fsub(lv, rv) } else { bcx.ins().isub(lv, rv) },
                Token::Star  => if is_float { bcx.ins().fmul(lv, rv) } else { bcx.ins().imul(lv, rv) },
                Token::Slash => if is_float { bcx.ins().fdiv(lv, rv) } else { bcx.ins().sdiv(lv, rv) },
                Token::EqEq  => if is_float { bcx.ins().fcmp(FloatCC::Equal,               lv, rv) } else { bcx.ins().icmp(IntCC::Equal,                    lv, rv) },
                Token::NotEq => if is_float { bcx.ins().fcmp(FloatCC::NotEqual,            lv, rv) } else { bcx.ins().icmp(IntCC::NotEqual,                 lv, rv) },
                Token::Lt    => if is_float { bcx.ins().fcmp(FloatCC::LessThan,            lv, rv) } else { bcx.ins().icmp(IntCC::SignedLessThan,            lv, rv) },
                Token::Gt    => if is_float { bcx.ins().fcmp(FloatCC::GreaterThan,         lv, rv) } else { bcx.ins().icmp(IntCC::SignedGreaterThan,         lv, rv) },
                Token::LtEq  => if is_float { bcx.ins().fcmp(FloatCC::LessThanOrEqual,    lv, rv) } else { bcx.ins().icmp(IntCC::SignedLessThanOrEqual,     lv, rv) },
                Token::GtEq  => if is_float { bcx.ins().fcmp(FloatCC::GreaterThanOrEqual,  lv, rv) } else { bcx.ins().icmp(IntCC::SignedGreaterThanOrEqual,  lv, rv) },
                // Token::And/Or are handled above, before `rv` is computed,
                // so they short-circuit — they never reach this match.
                _ => unimplemented!("binary op {:?}", op),
            }
        },

        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            let cond_val = compile_expr(cond, bcx, vars, ctx);

            let true_bb  = bcx.create_block();
            let false_bb = bcx.create_block();
            let merge_bb = bcx.create_block();

            let has_value = expr.item.ty != Type::None;
            let result_ty = cl_type(&expr.item.ty);
            if has_value {
                bcx.append_block_param(merge_bb, result_ty);
            }

            bcx.ins().brif(cond_val, true_bb, &[], false_bb, &[]);

            bcx.switch_to_block(true_bb);
            bcx.seal_block(true_bb);
            let tv = compile_expr(true_branch, bcx, vars, ctx);
            if has_value {
                let tv = ensure_width(tv, &true_branch.item.ty, result_ty, bcx);
                bcx.ins().jump(merge_bb, &[tv]);
            } else {
                bcx.ins().jump(merge_bb, &[]);
            }

            bcx.switch_to_block(false_bb);
            bcx.seal_block(false_bb);
            if has_value {
                let fv = if let Some(fb) = false_branch {
                    let v = compile_expr(fb, bcx, vars, ctx);
                    ensure_width(v, &fb.item.ty, result_ty, bcx)
                } else {
                    bcx.ins().iconst(result_ty, 0)
                };
                bcx.ins().jump(merge_bb, &[fv]);
            } else {
                if let Some(fb) = false_branch {
                    compile_expr(fb, bcx, vars, ctx);
                }
                bcx.ins().jump(merge_bb, &[]);
            }

            bcx.switch_to_block(merge_bb);
            bcx.seal_block(merge_bb);

            if has_value {
                bcx.block_params(merge_bb)[0]
            } else {
                bcx.ins().iconst(types::I64, 0)
            }
        },

        TypedExprKind::Call { callable, args } => {
            let func_name = match &callable.item.kind {
                TypedExprKind::Var(name) => name.clone(),
                _ => panic!("only named function calls supported in codegen"),
            };
            let func_id = ctx.func_ids[&func_name];
            let local_callee = ctx.module.declare_func_in_func(func_id, bcx.func);

            let param_types: Vec<Type> = match &callable.item.ty {
                Type::Function { params, .. } => params.clone(),
                _ => vec![],
            };
            let return_ty: Type = match &callable.item.ty {
                Type::Function { result, .. } => *result.clone(),
                _ => Type::Int,
            };

            let mut arg_vals: Vec<Value> = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                let mut v = compile_expr(a, bcx, vars, ctx);
                if let Some(param_ty) = param_types.get(i) {
                    v = coerce_value(v, &a.item.ty, param_ty, bcx);
                }
                arg_vals.push(v);
            }

            let call = bcx.ins().call(local_callee, &arg_vals);
            if return_ty == Type::None {
                bcx.ins().iconst(types::I64, 0)
            } else {
                let result = bcx.inst_results(call)[0];
                if is_heap_ty(&return_ty) {
                    root_heap_value(bcx, ctx, result);
                }
                result
            }
        },

        TypedExprKind::Index { target, index } => {
            let list_val = compile_expr(target, bcx, vars, ctx);
            let idx_val  = compile_expr(index, bcx, vars, ctx);

            let get_id = ctx.func_ids["frog_list_get"];
            let callee = ctx.module.declare_func_in_func(get_id, bcx.func);
            let call   = bcx.ins().call(callee, &[list_val, idx_val]);
            let raw    = bcx.inst_results(call)[0];

            let result = from_i64_repr(bcx, &expr.item.ty, raw);
            if is_heap_ty(&expr.item.ty) {
                root_heap_value(bcx, ctx, result);
            }
            result
        },

        TypedExprKind::Slice { target, start, end } => {
            let list_val = compile_expr(target, bcx, vars, ctx);
            // `frog_list_slice` treats i64::MIN/i64::MAX as "bound omitted"
            // sentinels (see its doc comment) — realistic indices never hit
            // these, so there's no ambiguity with an explicit bound.
            let start_val = match start {
                Some(s) => compile_expr(s, bcx, vars, ctx),
                None => bcx.ins().iconst(types::I64, i64::MIN),
            };
            let end_val = match end {
                Some(e) => compile_expr(e, bcx, vars, ctx),
                None => bcx.ins().iconst(types::I64, i64::MAX),
            };

            let id     = ctx.func_ids["frog_list_slice"];
            let callee = ctx.module.declare_func_in_func(id, bcx.func);
            let call   = bcx.ins().call(callee, &[list_val, start_val, end_val]);
            let result = bcx.inst_results(call)[0];
            root_heap_value(bcx, ctx, result);
            result
        },

        TypedExprKind::Range { start, end } => {
            let start_val = compile_expr(start, bcx, vars, ctx);
            let end_val   = compile_expr(end, bcx, vars, ctx);

            let id     = ctx.func_ids["frog_range"];
            let callee = ctx.module.declare_func_in_func(id, bcx.func);
            let call   = bcx.ins().call(callee, &[start_val, end_val]);
            let result = bcx.inst_results(call)[0];
            root_heap_value(bcx, ctx, result);
            result
        },

        TypedExprKind::Assign { name, value } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                bcx.ins().iconst(types::I64, 0)
            } else {
                let val = compile_expr(value, bcx, vars, ctx);
                let var = get_or_declare_var(bcx, vars, ctx, name, &value.item.ty);
                bcx.def_var(var, val);
                val
            }
        },

        TypedExprKind::Block(stmts) => {
            let mut last = bcx.ins().iconst(types::I64, 0);
            for stmt in stmts {
                last = compile_expr(stmt, bcx, vars, ctx);
            }
            last
        },

        TypedExprKind::Function { .. } => {
            bcx.ins().iconst(types::I64, 0)
        },

        TypedExprKind::List(elems) => {
            // Determine elem_tag from the list's element type.
            let elem_tag: i64 = match &expr.item.ty {
                Type::List(inner) => match inner.as_ref() {
                    Type::Str | Type::List(_) => 1,
                    _ => 0,
                },
                _ => 0,
            };

            let n = elems.len() as i64;
            let cap_val = bcx.ins().iconst(types::I64, n.max(1));
            let tag_val = bcx.ins().iconst(types::I64, elem_tag);

            let alloc_id = ctx.func_ids["frog_alloc_list"];
            let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[cap_val, tag_val]);
            let list_ptr = bcx.inst_results(alloc_call)[0];
            // Root the list itself *before* compiling its elements: an
            // element expression (e.g. a Str) can allocate and trigger a
            // collection, and the list must already be reachable by then.
            root_heap_value(bcx, ctx, list_ptr);

            let push_id = ctx.func_ids["frog_list_push"];
            for elem in elems {
                let ev = compile_expr(elem, bcx, vars, ctx);
                // The list's backing store is a flat i64 buffer (see
                // FrogList in runtime/gc.rs); Float and Bool elements need
                // the same bitcast/zero-extend conversion applied to every
                // other i64-wire-format value (see to_i64_repr). Without
                // this, pushing an F64 or I8 SSA value into an i64-typed
                // call argument is a Cranelift type mismatch — a "Verifier
                // errors" panic, not a bug in the pushed value itself.
                let ev = to_i64_repr(bcx, &elem.item.ty, ev);
                let push_ref = ctx.module.declare_func_in_func(push_id, bcx.func);
                let push_call = bcx.ins().call(push_ref, &[list_ptr, ev]);
                let _ = bcx.inst_results(push_call)[0];
            }

            list_ptr
        },

        TypedExprKind::ForLoop { var, iterable, cond, body } => {
            compile_for_loop(var, iterable, cond, body, None, bcx, vars, ctx);
            bcx.ins().iconst(types::I64, 0)
        },

        TypedExprKind::Comprehension { var, iterable, cond, body } => {
            let elem_tag: i64 = if is_heap_ty(&body.item.ty) { 1 } else { 0 };
            let cap_val = bcx.ins().iconst(types::I64, 1);
            let tag_val = bcx.ins().iconst(types::I64, elem_tag);

            let alloc_id = ctx.func_ids["frog_alloc_list"];
            let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[cap_val, tag_val]);
            let result_list = bcx.inst_results(alloc_call)[0];
            // Root the result list before the loop runs at all: it must
            // already be reachable by the time the first pushed element
            // (or the iterable itself) can trigger a collection.
            root_heap_value(bcx, ctx, result_list);

            compile_for_loop(var, iterable, cond, body, Some(result_list), bcx, vars, ctx);
            result_list
        },
    }
}

/// Shared codegen for `for var in iterable (if cond)? body`, used by both
/// `TypedExprKind::ForLoop` (bare loop, `result_list: None`) and
/// `TypedExprKind::Comprehension` (`result_list: Some(list_ptr)`, into
/// which each `body` evaluation is pushed).
///
/// Uses Cranelift block params to carry the loop index across iterations
/// (the same pattern as the `Conditional`/short-circuit `and`/`or` merge
/// blocks above) rather than `Variable`/`declare_var`, which this codebase
/// doesn't otherwise use.
#[allow(clippy::too_many_arguments)]
fn compile_for_loop(
    var: &str,
    iterable: &Spanned<TypedExpr>,
    cond: &Option<Box<Spanned<TypedExpr>>>,
    body: &Spanned<TypedExpr>,
    result_list: Option<Value>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) {
    let list_val = compile_expr(iterable, bcx, vars, ctx);
    let elem_ty = match &iterable.item.ty {
        Type::List(inner) => (**inner).clone(),
        other => unreachable!("for-loop iterable must be a List after type checking, got {}", other),
    };

    let len_id = ctx.func_ids["frog_list_len"];
    let len_callee = ctx.module.declare_func_in_func(len_id, bcx.func);
    let len_call = bcx.ins().call(len_callee, &[list_val]);
    let len_val = bcx.inst_results(len_call)[0];

    let header_bb = bcx.create_block();
    let body_bb   = bcx.create_block();
    let exit_bb   = bcx.create_block();
    bcx.append_block_param(header_bb, types::I64);

    let zero = bcx.ins().iconst(types::I64, 0);
    bcx.ins().jump(header_bb, &[zero]);

    // `header_bb` has a second predecessor — the back-edge jump emitted at
    // the end of this function — so it can't be sealed until that jump
    // exists. Its sole use of `switch_to_block` before that is fine;
    // sealing (not switching) is what Cranelift requires deferred.
    bcx.switch_to_block(header_bb);
    let i = bcx.block_params(header_bb)[0];
    let in_range = bcx.ins().icmp(IntCC::SignedLessThan, i, len_val);
    bcx.ins().brif(in_range, body_bb, &[], exit_bb, &[]);

    bcx.switch_to_block(body_bb);
    bcx.seal_block(body_bb);

    let get_id = ctx.func_ids["frog_list_get"];
    let get_callee = ctx.module.declare_func_in_func(get_id, bcx.func);
    let get_call = bcx.ins().call(get_callee, &[list_val, i]);
    let raw = bcx.inst_results(get_call)[0];
    let elem = from_i64_repr(bcx, &elem_ty, raw);
    if is_heap_ty(&elem_ty) {
        root_heap_value(bcx, ctx, elem);
    }
    let var_id = get_or_declare_var(bcx, vars, ctx, var, &elem_ty);
    bcx.def_var(var_id, elem);

    // Optional `if` filter in the loop header: skip straight to the
    // increment (without running `body`) when it's false.
    if let Some(c) = cond {
        let do_bb   = bcx.create_block();
        let skip_bb = bcx.create_block();
        let cond_val = compile_expr(c, bcx, vars, ctx);
        bcx.ins().brif(cond_val, do_bb, &[], skip_bb, &[]);

        bcx.switch_to_block(skip_bb);
        bcx.seal_block(skip_bb);
        let i_next = bcx.ins().iadd_imm(i, 1);
        bcx.ins().jump(header_bb, &[i_next]);

        bcx.switch_to_block(do_bb);
        bcx.seal_block(do_bb);
    }

    let body_val = compile_expr(body, bcx, vars, ctx);
    if let Some(list_ptr) = result_list {
        let pushed = to_i64_repr(bcx, &body.item.ty, body_val);
        let push_id = ctx.func_ids["frog_list_push"];
        let push_callee = ctx.module.declare_func_in_func(push_id, bcx.func);
        bcx.ins().call(push_callee, &[list_ptr, pushed]);
    }

    let i_next = bcx.ins().iadd_imm(i, 1);
    bcx.ins().jump(header_bb, &[i_next]);
    bcx.seal_block(header_bb);

    bcx.switch_to_block(exit_bb);
    bcx.seal_block(exit_bb);
}

impl Codegen {
    /// Snapshot `func_ids` before a `compile_entry` call that might panic
    /// partway through (e.g. after Pass 1 has declared this entry's
    /// functions but before Pass 2 finishes defining them). Pair with
    /// `restore_func_ids` on failure so a half-declared entry's functions
    /// don't linger in the name→FuncId map pointing at undefined code.
    pub fn checkpoint_func_ids(&self) -> HashMap<String, FuncId> {
        self.func_ids.clone()
    }

    pub fn restore_func_ids(&mut self, snapshot: HashMap<String, FuncId>) {
        self.func_ids = snapshot;
    }

    /// Replace `builder_ctx` with a fresh one. `FunctionBuilder::new` asserts
    /// its `FunctionBuilderContext` is empty, and it's only ever emptied by
    /// `FunctionBuilder::finalize` — which a `compile_entry` call that panics
    /// (or otherwise aborts) partway through never reaches. Without this,
    /// the *next* compilation attempt on this `Codegen` would immediately
    /// hit that assertion, permanently breaking it. The half-built machine
    /// code left behind in `module`/`func_ids` is harmless dead weight (see
    /// `checkpoint_func_ids`/`restore_func_ids`) — cranelift-jit only
    /// finalizes functions that were actually *defined*, never ones merely
    /// declared, so an abandoned declaration is silently inert.
    pub fn reset_builder_ctx(&mut self) {
        self.builder_ctx = FunctionBuilderContext::new();
    }

    pub fn new() -> Self {
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "false").expect("is_pic setting");
        let flags = settings::Flags::new(flag_builder);
        let isa = cranelift_native::builder()
            .expect("host machine not supported by Cranelift")
            .finish(flags)
            .expect("ISA builder failed");

        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());

        // Register runtime symbol addresses so the JIT can resolve call targets.
        builder.symbol("frog_alloc_str",   ffi::frog_alloc_str   as *const u8);
        builder.symbol("frog_str_len",     ffi::frog_str_len     as *const u8);
        builder.symbol("frog_str_concat",  ffi::frog_str_concat  as *const u8);
        builder.symbol("frog_str_eq",      ffi::frog_str_eq      as *const u8);
        builder.symbol("frog_str_cmp",     ffi::frog_str_cmp     as *const u8);
        builder.symbol("frog_str_print",   ffi::frog_str_print   as *const u8);
        builder.symbol("frog_str_println", ffi::frog_str_println as *const u8);
        builder.symbol("frog_alloc_list",  ffi::frog_alloc_list  as *const u8);
        builder.symbol("frog_list_len",    ffi::frog_list_len    as *const u8);
        builder.symbol("frog_list_get",    ffi::frog_list_get    as *const u8);
        builder.symbol("frog_list_set",    ffi::frog_list_set    as *const u8);
        builder.symbol("frog_list_push",   ffi::frog_list_push   as *const u8);
        builder.symbol("frog_list_slice",  ffi::frog_list_slice  as *const u8);
        builder.symbol("frog_range",       ffi::frog_range       as *const u8);
        builder.symbol("frog_gc_dump",     ffi::frog_gc_dump     as *const u8);
        builder.symbol("frog_frame_push",  ffi::frog_frame_push  as *const u8);
        builder.symbol("frog_frame_pop",   ffi::frog_frame_pop   as *const u8);

        let mut module   = JITModule::new(builder);
        let mut func_ids = HashMap::<String, FuncId>::new();

        use types::I64;
        // Declare Cranelift import signatures for each runtime function.
        declare_rt(&mut module, &mut func_ids, "frog_alloc_str",  "frog_alloc_str",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_len",    "frog_str_len",    &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_concat", "frog_str_concat", &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_eq",     "frog_str_eq",     &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_cmp",    "frog_str_cmp",    &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_print",  "frog_str_print",  &[I64],           None);
        // "print" in froglang calls frog_str_println (with newline).
        declare_rt(&mut module, &mut func_ids, "frog_str_println","print",           &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_alloc_list", "frog_alloc_list", &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_len",   "frog_list_len",   &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_get",   "frog_list_get",   &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_set",   "frog_list_set",   &[I64, I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_list_push",  "frog_list_push",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_slice", "frog_list_slice", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_range",      "frog_range",      &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_gc_dump",    "gc_dump",         &[],               None);
        declare_rt(&mut module, &mut func_ids, "frog_frame_push", "frog_frame_push", &[I64, I64],      None);
        declare_rt(&mut module, &mut func_ids, "frog_frame_pop",  "frog_frame_pop",  &[],              None);

        Codegen {
            module,
            func_ids,
            builder_ctx: FunctionBuilderContext::new(),
        }
    }

    fn make_sig(&self, params: &[(String, Type)], return_type: &Type) -> cranelift_codegen::ir::Signature {
        let mut sig = self.module.make_signature();
        for (_, ty) in params {
            sig.params.push(AbiParam::new(cl_type(ty)));
        }
        if *return_type != Type::None {
            sig.returns.push(AbiParam::new(cl_type(return_type)));
        }
        sig
    }

    fn build_func_body(
        builder_ctx: &mut FunctionBuilderContext,
        cl_ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        params: &[(String, Type)],
        return_type: &Type,
        body: &Spanned<TypedExpr>,
        string_arena: &mut Vec<Vec<u8>>,
    ) {
        let mut bcx = FunctionBuilder::new(&mut cl_ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);

        let mut vars: HashMap<String, Variable> = HashMap::new();
        let mut var_counter: u32 = 0;
        let entry_params: Vec<Value> = bcx.block_params(entry).to_vec();
        for ((name, ty), val) in params.iter().zip(entry_params) {
            declare_and_def_var(&mut bcx, &mut vars, &mut var_counter, name, ty, val);
        }

        let heap_slot = setup_shadow_frame(&mut bcx, module, func_ids, count_heap_slots(body));
        let mut ctx = Ctx { func_ids, module, string_arena, heap_slot, heap_cursor: 0, var_counter };
        let result = compile_expr(body, &mut bcx, &mut vars, &mut ctx);
        teardown_shadow_frame(&mut bcx, module, func_ids, heap_slot);

        if *return_type != Type::None {
            bcx.ins().return_(&[result]);
        } else {
            bcx.ins().return_(&[]);
        }

        bcx.seal_all_blocks();
        bcx.finalize();
    }

    /// Build the `__frog_main[_N]` body. The function takes one `i64` pointer
    /// parameter (`out_ptr`, unused if there are no top-level bindings) and
    /// writes each top-level `let`/`func`-free `Assign`'s value into
    /// consecutive 8-byte slots there, in source order — this is how the
    /// caller (`FrogState::eval`) learns the values of *every* binding made
    /// in this entry, not just the last one. Returns that ordered
    /// `(name, type)` list so the caller can decode `out_ptr`'s contents.
    fn build_main_body(
        builder_ctx: &mut FunctionBuilderContext,
        cl_ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        stmts: &[Spanned<TypedExpr>],
        string_arena: &mut Vec<Vec<u8>>,
        pre_env: &HashMap<String, i64>,
        env_types: &HashMap<String, Type>,
    ) -> Vec<(String, Type)> {
        let mut bcx = FunctionBuilder::new(&mut cl_ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let out_ptr = bcx.block_params(entry)[0];

        let mut vars: HashMap<String, Variable> = HashMap::new();
        let mut var_counter: u32 = 0;

        // Pre-seed vars from prior REPL entries as iconst values.
        for (name, &bits) in pre_env {
            let ty = env_types.get(name).unwrap_or(&Type::Int);
            let val = match ty {
                Type::Float => bcx.ins().f64const(f64::from_bits(bits as u64)),
                Type::Bool  => bcx.ins().iconst(types::I8, bits),
                _           => bcx.ins().iconst(types::I64, bits),
            };
            declare_and_def_var(&mut bcx, &mut vars, &mut var_counter, name, ty, val);
        }
        let mut last_val = bcx.ins().iconst(types::I64, 0);
        let mut last_ty = &Type::Int;

        let n: usize = stmts.iter().map(count_heap_slots).sum();
        let heap_slot = setup_shadow_frame(&mut bcx, module, func_ids, n);
        let mut ctx = Ctx { func_ids, module, string_arena, heap_slot, heap_cursor: 0, var_counter };

        let mut bindings: Vec<(String, Type)> = Vec::new();

        for stmt in stmts {
            if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                    continue;
                }
                last_val = compile_expr(stmt, &mut bcx, &mut vars, &mut ctx);
                last_ty = &stmt.item.ty;
                let repr = to_i64_repr(&mut bcx, last_ty, last_val);
                let offset = (bindings.len() * 8) as i32;
                bcx.ins().store(MemFlags::new(), repr, out_ptr, offset);
                bindings.push((name.clone(), last_ty.clone()));
                continue;
            }
            last_val = compile_expr(stmt, &mut bcx, &mut vars, &mut ctx);
            last_ty = &stmt.item.ty;
        }

        teardown_shadow_frame(&mut bcx, module, func_ids, heap_slot);

        // __frog_main[_N] always returns i64.
        let last_val = to_i64_repr(&mut bcx, last_ty, last_val);

        bcx.ins().return_(&[last_val]);
        bcx.seal_all_blocks();
        bcx.finalize();

        bindings
    }

    /// Two-pass compilation of a top-level typed block.
    /// Returns the `FuncId` of `__frog_main` and the ordered list of
    /// top-level bindings it writes to its `out_ptr` parameter.
    /// Compile a single top-level program or REPL entry into a uniquely-named
    /// `__frog_main_N` function, pre-seeding the variable environment from
    /// prior entries (empty for a one-shot compile, e.g. `compile_and_run`).
    /// Returns its `FuncId` and the ordered list of top-level bindings it
    /// writes to its `out_ptr` parameter.
    pub fn compile_entry(
        &mut self,
        typed: Spanned<TypedExpr>,
        string_arena: &mut Vec<Vec<u8>>,
        entry_id: usize,
        pre_env: &HashMap<String, i64>,
        env_types: &HashMap<String, Type>,
    ) -> (FuncId, Vec<(String, Type)>) {
        let stmts: Vec<Spanned<TypedExpr>> = match typed.item.kind {
            TypedExprKind::Block(s) => s,
            _ => vec![typed],
        };

        // ── Pass 1: Declare all top-level functions ───────────────────────────
        // Each entry's functions get a symbol unique to this entry (mangled
        // with `entry_id`), so redefining `func f` in a later REPL entry
        // never collides with `f`'s previous JIT symbol. `func_ids` stays
        // keyed by the plain source name and is simply overwritten, so any
        // subsequent call (in this or a later entry) resolves to the newest
        // definition — the old machine code stays resident but unreachable.
        for stmt in &stmts {
            if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                if let TypedExprKind::Function { params, return_type, .. } = &value.item.kind {
                    let sig = self.make_sig(params, return_type);
                    let mangled = format!("{}__frogfn{}", name, entry_id);
                    let func_id = self.module
                        .declare_function(&mangled, Linkage::Local, &sig)
                        .unwrap_or_else(|e| panic!("declare_function '{}' failed: {}", mangled, e));
                    self.func_ids.insert(name.clone(), func_id);
                }
            }
        }

        // ── Pass 2: Define all function bodies ───────────────────────────────
        let func_defs: Vec<(String, FuncId, Vec<(String, Type)>, Type, Box<Spanned<TypedExpr>>)> =
            stmts.iter().filter_map(|stmt| {
                if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                    if let TypedExprKind::Function { params, return_type, body } = &value.item.kind {
                        return Some((
                            name.clone(),
                            self.func_ids[name],
                            params.clone(),
                            return_type.clone(),
                            body.clone(),
                        ));
                    }
                }
                None
            }).collect();

        for (_, func_id, params, return_type, body) in &func_defs {
            let sig = self.make_sig(params, &return_type);
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig;

            // `module` and `func_ids` are disjoint fields, so borrowing them
            // separately here (rather than cloning `func_ids` — O(n) per
            // function, O(n^2) per entry) is fine: `func_ids` is read-only
            // for the whole of Pass 2, only ever written during Pass 1 above.
            Self::build_func_body(
                &mut self.builder_ctx,
                &mut ctx,
                &mut self.module,
                &self.func_ids,
                params,
                return_type,
                body,
                string_arena,
            );

            self.module
                .define_function(*func_id, &mut ctx)
                .unwrap_or_else(|e| panic!("define_function failed: {}", e));
            self.module.clear_context(&mut ctx);
        }

        // ── Pass 3: Build __frog_main_N ───────────────────────────────────────
        let entry_name = format!("__frog_main_{}", entry_id);
        let mut main_sig = self.module.make_signature();
        main_sig.params.push(AbiParam::new(types::I64));  // out_ptr
        main_sig.returns.push(AbiParam::new(types::I64));
        let main_id = self.module
            .declare_function(&entry_name, Linkage::Local, &main_sig)
            .unwrap_or_else(|e| panic!("declare {} failed: {}", entry_name, e));

        let mut ctx = self.module.make_context();
        ctx.func.signature = main_sig;

        let bindings = Self::build_main_body(
            &mut self.builder_ctx,
            &mut ctx,
            &mut self.module,
            &self.func_ids,
            &stmts,
            string_arena,
            pre_env,
            env_types,
        );

        self.module
            .define_function(main_id, &mut ctx)
            .unwrap_or_else(|e| panic!("define {} failed: {}", entry_name, e));
        self.module.clear_context(&mut ctx);

        self.module.finalize_definitions().expect("finalize_definitions failed");

        (main_id, bindings)
    }
}

/// Parse, type-check, compile, and run a froglang source string.
/// Returns the i64 result of the final expression.
pub fn compile_and_run(src: &str) -> i64 {
    use crate::frontend::parser::Parser;
    use crate::frontend::typeck::TypeChecker;

    let ast = Parser::parse(src).expect("parse error");
    let mut tc = TypeChecker::new();
    let typed = tc.check_and_lower(ast).expect("type error");

    let mut codegen = Codegen::new();
    let mut string_arena: Vec<Vec<u8>> = Vec::new();
    let (main_id, bindings) = codegen.compile_entry(
        typed, &mut string_arena, 0, &HashMap::new(), &HashMap::new(),
    );

    let ptr = codegen.module.get_finalized_function(main_id);
    let f: fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    let mut out_buf: Vec<i64> = vec![0i64; bindings.len()];
    f(out_buf.as_mut_ptr() as i64)
    // string_arena and out_buf dropped here, after f() returns
}
