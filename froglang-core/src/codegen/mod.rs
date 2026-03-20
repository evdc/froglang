use std::collections::HashMap;

use cranelift_codegen::ir::{condcodes::{FloatCC, IntCC}, types, AbiParam, InstBuilder, MemFlags, Value};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};
use crate::frontend::typeck::{Type, numeric_join};

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
/// No-op when types are equal. Panics if no widening path exists
/// (type checker should have caught this before codegen).
fn coerce_value(val: Value, from_ty: &Type, to_ty: &Type, bcx: &mut FunctionBuilder) -> Value {
    use crate::frontend::typeck::widens_to;
    if from_ty == to_ty { return val; }
    assert!(widens_to(from_ty, to_ty), "no widening from {:?} to {:?}", from_ty, to_ty);
    match (from_ty, to_ty) {
        (Type::Int, Type::Float) => bcx.ins().fcvt_from_sint(types::F64, val),
        // Future: (Type::Int32, Type::Int64) => bcx.ins().sextend(types::I64, val),
        _ => unreachable!(),
    }
}

/// Compile a typed expression into Cranelift IR, returning its SSA value.
fn compile_expr(
    expr: &Spanned<TypedExpr>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Value>,
    func_ids: &HashMap<String, FuncId>,
    module: &mut JITModule,
) -> Value {
    match &expr.item.kind {
        TypedExprKind::IntLit(n) => bcx.ins().iconst(types::I64, *n),

        TypedExprKind::BoolLit(b) => bcx.ins().iconst(types::I8, *b as i64),

        TypedExprKind::FloatLit(f) => bcx.ins().f64const(*f),

        TypedExprKind::StrLit(_) => unimplemented!("string codegen not yet supported"),

        TypedExprKind::Var(name) => {
            *vars.get(name.as_str())
                .unwrap_or_else(|| panic!("unbound variable in codegen: {}", name))
        },

        TypedExprKind::Unary { op, expr: inner } => {
            let v = compile_expr(inner, bcx, vars, func_ids, module);
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
            let lv = compile_expr(left,  bcx, vars, func_ids, module);
            let rv = compile_expr(right, bcx, vars, func_ids, module);
            let op_ty = numeric_join(&left.item.ty, &right.item.ty).unwrap_or_else(|| left.item.ty.clone());
            let lv = coerce_value(lv, &left.item.ty, &op_ty, bcx);
            let rv = coerce_value(rv, &right.item.ty, &op_ty, bcx);
            let is_float = op_ty == Type::Float;
            match op {
                Token::Plus  => if is_float { bcx.ins().fadd(lv, rv) } else { bcx.ins().iadd(lv, rv) },
                Token::Minus => if is_float { bcx.ins().fsub(lv, rv) } else { bcx.ins().isub(lv, rv) },
                Token::Star  => if is_float { bcx.ins().fmul(lv, rv) } else { bcx.ins().imul(lv, rv) },
                Token::Slash => if is_float { bcx.ins().fdiv(lv, rv) } else { bcx.ins().sdiv(lv, rv) },
                Token::EqEq  => if is_float { bcx.ins().fcmp(FloatCC::Equal,            lv, rv) } else { bcx.ins().icmp(IntCC::Equal,                    lv, rv) },
                Token::NotEq => if is_float { bcx.ins().fcmp(FloatCC::NotEqual,         lv, rv) } else { bcx.ins().icmp(IntCC::NotEqual,                 lv, rv) },
                Token::Lt    => if is_float { bcx.ins().fcmp(FloatCC::LessThan,         lv, rv) } else { bcx.ins().icmp(IntCC::SignedLessThan,            lv, rv) },
                Token::Gt    => if is_float { bcx.ins().fcmp(FloatCC::GreaterThan,      lv, rv) } else { bcx.ins().icmp(IntCC::SignedGreaterThan,         lv, rv) },
                Token::LtEq  => if is_float { bcx.ins().fcmp(FloatCC::LessThanOrEqual,    lv, rv) } else { bcx.ins().icmp(IntCC::SignedLessThanOrEqual,   lv, rv) },
                Token::GtEq  => if is_float { bcx.ins().fcmp(FloatCC::GreaterThanOrEqual, lv, rv) } else { bcx.ins().icmp(IntCC::SignedGreaterThanOrEqual, lv, rv) },
                Token::And   => bcx.ins().band(lv, rv),
                Token::Or    => bcx.ins().bor(lv, rv),
                _ => unimplemented!("binary op {:?}", op),
            }
        },

        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            let cond_val = compile_expr(cond, bcx, vars, func_ids, module);

            let true_bb  = bcx.create_block();
            let false_bb = bcx.create_block();
            let merge_bb = bcx.create_block();

            // Only add a block parameter when the expression produces a value.
            let has_value = expr.item.ty != Type::None;
            let result_ty = cl_type(&expr.item.ty);
            if has_value {
                bcx.append_block_param(merge_bb, result_ty);
            }

            bcx.ins().brif(cond_val, true_bb, &[], false_bb, &[]);

            // --- true branch ---
            bcx.switch_to_block(true_bb);
            bcx.seal_block(true_bb);
            let tv = compile_expr(true_branch, bcx, vars, func_ids, module);
            if has_value {
                let tv = ensure_width(tv, &true_branch.item.ty, result_ty, bcx);
                bcx.ins().jump(merge_bb, &[tv]);
            } else {
                bcx.ins().jump(merge_bb, &[]);
            }

            // --- false branch ---
            bcx.switch_to_block(false_bb);
            bcx.seal_block(false_bb);
            if has_value {
                let fv = if let Some(fb) = false_branch {
                    let v = compile_expr(fb, bcx, vars, func_ids, module);
                    ensure_width(v, &fb.item.ty, result_ty, bcx)
                } else {
                    bcx.ins().iconst(result_ty, 0)
                };
                bcx.ins().jump(merge_bb, &[fv]);
            } else {
                if let Some(fb) = false_branch {
                    compile_expr(fb, bcx, vars, func_ids, module);
                }
                bcx.ins().jump(merge_bb, &[]);
            }

            // --- merge ---
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
            let mut arg_vals: Vec<Value> = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                let mut v = compile_expr(a, bcx, vars, func_ids, module);
                if let Some(param_ty) = param_types.get(i) {
                    v = coerce_value(v, &a.item.ty, param_ty, bcx);
                }
                arg_vals.push(v);
            }

            let call = bcx.ins().call(local_callee, &arg_vals);
            bcx.inst_results(call)[0]
        },

        TypedExprKind::Assign { name, value } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                // Top-level function definitions are compiled in the two-pass; skip here.
                bcx.ins().iconst(types::I64, 0)
            } else {
                let val = compile_expr(value, bcx, vars, func_ids, module);
                vars.insert(name.clone(), val);
                val
            }
        },

        TypedExprKind::Block(stmts) => {
            let mut last = bcx.ins().iconst(types::I64, 0);
            for stmt in stmts {
                last = compile_expr(stmt, bcx, vars, func_ids, module);
            }
            last
        },

        TypedExprKind::Function { .. } => {
            // Bare function expressions in non-assign context — unsupported in MVP.
            bcx.ins().iconst(types::I64, 0)
        },

        TypedExprKind::List(_) => unimplemented!("list codegen not yet supported"),
    }
}

impl Codegen {
    pub fn new() -> Self {
        // Disable PIC so that inter-function calls use direct (non-PLT) relocations.
        // PLT is only supported on x86_64 in Cranelift JIT; on aarch64 (Apple Silicon)
        // we must use absolute addresses which the JIT can resolve directly.
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "false").expect("is_pic setting");
        let flags = settings::Flags::new(flag_builder);
        let isa = cranelift_native::builder()
            .expect("host machine not supported by Cranelift")
            .finish(flags)
            .expect("ISA builder failed");
        let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        let module = JITModule::new(builder);
        Codegen {
            module,
            func_ids: HashMap::new(),
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

    /// Compile a function body into `ctx`, then define it in the module.
    /// Takes `builder_ctx` and `module` as separate parameters to enable
    /// disjoint field borrowing at call sites.
    fn build_func_body(
        builder_ctx: &mut FunctionBuilderContext,
        ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        params: &[(String, Type)],
        return_type: &Type,
        body: &Spanned<TypedExpr>,
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

        let result = compile_expr(body, &mut bcx, &mut vars, func_ids, module);

        if *return_type != Type::None {
            bcx.ins().return_(&[result]);
        } else {
            bcx.ins().return_(&[]);
        }

        bcx.seal_all_blocks();
        bcx.finalize();
    }

    /// Build the `__frog_main` function that runs all non-function top-level
    /// statements and returns the value of the last expression.
    fn build_main_body(
        builder_ctx: &mut FunctionBuilderContext,
        ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        stmts: &[Spanned<TypedExpr>],
    ) {
        let mut bcx = FunctionBuilder::new(&mut ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);

        let mut vars: HashMap<String, Value> = HashMap::new();
        let mut last_val = bcx.ins().iconst(types::I64, 0);
        let mut last_ty = &Type::Int;

        for stmt in stmts {
            // Skip top-level function-definition assigns; they're compiled in pass 2.
            if let TypedExprKind::Assign { value, .. } = &stmt.item.kind {
                if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                    continue;
                }
            }
            last_val = compile_expr(stmt, &mut bcx, &mut vars, func_ids, module);
            last_ty = &stmt.item.ty;
        }

        // __frog_main always returns i64; coerce other numeric types to match.
        if *last_ty == Type::Float {
            last_val = bcx.ins().bitcast(types::I64, MemFlags::new(), last_val);
        } else if *last_ty == Type::Bool {
            last_val = bcx.ins().uextend(types::I64, last_val);
        }

        bcx.ins().return_(&[last_val]);
        bcx.seal_all_blocks();
        bcx.finalize();
    }

    /// Two-pass compilation of a top-level typed block.
    /// Returns the `FuncId` of `__frog_main`.
    pub fn compile(&mut self, typed: Spanned<TypedExpr>) -> FuncId {
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
        // Collect function data first to avoid holding borrows into stmts while
        // we mutate self.
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
        );

        self.module
            .define_function(main_id, &mut ctx)
            .expect("define __frog_main failed");
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
    let main_id = codegen.compile(typed);

    let ptr = codegen.module.get_finalized_function(main_id);
    let f: fn() -> i64 = unsafe { std::mem::transmute(ptr) };
    f()
}
