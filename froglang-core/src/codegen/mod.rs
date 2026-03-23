use std::collections::HashMap;

use cranelift_codegen::ir::{condcodes::{FloatCC, IntCC}, types, AbiParam, InstBuilder, MemFlags, Value};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
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
/// `string_arena` keeps source `Vec<u8>` buffers alive until the JIT executes;
/// `frog_alloc_str` copies bytes immediately, so the arena only needs to outlive
/// the call to the compiled function.
fn compile_expr(
    expr: &Spanned<TypedExpr>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Value>,
    func_ids: &HashMap<String, FuncId>,
    module: &mut JITModule,
    string_arena: &mut Vec<Vec<u8>>,
) -> Value {
    match &expr.item.kind {
        TypedExprKind::IntLit(n) => bcx.ins().iconst(types::I64, *n),

        TypedExprKind::BoolLit(b) => bcx.ins().iconst(types::I8, *b as i64),

        TypedExprKind::FloatLit(f) => bcx.ins().f64const(*f),

        TypedExprKind::StrLit(s) => {
            let bytes = s.as_bytes().to_vec();
            let ptr = bytes.as_ptr() as i64;
            let len = bytes.len() as i64;
            string_arena.push(bytes);  // keep alive until after JIT call

            let data_val = bcx.ins().iconst(types::I64, ptr);
            let len_val  = bcx.ins().iconst(types::I64, len);

            let func_id = func_ids["frog_alloc_str"];
            let callee  = module.declare_func_in_func(func_id, bcx.func);
            let call    = bcx.ins().call(callee, &[data_val, len_val]);
            bcx.inst_results(call)[0]
        },

        TypedExprKind::Var(name) => {
            *vars.get(name.as_str())
                .unwrap_or_else(|| panic!("unbound variable in codegen: {}", name))
        },

        TypedExprKind::Unary { op, expr: inner } => {
            let v = compile_expr(inner, bcx, vars, func_ids, module, string_arena);
            match op {
                Token::Minus => {
                    if inner.item.ty == Type::Float {
                        bcx.ins().fneg(v)
                    } else {
                        let zero = bcx.ins().iconst(types::I64, 0);
                        bcx.ins().isub(zero, v)
                    }
                },
                _ => unimplemented!("unary op {:?}", op),
            }
        },

        TypedExprKind::Binary { op, left, right } => {
            // ── String operations (must short-circuit before numeric path) ──
            if left.item.ty == Type::Str {
                let lv = compile_expr(left,  bcx, vars, func_ids, module, string_arena);
                let rv = compile_expr(right, bcx, vars, func_ids, module, string_arena);
                return match op {
                    Token::Plus => {
                        let id     = func_ids["frog_str_concat"];
                        let callee = module.declare_func_in_func(id, bcx.func);
                        let call   = bcx.ins().call(callee, &[lv, rv]);
                        bcx.inst_results(call)[0]
                    },
                    Token::EqEq | Token::NotEq => {
                        let id     = func_ids["frog_str_eq"];
                        let callee = module.declare_func_in_func(id, bcx.func);
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
                    _ => unimplemented!("string binary op {:?}", op),
                };
            }

            // ── Numeric operations ──────────────────────────────────────────
            let lv = compile_expr(left,  bcx, vars, func_ids, module, string_arena);
            let rv = compile_expr(right, bcx, vars, func_ids, module, string_arena);
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
                Token::And   => bcx.ins().band(lv, rv),
                Token::Or    => bcx.ins().bor(lv, rv),
                _ => unimplemented!("binary op {:?}", op),
            }
        },

        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            let cond_val = compile_expr(cond, bcx, vars, func_ids, module, string_arena);

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
            let tv = compile_expr(true_branch, bcx, vars, func_ids, module, string_arena);
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
                    let v = compile_expr(fb, bcx, vars, func_ids, module, string_arena);
                    ensure_width(v, &fb.item.ty, result_ty, bcx)
                } else {
                    bcx.ins().iconst(result_ty, 0)
                };
                bcx.ins().jump(merge_bb, &[fv]);
            } else {
                if let Some(fb) = false_branch {
                    compile_expr(fb, bcx, vars, func_ids, module, string_arena);
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
            let func_id = func_ids[&func_name];
            let local_callee = module.declare_func_in_func(func_id, bcx.func);

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
                let mut v = compile_expr(a, bcx, vars, func_ids, module, string_arena);
                if let Some(param_ty) = param_types.get(i) {
                    v = coerce_value(v, &a.item.ty, param_ty, bcx);
                }
                arg_vals.push(v);
            }

            let call = bcx.ins().call(local_callee, &arg_vals);
            if return_ty == Type::None {
                bcx.ins().iconst(types::I64, 0)
            } else {
                bcx.inst_results(call)[0]
            }
        },

        TypedExprKind::Assign { name, value } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                bcx.ins().iconst(types::I64, 0)
            } else {
                let val = compile_expr(value, bcx, vars, func_ids, module, string_arena);
                vars.insert(name.clone(), val);
                val
            }
        },

        TypedExprKind::Block(stmts) => {
            let mut last = bcx.ins().iconst(types::I64, 0);
            for stmt in stmts {
                last = compile_expr(stmt, bcx, vars, func_ids, module, string_arena);
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

            let alloc_id = func_ids["frog_alloc_list"];
            let alloc_ref = module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[cap_val, tag_val]);
            let list_ptr = bcx.inst_results(alloc_call)[0];

            let push_id = func_ids["frog_list_push"];
            for elem in elems {
                let ev = compile_expr(elem, bcx, vars, func_ids, module, string_arena);
                let push_ref = module.declare_func_in_func(push_id, bcx.func);
                let push_call = bcx.ins().call(push_ref, &[list_ptr, ev]);
                let _ = bcx.inst_results(push_call)[0];
            }

            list_ptr
        },
    }
}

impl Codegen {
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
        builder.symbol("frog_str_print",   ffi::frog_str_print   as *const u8);
        builder.symbol("frog_str_println", ffi::frog_str_println as *const u8);
        builder.symbol("frog_alloc_list",  ffi::frog_alloc_list  as *const u8);
        builder.symbol("frog_list_len",    ffi::frog_list_len    as *const u8);
        builder.symbol("frog_list_get",    ffi::frog_list_get    as *const u8);
        builder.symbol("frog_list_set",    ffi::frog_list_set    as *const u8);
        builder.symbol("frog_list_push",   ffi::frog_list_push   as *const u8);
        builder.symbol("frog_gc_dump",     ffi::frog_gc_dump     as *const u8);

        let mut module   = JITModule::new(builder);
        let mut func_ids = HashMap::<String, FuncId>::new();

        use types::I64;
        // Declare Cranelift import signatures for each runtime function.
        declare_rt(&mut module, &mut func_ids, "frog_alloc_str",  "frog_alloc_str",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_len",    "frog_str_len",    &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_concat", "frog_str_concat", &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_eq",     "frog_str_eq",     &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_print",  "frog_str_print",  &[I64],           None);
        // "print" in froglang calls frog_str_println (with newline).
        declare_rt(&mut module, &mut func_ids, "frog_str_println","print",           &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_alloc_list", "frog_alloc_list", &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_len",   "frog_list_len",   &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_get",   "frog_list_get",   &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_set",   "frog_list_set",   &[I64, I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_list_push",  "frog_list_push",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_gc_dump",    "gc_dump",         &[],               None);

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
        ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        params: &[(String, Type)],
        return_type: &Type,
        body: &Spanned<TypedExpr>,
        string_arena: &mut Vec<Vec<u8>>,
    ) {
        let mut bcx = FunctionBuilder::new(&mut ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);

        let mut vars: HashMap<String, Value> = HashMap::new();
        let entry_params: Vec<Value> = bcx.block_params(entry).to_vec();
        for ((name, _), val) in params.iter().zip(entry_params) {
            vars.insert(name.clone(), val);
        }

        let result = compile_expr(body, &mut bcx, &mut vars, func_ids, module, string_arena);

        if *return_type != Type::None {
            bcx.ins().return_(&[result]);
        } else {
            bcx.ins().return_(&[]);
        }

        bcx.seal_all_blocks();
        bcx.finalize();
    }

    fn build_main_body(
        builder_ctx: &mut FunctionBuilderContext,
        ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        stmts: &[Spanned<TypedExpr>],
        string_arena: &mut Vec<Vec<u8>>,
        pre_env: &HashMap<String, i64>,
        env_types: &HashMap<String, Type>,
    ) {
        let mut bcx = FunctionBuilder::new(&mut ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);

        let mut vars: HashMap<String, Value> = HashMap::new();

        // Pre-seed vars from prior REPL entries as iconst values.
        for (name, &bits) in pre_env {
            let ty = env_types.get(name).unwrap_or(&Type::Int);
            let val = match ty {
                Type::Float => bcx.ins().f64const(f64::from_bits(bits as u64)),
                Type::Bool  => bcx.ins().iconst(types::I8, bits),
                _           => bcx.ins().iconst(types::I64, bits),
            };
            vars.insert(name.clone(), val);
        }
        let mut last_val = bcx.ins().iconst(types::I64, 0);
        let mut last_ty = &Type::Int;

        for stmt in stmts {
            if let TypedExprKind::Assign { value, .. } = &stmt.item.kind {
                if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                    continue;
                }
            }
            last_val = compile_expr(stmt, &mut bcx, &mut vars, func_ids, module, string_arena);
            last_ty = &stmt.item.ty;
        }

        // __frog_main always returns i64.
        if *last_ty == Type::Float {
            last_val = bcx.ins().bitcast(types::I64, MemFlags::new(), last_val);
        } else if *last_ty == Type::Bool {
            last_val = bcx.ins().uextend(types::I64, last_val);
        }
        // Str and List(_) are already I64 pointers — no conversion needed.

        bcx.ins().return_(&[last_val]);
        bcx.seal_all_blocks();
        bcx.finalize();
    }

    /// Two-pass compilation of a top-level typed block.
    /// Returns the `FuncId` of `__frog_main`.
    pub fn compile(&mut self, typed: Spanned<TypedExpr>, string_arena: &mut Vec<Vec<u8>>) -> FuncId {
        let stmts: Vec<Spanned<TypedExpr>> = match typed.item.kind {
            TypedExprKind::Block(s) => s,
            _ => vec![typed],
        };

        // ── Pass 1: Declare all top-level functions ───────────────────────────
        for stmt in &stmts {
            if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                if let TypedExprKind::Function { params, return_type, .. } = &value.item.kind {
                    let sig = self.make_sig(params, return_type);
                    let func_id = self.module
                        .declare_function(name, Linkage::Local, &sig)
                        .unwrap_or_else(|e| panic!("declare_function '{}' failed: {}", name, e));
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
            let func_ids_snap = self.func_ids.clone();
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig;

            Self::build_func_body(
                &mut self.builder_ctx,
                &mut ctx,
                &mut self.module,
                &func_ids_snap,
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

        // ── Pass 3: Build __frog_main ─────────────────────────────────────────
        let mut main_sig = self.module.make_signature();
        main_sig.returns.push(AbiParam::new(types::I64));
        let main_id = self.module
            .declare_function("__frog_main", Linkage::Local, &main_sig)
            .expect("declare __frog_main failed");

        let func_ids_snap = self.func_ids.clone();
        let mut ctx = self.module.make_context();
        ctx.func.signature = main_sig;

        Self::build_main_body(
            &mut self.builder_ctx,
            &mut ctx,
            &mut self.module,
            &func_ids_snap,
            &stmts,
            string_arena,
            &HashMap::new(),
            &HashMap::new(),
        );

        self.module
            .define_function(main_id, &mut ctx)
            .expect("define __frog_main failed");
        self.module.clear_context(&mut ctx);

        self.module.finalize_definitions().expect("finalize_definitions failed");

        main_id
    }

    /// Compile a single REPL entry into a uniquely-named `__frog_main_N` function,
    /// pre-seeding the variable environment from prior entries.
    pub fn compile_entry(
        &mut self,
        typed: Spanned<TypedExpr>,
        string_arena: &mut Vec<Vec<u8>>,
        entry_id: usize,
        pre_env: &HashMap<String, i64>,
        env_types: &HashMap<String, Type>,
    ) -> FuncId {
        let stmts: Vec<Spanned<TypedExpr>> = match typed.item.kind {
            TypedExprKind::Block(s) => s,
            _ => vec![typed],
        };

        // ── Pass 1: Declare all top-level functions ───────────────────────────
        for stmt in &stmts {
            if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                if let TypedExprKind::Function { params, return_type, .. } = &value.item.kind {
                    let sig = self.make_sig(params, return_type);
                    let func_id = self.module
                        .declare_function(name, Linkage::Local, &sig)
                        .unwrap_or_else(|e| panic!("declare_function '{}' failed: {}", name, e));
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
            let func_ids_snap = self.func_ids.clone();
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig;

            Self::build_func_body(
                &mut self.builder_ctx,
                &mut ctx,
                &mut self.module,
                &func_ids_snap,
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
        main_sig.returns.push(AbiParam::new(types::I64));
        let main_id = self.module
            .declare_function(&entry_name, Linkage::Local, &main_sig)
            .unwrap_or_else(|e| panic!("declare {} failed: {}", entry_name, e));

        let func_ids_snap = self.func_ids.clone();
        let mut ctx = self.module.make_context();
        ctx.func.signature = main_sig;

        Self::build_main_body(
            &mut self.builder_ctx,
            &mut ctx,
            &mut self.module,
            &func_ids_snap,
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

        main_id
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
    let main_id = codegen.compile(typed, &mut string_arena);

    let ptr = codegen.module.get_finalized_function(main_id);
    let f: fn() -> i64 = unsafe { std::mem::transmute(ptr) };
    f()
    // string_arena dropped here, after f() returns
}
