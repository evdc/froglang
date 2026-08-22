//! Why froglang pins a Cranelift new enough to track stack-map bindings
//! through SSA-inserted block parameters — RUNTIME.md Part 2's prerequisite.
//!
//! Part 2 delegates GC root discovery to Cranelift's user stack maps. The
//! contract froglang needs is: mark a `Variable` with
//! `declare_var_needs_stack_map`, and *every* SSA value that variable's
//! value ever flows through is recorded at every safepoint it is live
//! across. Anything less is a missed root, i.e. exactly the use-after-free
//! class the shadow stack already has — delegating would move the bug, not
//! remove it.
//!
//! In cranelift-frontend 0.113 that contract did not hold. `stack_map_vars`
//! was consulted only in `try_use_var` and `try_def_var`, so a block
//! parameter the SSA builder inserts purely to *route* a variable's value
//! through a block that never uses it was never declared, never spilled,
//! and absent from that block's stack maps. 0.135 moved the tracking into
//! the SSA builder itself, which records the binding at the point it
//! appends the parameter.
//!
//! This test builds the shape that distinguishes the two — a conduit block
//! with a safepoint, between a two-way definition and a two-way use — and
//! asserts the value is present. It fails against 0.113. It is not testing
//! froglang code; it is pinning the property froglang's rooting depends on,
//! so a future Cranelift downgrade or regression is a red test rather than
//! an intermittent collector crash.

use cranelift_codegen::ir::{
    types, AbiParam, ExtFuncData, ExternalName, Function, InstBuilder, Signature, UserFuncName,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

#[test]
fn a_conduit_block_param_is_recorded_in_its_blocks_stack_map() {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(types::I64)); // one definition of `x`
    sig.params.push(AbiParam::new(types::I64)); // the other
    sig.params.push(AbiParam::new(types::I8));  // an opaque condition
    sig.returns.push(AbiParam::new(types::I64));

    let mut func = Function::with_name_signature(UserFuncName::testcase("conduit"), sig);
    let callee_sig = func.import_signature(Signature::new(CallConv::SystemV));
    let gc = func.import_function(ExtFuncData {
        name: ExternalName::testcase("gc"),
        signature: callee_sig,
        colocated: false,
        patchable: false,
    });

    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fctx);

    let entry = b.create_block();
    let def_a = b.create_block();
    let def_b = b.create_block();
    let conduit = b.create_block();
    let arm_a = b.create_block();
    let arm_b = b.create_block();
    let uses = b.create_block();

    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let (a, bb, c) = {
        let p = b.block_params(entry);
        (p[0], p[1], p[2])
    };

    let x = b.declare_var(types::I64);
    b.declare_var_needs_stack_map(x);
    b.ins().brif(c, def_a, &[], def_b, &[]);

    // Two different definitions, so `conduit` needs a parameter for `x`.
    b.switch_to_block(def_a); b.seal_block(def_a);
    b.def_var(x, a);
    b.ins().jump(conduit, &[]);

    b.switch_to_block(def_b); b.seal_block(def_b);
    b.def_var(x, bb);
    b.ins().jump(conduit, &[]);

    // The conduit: it holds a safepoint but never mentions `x` itself, so
    // no `use_var`/`def_var` here ever sees its parameter.
    b.switch_to_block(conduit); b.seal_block(conduit);
    b.ins().call(gc, &[]);
    b.ins().brif(c, arm_a, &[], arm_b, &[]);

    // One arm redefines `x`, so the use block below is a real join and gets
    // a parameter of its own — leaving the conduit's parameter as the only
    // carrier on the other path, and one no `use_var` ever returns.
    b.switch_to_block(arm_a); b.seal_block(arm_a);
    let shifted = b.ins().iadd_imm_s(a, 8);
    b.def_var(x, shifted);
    b.ins().jump(uses, &[]);

    b.switch_to_block(arm_b); b.seal_block(arm_b);
    b.ins().jump(uses, &[]);

    b.switch_to_block(uses); b.seal_block(uses);
    let v = b.use_var(x);
    b.ins().call(gc, &[]);
    b.ins().return_(&[v]);

    let isa = cranelift_native::builder()
        .expect("native ISA builder")
        .finish(settings::Flags::new(settings::builder()))
        .expect("native ISA");
    b.finalize(isa.frontend_config());

    let mut cctx = cranelift_codegen::Context::for_function(func);
    let clif = cctx.func.clone();
    let code = cctx.compile(&*isa, &mut Default::default()).expect("compile");

    // Find the conduit block's parameter and assert it was spilled — the
    // observable consequence of it being tracked. Without tracking, the
    // block carries no `stack_store` of its parameter at all.
    let maps = code.buffer.user_stack_maps();
    assert_eq!(maps.len(), 2, "expected one stack map per call");
    for (_, _, m) in maps {
        assert!(
            m.entries().next().is_some(),
            "a safepoint recorded no roots at all, so the variable's value is \
             unreachable to a collector there:\n{}",
            clif.display(),
        );
    }

    // The stronger statement: the conduit block really does have a
    // parameter (i.e. this test is exercising the shape it claims to), and
    // that parameter really is spilled.
    let conduit_params = clif.dfg.block_params(conduit);
    assert_eq!(
        conduit_params.len(), 1,
        "the conduit block should carry exactly one SSA-inserted parameter — \
         if it has none, cranelift's SSA construction changed and this test no \
         longer exercises the property it is pinning:\n{}",
        clif.display(),
    );
    let param = conduit_params[0];
    let spilled = clif.layout.block_insts(conduit).any(|inst| {
        clif.dfg.insts[inst].opcode() == cranelift_codegen::ir::Opcode::Store
            && clif.dfg.inst_values(inst).any(|v| v == param)
    });
    assert!(
        spilled,
        "the conduit block's parameter {param} was never spilled, so it is \
         missing from the stack map at that block's safepoint — this is the \
         cranelift-frontend 0.113 behaviour, and rooting is unsound on it:\n{}",
        clif.display(),
    );
}
