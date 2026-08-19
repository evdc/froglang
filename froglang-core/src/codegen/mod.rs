use std::collections::HashMap;
use std::mem::{offset_of, size_of};

use cranelift_codegen::ir::{condcodes::{FloatCC, IntCC}, types, AbiParam, InstBuilder, MemFlags, StackSlot, StackSlotData, StackSlotKind, TrapCode, Value};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::frontend::tokens::{Spanned, Token};
use crate::frontend::typed_ast::{TypedExpr, TypedExprKind};
use crate::frontend::typeck::{UnionDef, UnionDefs, StructDefs, Type, numeric_join};
use crate::runtime::{ffi, gc};
use crate::runtime::gc::{FrogList, FrogVariant};

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
    /// Slots `setup_shadow_frame` actually allocated — i.e. what
    /// `count_heap_slots` predicted. Only used to assert that the
    /// `for_each_heap_producer` walk stays in sync with the
    /// `root_heap_value` calls `compile_expr_multi` really makes: a
    /// producer the walk fails to count makes `root_heap_value` store
    /// past the end of the slot *and* leaves that root outside the `len`
    /// handed to `frog_frame_push`, so the GC never scans it — a silent,
    /// intermittent memory bug rather than a test failure.
    heap_max:      usize,
    /// Next unused `Variable` index for mutable-local codegen (see
    /// `get_or_declare_var`). Each function-body compile starts a fresh
    /// counter (0-based) — `Variable` indices only need to be unique within
    /// a single `FunctionBuilder`, not globally.
    var_counter:   u32,
    /// Field layout for every registered struct, from `TypeChecker::struct_defs`.
    /// Structs are represented unboxed: a struct-typed value is never one
    /// SSA `Value`, it's flattened into as many `Value`s as it has leaf
    /// scalar/heap fields (recursively, for nested struct fields) — see
    /// `struct_fields` and `compile_expr_multi`.
    structs:       &'a StructDefs,
    /// Layout for every registered nominal union, from
    /// `TypeChecker::union_defs`. A union value, unlike a struct, IS a
    /// single GC-boxed heap pointer (see `runtime::gc::FrogVariant`) — this
    /// is only consulted to resolve a field name to a slot offset
    /// (`enum_field_leaf_types`), never to flatten a union value into more
    /// than one `Value`.
    unions:        &'a UnionDefs,
}

/// True iff a value of this type is a GC-managed heap pointer.
fn is_heap_ty(ty: &Type) -> bool {
    // Both kinds of `Type::Union` are GC-boxed exactly like the old
    // `Type::Enum` was (`runtime::gc::FrogVariant`): a registered nominal
    // union (`data X is A | B`, see `Ctx.unions`) and an anonymous
    // structural one (`Int | Str`), whose boxes `TypedExprKind::Widen`
    // allocates — so this arm is load-bearing for rooting those too.
    matches!(ty, Type::Str | Type::List(_) | Type::Union(_))
}

/// Recursively flatten `ty` into its ordered leaf `(dotted_path, Type)`
/// list. For any non-struct type, returns a single `("", ty)` pair — the
/// empty path lets `var_key` degrade to exactly today's plain `vars["name"]`
/// scheme for every existing scalar type, so nothing about non-struct
/// codegen changes. For `Type::Struct(name)`, recurses into each declared
/// field (in declaration order) so a struct-typed field is expanded inline
/// rather than nested, e.g. `Company{ceo: Person{name, age}}` flattens to
/// `[("ceo.name", Str), ("ceo.age", Int)]`.
pub fn struct_fields(ty: &Type, structs: &StructDefs) -> Vec<(String, Type)> {
    match ty {
        Type::Struct(name) => {
            let fields = structs.get(name).cloned().unwrap_or_default();
            let mut out = Vec::new();
            for (fname, fty) in fields {
                for (sub_path, sub_ty) in struct_fields(&fty, structs) {
                    let path = if sub_path.is_empty() { fname.clone() } else { format!("{}.{}", fname, sub_path) };
                    out.push((path, sub_ty));
                }
            }
            out
        },
        _ => vec![(String::new(), ty.clone())],
    }
}

/// Build the `vars` map key for leaf `leaf_path` (from `struct_fields`) of
/// the binding named `base`. For a scalar binding (`leaf_path == ""`) this
/// is just `base` — identical to every key used before structs existed.
fn var_key(base: &str, leaf_path: &str) -> String {
    if leaf_path.is_empty() { base.to_string() } else { format!("{}.{}", base, leaf_path) }
}

/// Number of heap-typed leaf fields in `ty` (0 for anything with none, 1 for
/// a plain `Str`/`List`, N for a struct with N heap-typed leaves). Each one
/// needs its own shadow-stack root — see call sites below.
fn heap_leaf_count(ty: &Type, structs: &StructDefs) -> usize {
    struct_fields(ty, structs).iter().filter(|(_, t)| is_heap_ty(t)).count()
}

/// Walk `expr` in exactly the recursion pattern `compile_expr` uses (including
/// skipping over nested `Function` bodies, which are compiled separately) and
/// invoke `f` once for every subexpression that allocates a new heap pointer
/// (once per heap-typed leaf field, for a struct-typed one).
fn for_each_heap_producer(expr: &Spanned<TypedExpr>, structs: &StructDefs, f: &mut impl FnMut()) {
    match &expr.item.kind {
        TypedExprKind::IntLit(_) | TypedExprKind::FloatLit(_)
        | TypedExprKind::BoolLit(_) | TypedExprKind::Var(_) => {},

        TypedExprKind::StrLit(_) => f(),

        TypedExprKind::Unary { expr: inner, .. } => for_each_heap_producer(inner, structs, f),

        TypedExprKind::Binary { op, left, right } => {
            for_each_heap_producer(left, structs, f);
            for_each_heap_producer(right, structs, f);
            // Only Str + Str (concat) allocates; Str == / != Str yields Bool.
            if *op == Token::Plus && left.item.ty == Type::Str {
                f();
            }
        },

        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            for_each_heap_producer(cond, structs, f);
            for_each_heap_producer(true_branch, structs, f);
            if let Some(fb) = false_branch {
                for_each_heap_producer(fb, structs, f);
            }
        },

        TypedExprKind::Call { callable, args } => {
            for_each_heap_producer(callable, structs, f);
            for arg in args { for_each_heap_producer(arg, structs, f); }
            // A struct-typed return re-roots one heap-typed leaf at a time
            // (see the `Call` arm of `compile_expr_multi`) — count matches.
            for _ in 0..heap_leaf_count(&expr.item.ty, structs) { f(); }
        },

        TypedExprKind::Index { target, index } => {
            for_each_heap_producer(target, structs, f);
            for_each_heap_producer(index, structs, f);
            // A heap-typed element read out of a list isn't a fresh
            // allocation, but it needs its own shadow-stack root all the
            // same: once read, it's only reachable from the containing
            // list, which may itself go unrooted (e.g. a temporary list
            // literal) before this value is done being used.
            for _ in 0..heap_leaf_count(&expr.item.ty, structs) { f(); }
        },

        TypedExprKind::Slice { target, start, end } => {
            for_each_heap_producer(target, structs, f);
            if let Some(s) = start { for_each_heap_producer(s, structs, f); }
            if let Some(e) = end { for_each_heap_producer(e, structs, f); }
            // Unlike Index, a slice always allocates a brand-new list.
            f();
        },

        TypedExprKind::Range { start, end } => {
            for_each_heap_producer(start, structs, f);
            for_each_heap_producer(end, structs, f);
            f(); // always allocates the materialized list
        },

        TypedExprKind::Assign { value, .. } => {
            // Mirrors compile_expr's Assign arm, which never visits a
            // Function value (it's compiled separately as a top-level fn).
            if !matches!(value.item.kind, TypedExprKind::Function { .. }) {
                for_each_heap_producer(value, structs, f);
            }
        },

        TypedExprKind::Function { .. } => {},

        TypedExprKind::List(elems) => {
            for e in elems { for_each_heap_producer(e, structs, f); }
            f();
        },

        TypedExprKind::Block(stmts) => {
            for s in stmts { for_each_heap_producer(s, structs, f); }
        },

        TypedExprKind::ForLoop { iterable, cond, body, .. } => {
            for_each_heap_producer(iterable, structs, f);
            // Reading a heap-typed element out of the list each iteration
            // needs its own root, same reasoning as `Index` above — see
            // `compile_for_loop`'s `root_heap_value(bcx, ctx, elem)` call.
            for _ in 0..elem_heap_leaf_count(iterable, structs) { f(); }
            if let Some(c) = cond { for_each_heap_producer(c, structs, f); }
            for_each_heap_producer(body, structs, f);
        },

        TypedExprKind::Comprehension { iterable, cond, body, .. } => {
            // The result list is allocated (and rooted) before the loop
            // starts — see `compile_expr`'s `Comprehension` arm.
            f();
            for_each_heap_producer(iterable, structs, f);
            for _ in 0..elem_heap_leaf_count(iterable, structs) { f(); }
            if let Some(c) = cond { for_each_heap_producer(c, structs, f); }
            for_each_heap_producer(body, structs, f);
        },

        // A struct value is never itself a single heap pointer — it's
        // flattened into its leaf fields (see `struct_fields`), each rooted
        // individually wherever it's actually produced. So unlike `List`,
        // `StructInit` calls `f()` for its *fields'* producers only, never
        // for itself.
        TypedExprKind::StructInit { fields, .. } => {
            for (_, v) in fields { for_each_heap_producer(v, structs, f); }
        },

        // Reading a field off an already-bound struct isn't itself a new
        // heap-value producer (its leaf is a `Variable`, already rooted
        // wherever it was produced) — only `target` might be (e.g.
        // `f().name`). A union-typed target is different: its fields live
        // in heap memory, so *reading* one is a fresh `Value` each time,
        // same as a list-element read (`Index`, above) — needs its own root.
        TypedExprKind::FieldAccess { target, enum_name, .. } => {
            for_each_heap_producer(target, structs, f);
            if enum_name.is_some() {
                for _ in 0..heap_leaf_count(&expr.item.ty, structs) { f(); }
            }
        },

        TypedExprKind::FieldAssign { value, .. } => for_each_heap_producer(value, structs, f),

        // A new heap object, just like `List`/`Slice`/`Range` — visits its
        // fields' own producers first, then itself.
        TypedExprKind::VariantInit { fields, .. } => {
            for (_, v) in fields { for_each_heap_producer(v, structs, f); }
            // A payload-less variant compiles to an immediate, not an
            // allocation, so it produces nothing to root — see the matching
            // arm in `compile_expr_multi`.
            if !fields.is_empty() { f(); }
        },

        // A runtime tag test — no allocation; only `target`'s own
        // producers (if any) matter.
        TypedExprKind::IsVariant { target, .. } => for_each_heap_producer(target, structs, f),

        // Reading a variant's own field is a fresh heap read, exactly like
        // the enum arm of `FieldAccess` above.
        TypedExprKind::VariantField { target, .. } => {
            for_each_heap_producer(target, structs, f);
            for _ in 0..heap_leaf_count(&expr.item.ty, structs) { f(); }
        },

        // `return` itself allocates nothing — whatever `value` produces is
        // already accounted for by recursing into it.
        TypedExprKind::Return(value) => {
            if let Some(v) = value { for_each_heap_producer(v, structs, f); }
        },

        TypedExprKind::NoneLit => {},

        // Boxes `value` into a new heap cell — unless `value`'s type is
        // `None`, which is already the immediate `1` and needs no
        // allocation at all (see `TypedExprKind::Widen`'s doc comment).
        TypedExprKind::Widen { value, .. } => {
            for_each_heap_producer(value, structs, f);
            if value.item.ty != Type::None { f(); }
        },

        // Unboxes a payload slot: no fresh allocation, but the unboxed
        // pointer is a fresh heap *read* that `compile_expr_multi` roots
        // via `read_variant_slots` — one slot per heap-typed leaf of the
        // narrowed type, exactly like `VariantField` above.
        TypedExprKind::Narrow { value, .. } => {
            for_each_heap_producer(value, structs, f);
            for _ in 0..heap_leaf_count(&expr.item.ty, structs) { f(); }
        },

        // A runtime tag test on an anonymous union — no allocation, exactly
        // like `IsVariant`.
        TypedExprKind::TypeTag { target, .. } => for_each_heap_producer(target, structs, f),
    }
}

/// Number of heap-typed leaf fields in `iterable`'s element type
/// (`iterable.item.ty` is always `List(elem)` after type checking) — 0 for
/// a scalar element, 1 for a plain `Str`/`List` element, N for a struct
/// element with N heap-typed leaves.
fn elem_heap_leaf_count(iterable: &Spanned<TypedExpr>, structs: &StructDefs) -> usize {
    match &iterable.item.ty {
        Type::List(inner) => heap_leaf_count(inner, structs),
        _ => 0,
    }
}

/// Count the heap-pointer-producing subexpressions in `expr` — the number of
/// shadow-stack slots its compiled function needs.
fn count_heap_slots(expr: &Spanned<TypedExpr>, structs: &StructDefs) -> usize {
    let mut n = 0usize;
    for_each_heap_producer(expr, structs, &mut || n += 1);
    n
}

/// Store a freshly-produced heap pointer into the next shadow-stack slot, if
/// this function has one (no-op for functions with no heap-typed values).
fn root_heap_value(bcx: &mut FunctionBuilder, ctx: &mut Ctx, val: Value) {
    if let Some(slot) = ctx.heap_slot {
        debug_assert!(
            ctx.heap_cursor < ctx.heap_max,
            "shadow frame overflow: slot {} of {} — `for_each_heap_producer` undercounts this expression's heap producers",
            ctx.heap_cursor, ctx.heap_max,
        );
        let offset = (ctx.heap_cursor * 8) as i32;
        bcx.ins().stack_store(val, slot, offset);
        ctx.heap_cursor += 1;
    }
}

// ── Inline heap-object access ────────────────────────────────────────────────
//
// Reading a list element, reading or writing a variant payload slot, and
// appending to a list with spare capacity are each a single load or store at
// an offset fixed by the object's own layout.  Routing them through the
// `frog_*` FFI symbols made them opaque out-of-line calls instead, and those
// calls — not the memory traffic they perform — dominated the `orders`
// benchmark's profile.  The hot paths below emit the memory operation
// directly.  The FFI entry points stay: they are still what the paths with
// real runtime logic use (negative-index resolution, list growth) and what
// the embedding API calls from Rust.

/// Flags for accesses to a live GC object: the pointer came from `alloc`, so
/// it is non-null and 8-byte aligned, and every offset here is derived from
/// the object's declared layout, so nothing can trap.
fn heap_mem() -> MemFlags { MemFlags::trusted() }

/// Address of raw slot `slot` in `list`'s flat data buffer. Reloads `data`
/// on each use rather than hoisting it, since a push can reallocate the
/// buffer out from under a cached copy.
fn list_slot_addr(bcx: &mut FunctionBuilder, list: Value, slot: Value) -> Value {
    let data = bcx.ins().load(types::I64, heap_mem(), list, offset_of!(FrogList, data) as i32);
    let byte_off = bcx.ins().imul_imm(slot, 8);
    bcx.ins().iadd(data, byte_off)
}

/// A list's per-element slot count, normalized to at least 1 exactly as
/// `frog_list_len` and `frog_list_get` do.
fn list_stride(bcx: &mut FunctionBuilder, list: Value) -> Value {
    let raw = bcx.ins().load(types::I32, heap_mem(), list, offset_of!(FrogList, stride) as i32);
    let s = bcx.ins().uextend(types::I64, raw);
    let is_zero = bcx.ins().icmp_imm(IntCC::Equal, s, 0);
    let one = bcx.ins().iconst(types::I64, 1);
    bcx.ins().select(is_zero, one, s)
}

/// Byte offset of payload slot `slot` within a `FrogVariant`.
fn variant_slot_offset(slot: usize) -> i32 {
    (size_of::<FrogVariant>() + slot * 8) as i32
}

/// Append one raw slot to `list`. The common case — spare capacity, so the
/// push is a store plus a length bump — is inline; growing the buffer still
/// goes through `frog_list_push`, which has to reallocate and report the new
/// bytes to the GC.
fn emit_list_push(bcx: &mut FunctionBuilder, ctx: &mut Ctx, list: Value, val: Value) {
    let len = bcx.ins().load(types::I32, heap_mem(), list, offset_of!(FrogList, len) as i32);
    let cap = bcx.ins().load(types::I32, heap_mem(), list, offset_of!(FrogList, cap) as i32);
    let has_room = bcx.ins().icmp(IntCC::UnsignedLessThan, len, cap);

    let fast_bb = bcx.create_block();
    let slow_bb = bcx.create_block();
    let done_bb = bcx.create_block();
    bcx.ins().brif(has_room, fast_bb, &[], slow_bb, &[]);

    bcx.switch_to_block(fast_bb);
    bcx.seal_block(fast_bb);
    let len64 = bcx.ins().uextend(types::I64, len);
    let addr = list_slot_addr(bcx, list, len64);
    bcx.ins().store(heap_mem(), val, addr, 0);
    let next_len = bcx.ins().iadd_imm(len, 1);
    bcx.ins().store(heap_mem(), next_len, list, offset_of!(FrogList, len) as i32);
    bcx.ins().jump(done_bb, &[]);

    bcx.switch_to_block(slow_bb);
    bcx.seal_block(slow_bb);
    let push_id = ctx.func_ids["frog_list_push"];
    let push_ref = ctx.module.declare_func_in_func(push_id, bcx.func);
    bcx.ins().call(push_ref, &[list, val]);
    bcx.ins().jump(done_bb, &[]);

    bcx.switch_to_block(done_bb);
    bcx.seal_block(done_bb);
}

/// Emit the runtime test `val is <the variant at index `tag`>` for an enum
/// value of `def`'s enum, as an `I8` boolean.
///
/// Which code this needs comes down to which representations `val` can
/// actually have (see gc.rs's "Immediate (unboxed) values"):
///
///   * the tested variant is payload-less — then it is unboxed, and every
///     other value of this enum (boxed or not) has a different bit pattern,
///     so the whole test is one comparison against a constant;
///   * the enum has no payload-less variant at all — then `val` is always a
///     pointer, so the tag can be loaded unconditionally;
///   * otherwise `val` may be an immediate, which can never match a
///     payload-carrying variant but must not be dereferenced to find that
///     out — so the load is guarded by the low-bit test.
fn emit_is_variant(bcx: &mut FunctionBuilder, val: Value, def: &UnionDef, tag: u32) -> Value {
    let target_is_unit = def.common.is_empty()
        && def.variants.get(tag as usize).is_some_and(|(_, fs)| fs.is_empty());
    let any_unit = def.common.is_empty()
        && def.variants.iter().any(|(_, fs)| fs.is_empty());
    emit_tag_test(bcx, val, target_is_unit, any_unit, tag)
}

/// Shared by `emit_is_variant` (a nominal union's own declared tag scheme)
/// and codegen's anonymous-union `TypeTag` test — same runtime shapes
/// (see gc.rs's "Immediate (unboxed) values"), different source of the
/// `target_is_immediate`/`any_immediate` facts (a nominal union's
/// `UnionDef` vs. an anonymous union's flat `Type::Union` member list,
/// where the only immediate member can ever be `Type::None` — see
/// `TypedExprKind::Widen`'s doc comment).
///
/// Which code this needs comes down to which representations `val` can
/// actually have:
///
///   * the tested member is immediate — then every other value of this
///     union (boxed or not) has a different bit pattern, so the whole test
///     is one comparison against a constant;
///   * the union has no immediate member at all — then `val` is always a
///     pointer, so the tag can be loaded unconditionally;
///   * otherwise `val` may be an immediate, which can never match a
///     boxed member but must not be dereferenced to find that out — so the
///     load is guarded by the low-bit test.
fn emit_tag_test(bcx: &mut FunctionBuilder, val: Value, target_is_immediate: bool, any_immediate: bool, tag: u32) -> Value {
    if target_is_immediate {
        return bcx.ins().icmp_imm(IntCC::Equal, val, gc::immediate_variant(tag));
    }

    if !any_immediate {
        let actual = bcx.ins().load(types::I32, heap_mem(), val, offset_of!(FrogVariant, tag) as i32);
        return bcx.ins().icmp_imm(IntCC::Equal, actual, tag as i64);
    }

    let boxed_bb = bcx.create_block();
    let done_bb  = bcx.create_block();
    bcx.append_block_param(done_bb, types::I8);

    let is_immediate = bcx.ins().band_imm(val, 1);
    let no = bcx.ins().iconst(types::I8, 0);
    bcx.ins().brif(is_immediate, done_bb, &[no], boxed_bb, &[]);

    bcx.switch_to_block(boxed_bb);
    bcx.seal_block(boxed_bb);
    let actual = bcx.ins().load(types::I32, heap_mem(), val, offset_of!(FrogVariant, tag) as i32);
    let matched = bcx.ins().icmp_imm(IntCC::Equal, actual, tag as i64);
    bcx.ins().jump(done_bb, &[matched]);

    bcx.switch_to_block(done_bb);
    bcx.seal_block(done_bb);
    bcx.block_params(done_bb)[0]
}

/// Allocate a `FrogVariant`-shaped box holding `flat_vals` (already-computed
/// leaf values, e.g. from `struct_fields`-flattening a struct, or a single
/// scalar) tagged `tag`, and store them in. Shared by `VariantInit` (a
/// nominal union's non-nullary member) and `TypedExprKind::Widen` (boxing a
/// scalar or plain struct into an anonymous union) — both need exactly the
/// same runtime shape, just reached from different typed-AST nodes.
fn box_into_variant(tag: u32, flat_vals: &[Value], flat_types: &[Type], bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Value {
    let mut ptr_mask: i64 = 0;
    for (i, t) in flat_types.iter().enumerate() {
        if is_heap_ty(t) { ptr_mask |= 1i64 << i; }
    }
    let tag_val    = bcx.ins().iconst(types::I64, tag as i64);
    let nslots_val = bcx.ins().iconst(types::I64, flat_vals.len() as i64);
    let mask_val   = bcx.ins().iconst(types::I64, ptr_mask);

    let alloc_id  = ctx.func_ids["frog_alloc_variant"];
    let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
    let call      = bcx.ins().call(alloc_ref, &[tag_val, nslots_val, mask_val]);
    let ptr       = bcx.inst_results(call)[0];
    // Root the new object itself before populating it — matches the
    // traversal order `for_each_heap_producer` uses for both callers
    // (fields'/value's own producers first, then `f()` for this box).
    root_heap_value(bcx, ctx, ptr);

    for (i, (v, t)) in flat_vals.iter().zip(flat_types.iter()).enumerate() {
        let wire = to_i64_repr(bcx, t, *v);
        bcx.ins().store(heap_mem(), wire, ptr, variant_slot_offset(i));
    }
    ptr
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

/// A zero of Cranelift type `t`, for a value slot that is never actually
/// observed (the results of an unreachable block's terminator, or a missing
/// `else` branch's contribution to a merge block). `iconst` is integer-only,
/// so float slots must use the float constant instructions instead —
/// `iconst` on an `F64` trips the Cranelift verifier.
fn placeholder_value(bcx: &mut FunctionBuilder, t: types::Type) -> Value {
    if t == types::F64 {
        bcx.ins().f64const(0.0)
    } else if t == types::F32 {
        bcx.ins().f32const(0.0)
    } else {
        bcx.ins().iconst(t, 0)
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

/// Emit a non-GC string fragment used while formatting composite values.
fn print_fragment(text: &str, bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    let bytes = text.as_bytes().to_vec();
    let ptr = bytes.as_ptr() as i64;
    let len = bytes.len() as i64;
    ctx.string_arena.push(bytes);
    let data = bcx.ins().iconst(types::I64, ptr);
    let len = bcx.ins().iconst(types::I64, len);
    let id = ctx.func_ids["frog_bytes_print"];
    let callee = ctx.module.declare_func_in_func(id, bcx.func);
    bcx.ins().call(callee, &[data, len]);
}

/// The `kind` discriminant `frog_list_print`/`frog_list_println` (in
/// runtime/ffi.rs) switch on to render a list's element type. Both codegen
/// call sites that build this discriminant must agree on the encoding.
fn list_elem_kind(elem_ty: &Type) -> i64 {
    match elem_ty {
        Type::Int => 0,
        Type::Float => 1,
        Type::Bool => 2,
        Type::Str => 3,
        Type::List(_) => 4,
        Type::Struct(_) => 5,
        _ => 6,
    }
}

/// Print one value without a trailing newline. Structs are represented as a
/// sequence of flattened leaf values, so this recursively consumes that
/// sequence according to the declared field layout.
fn print_value(ty: &Type, values: &[Value], cursor: &mut usize, bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    match ty {
        Type::Struct(name) => {
            print_fragment(&format!("{}(", name), bcx, ctx);
            let fields = ctx.structs.get(name).expect("known struct in codegen");
            for (i, (field, field_ty)) in fields.iter().enumerate() {
                if i != 0 { print_fragment(", ", bcx, ctx); }
                print_fragment(&format!("{}=", field), bcx, ctx);
                print_value(field_ty, values, cursor, bcx, ctx);
            }
            print_fragment(")", bcx, ctx);
        }
        Type::Str => {
            let id = ctx.func_ids["frog_str_repr_print"];
            let callee = ctx.module.declare_func_in_func(id, bcx.func);
            bcx.ins().call(callee, &[values[*cursor]]);
            *cursor += 1;
        }
        Type::Int | Type::Float | Type::Bool | Type::List(_) => {
            let (id, extra) = match ty {
                Type::Int => ("frog_int_print", None),
                Type::Float => ("frog_float_print", None),
                Type::Bool => ("frog_bool_print", None),
                Type::List(inner) => ("frog_list_print", Some(list_elem_kind(inner))),
                _ => unreachable!(),
            };
            let callee = ctx.module.declare_func_in_func(ctx.func_ids[id], bcx.func);
            if let Some(kind) = extra {
                let kind = bcx.ins().iconst(types::I64, kind);
                bcx.ins().call(callee, &[values[*cursor], kind]);
            } else {
                bcx.ins().call(callee, &[values[*cursor]]);
            }
            *cursor += 1;
        }
        other => panic!("print codegen does not support {:?}", other),
    }
}

/// Compile a scalar (non-struct-typed) expression into Cranelift IR,
/// returning its single SSA value. A thin convenience wrapper around
/// `compile_expr_multi` for the many call sites whose operand is always
/// scalar by construction (numeric/boolean operators, conditions, indices,
/// slice bounds, string operations — none of these ever admit a
/// struct-typed operand under the type system). Panics (via the
/// `debug_assert`) if called on a struct-typed expression — those call sites
/// must use `compile_expr_multi` directly.
fn compile_expr(
    expr: &Spanned<TypedExpr>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Value {
    let mut vs = compile_expr_multi(expr, bcx, vars, ctx);
    debug_assert_eq!(vs.len(), 1,
        "compile_expr called on a multi-valued (struct-typed) expression of type {:?} — use compile_expr_multi", expr.item.ty);
    vs.pop().unwrap()
}

/// Compile a typed expression into Cranelift IR, returning its SSA value(s).
///
/// Every froglang value used to be exactly one Cranelift `Value`; structs
/// broke that, since they're represented unboxed — a struct-typed
/// expression compiles to as many `Value`s as it has leaf scalar/heap
/// fields (recursively, for nested struct fields — see `struct_fields`).
/// Every non-struct-typed expression still produces exactly one `Value`,
/// wrapped in a one-element `Vec`, so nothing about existing (pre-struct)
/// codegen changes in substance.
///
/// `ctx.string_arena` keeps source `Vec<u8>` buffers alive until the JIT
/// executes; `frog_alloc_str` copies bytes immediately, so the arena only
/// needs to outlive the call to the compiled function.
///
/// Every subexpression that allocates a new heap pointer (see
/// `for_each_heap_producer`) is stored into `ctx`'s shadow-stack slot via
/// `root_heap_value` immediately after being produced, so it stays visible to
/// the GC's mark phase for the remainder of this function's execution.
fn compile_expr_multi(
    expr: &Spanned<TypedExpr>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Vec<Value> {
    match &expr.item.kind {
        TypedExprKind::IntLit(n) => vec![bcx.ins().iconst(types::I64, *n)],

        TypedExprKind::BoolLit(b) => vec![bcx.ins().iconst(types::I8, *b as i64)],

        TypedExprKind::FloatLit(f) => vec![bcx.ins().f64const(*f)],

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
            vec![result]
        },

        TypedExprKind::Var(name) => {
            struct_fields(&expr.item.ty, ctx.structs).iter().map(|(path, _)| {
                let key = var_key(name, path);
                let var = *vars.get(&key)
                    .unwrap_or_else(|| panic!("unbound variable in codegen: {}", key));
                bcx.use_var(var)
            }).collect()
        },

        TypedExprKind::Unary { op, expr: inner } => {
            let v = compile_expr(inner, bcx, vars, ctx);
            vec![match op {
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
            }]
        },

        TypedExprKind::Binary { op, left, right } => {
            // ── String operations (must short-circuit before numeric path) ──
            if left.item.ty == Type::Str {
                let lv = compile_expr(left,  bcx, vars, ctx);
                let rv = compile_expr(right, bcx, vars, ctx);
                return vec![match op {
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
                }];
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
                return vec![bcx.block_params(merge_bb)[0]];
            }

            // ── Numeric operations ──────────────────────────────────────────
            let lv = compile_expr(left,  bcx, vars, ctx);
            let rv = compile_expr(right, bcx, vars, ctx);
            let op_ty = numeric_join(&left.item.ty, &right.item.ty).unwrap_or_else(|| left.item.ty.clone());
            let lv = coerce_value(lv, &left.item.ty, &op_ty, bcx);
            let rv = coerce_value(rv, &right.item.ty, &op_ty, bcx);
            let is_float = op_ty == Type::Float;
            vec![match op {
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
            }]
        },

        TypedExprKind::Conditional { cond, true_branch, false_branch } => {
            let cond_val = compile_expr(cond, bcx, vars, ctx);

            let true_bb  = bcx.create_block();
            let false_bb = bcx.create_block();
            let merge_bb = bcx.create_block();

            // `expr.item.ty == Never` means *both* branches terminate
            // (each is itself `Never`-typed, or — a match's "missing tail"
            // default, see `lower_match` — is absent and stands for
            // provably-unreachable code; the type system only ever unifies
            // to `Never` when every contributing side is `Never`, so this
            // is inductive, not an assumption). `?`/`!`'s match desugaring
            // (`ERRORS.md` Phase 5) is the first thing that actually builds
            // this shape at codegen — every arm before it always paired a
            // `Never` branch with a real value on the other side. Since
            // nothing downstream of this conditional is ever reachable,
            // `merge_bb` gets no value and no param; each branch supplies
            // its own terminator (`return_`/a nested `Never` conditional's
            // own trap, or a `Never`-returning `Call` — e.g. `panic` — own
            // trap), or — the "missing tail" case — a `trap` here.
            // `merge_bb` itself is simply never reached and stays
            // unlaid-out.
            if expr.item.ty == Type::Never {
                bcx.ins().brif(cond_val, true_bb, &[], false_bb, &[]);

                bcx.switch_to_block(true_bb);
                bcx.seal_block(true_bb);
                compile_expr_multi(true_branch, bcx, vars, ctx);
                if true_branch.item.ty != Type::Never {
                    bcx.ins().trap(TrapCode::user(2).expect("2 is a valid user trap code"));
                }

                bcx.switch_to_block(false_bb);
                bcx.seal_block(false_bb);
                match false_branch {
                    Some(fb) => {
                        compile_expr_multi(fb, bcx, vars, ctx);
                        if fb.item.ty != Type::Never {
                            bcx.ins().trap(TrapCode::user(2).expect("2 is a valid user trap code"));
                        }
                    }
                    None => { bcx.ins().trap(TrapCode::user(2).expect("2 is a valid user trap code")); }
                }

                // Every path above already ended in a terminator (a nested
                // `Never` conditional's own trap, `return_`, or the `trap`
                // just emitted) — this conditional itself is `Never`-typed,
                // so it might be the tail of its enclosing function body
                // (`build_func_body`'s own tail `return_`/`teardown_shadow_frame`
                // would otherwise try to append to an already-filled
                // block), or nested inside a `Block`/another `Conditional`
                // that keeps building after it. Either way it needs a
                // fresh block to land in — see `Return`'s codegen.
                let dead = bcx.create_block();
                bcx.switch_to_block(dead);
                bcx.seal_block(dead);
                return Vec::new();
            }

            let has_value = expr.item.ty != Type::None;
            let is_struct = matches!(&expr.item.ty, Type::Struct(_));

            if !is_struct {
                // ── scalar path, unchanged from before structs existed ──
                let result_ty = cl_type(&expr.item.ty);
                if has_value {
                    bcx.append_block_param(merge_bb, result_ty);
                }

                bcx.ins().brif(cond_val, true_bb, &[], false_bb, &[]);

                bcx.switch_to_block(true_bb);
                bcx.seal_block(true_bb);
                // `compile_expr_multi`, not `compile_expr` — a `Never`-typed
                // branch (a `return`) yields zero values, which the
                // single-value wrapper's assertion would reject; every other
                // scalar branch still yields exactly one, unpacked below.
                let tv = compile_expr_multi(true_branch, bcx, vars, ctx);
                // A `Never`-typed branch has already emitted its own
                // terminator — jumping to `merge_bb` on top of that would
                // be a second terminator in the same block, which
                // Cranelift rejects. Every other branch shape reaches here
                // normally and joins as before.
                if true_branch.item.ty != Type::Never {
                    if has_value {
                        let tv = ensure_width(tv[0], &true_branch.item.ty, result_ty, bcx);
                        bcx.ins().jump(merge_bb, &[tv]);
                    } else {
                        bcx.ins().jump(merge_bb, &[]);
                    }
                }

                bcx.switch_to_block(false_bb);
                bcx.seal_block(false_bb);
                if let Some(fb) = false_branch {
                    let fv = compile_expr_multi(fb, bcx, vars, ctx);
                    if fb.item.ty != Type::Never {
                        if has_value {
                            let fv = ensure_width(fv[0], &fb.item.ty, result_ty, bcx);
                            bcx.ins().jump(merge_bb, &[fv]);
                        } else {
                            bcx.ins().jump(merge_bb, &[]);
                        }
                    }
                } else if has_value {
                    let fv = bcx.ins().iconst(result_ty, 0);
                    bcx.ins().jump(merge_bb, &[fv]);
                } else {
                    bcx.ins().jump(merge_bb, &[]);
                }

                bcx.switch_to_block(merge_bb);
                bcx.seal_block(merge_bb);

                if has_value {
                    vec![bcx.block_params(merge_bb)[0]]
                } else {
                    vec![bcx.ins().iconst(types::I64, 0)]
                }
            } else {
                // ── struct-typed path: K block params, one per leaf field.
                // No widening needed — struct unification is nominal/exact,
                // so both branches' leaf types are identical to expr's own.
                let leafs = struct_fields(&expr.item.ty, ctx.structs);
                let param_tys: Vec<types::Type> = leafs.iter().map(|(_, t)| cl_type(t)).collect();
                for t in &param_tys { bcx.append_block_param(merge_bb, *t); }

                bcx.ins().brif(cond_val, true_bb, &[], false_bb, &[]);

                bcx.switch_to_block(true_bb);
                bcx.seal_block(true_bb);
                let tv = compile_expr_multi(true_branch, bcx, vars, ctx);
                // See the scalar path above for why a `Never`-typed branch
                // must not also jump — it already terminated itself.
                if true_branch.item.ty != Type::Never {
                    bcx.ins().jump(merge_bb, &tv);
                }

                bcx.switch_to_block(false_bb);
                bcx.seal_block(false_bb);
                match false_branch {
                    Some(fb) => {
                        let fv = compile_expr_multi(fb, bcx, vars, ctx);
                        if fb.item.ty != Type::Never {
                            bcx.ins().jump(merge_bb, &fv);
                        }
                    }
                    None => {
                        let fv: Vec<Value> = param_tys.iter().map(|&t| placeholder_value(bcx, t)).collect();
                        bcx.ins().jump(merge_bb, &fv);
                    }
                };

                bcx.switch_to_block(merge_bb);
                bcx.seal_block(merge_bb);
                bcx.block_params(merge_bb).to_vec()
            }
        },

        TypedExprKind::Call { callable, args } => {
            let func_name = match &callable.item.kind {
                TypedExprKind::Var(name) => name.clone(),
                _ => panic!("only named function calls supported in codegen"),
            };

            // `print` accepts values of any type.  Its runtime entry
            // point is selected here, after type checking has established the
            // concrete argument type, so no invalid Str coercion is emitted.
            if func_name == "print" {
                let arg = &args[0];
                if matches!(&arg.item.ty, Type::Struct(_)) {
                    let values = compile_expr_multi(arg, bcx, vars, ctx);
                    let mut cursor = 0;
                    print_value(&arg.item.ty, &values, &mut cursor, bcx, ctx);
                    print_fragment("\n", bcx, ctx);
                    return vec![bcx.ins().iconst(types::I64, 0)];
                }
                let arg_val = compile_expr(arg, bcx, vars, ctx);
                let (rt_name, extra_arg) = match &arg.item.ty {
                    Type::Str => ("print", None),
                    Type::Int => ("frog_int_println", None),
                    Type::Float => ("frog_float_println", None),
                    Type::Bool => ("frog_bool_println", None),
                    Type::List(inner) => ("frog_list_println", Some(list_elem_kind(inner))),
                    ty => panic!("print codegen does not support {:?}", ty),
                };
                let func_id = ctx.func_ids[rt_name];
                let callee = ctx.module.declare_func_in_func(func_id, bcx.func);
                if let Some(kind) = extra_arg {
                    let kind = bcx.ins().iconst(types::I64, kind);
                    bcx.ins().call(callee, &[arg_val, kind]);
                } else {
                    bcx.ins().call(callee, &[arg_val]);
                }
                return vec![bcx.ins().iconst(types::I64, 0)];
            }

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
                if matches!(&a.item.ty, Type::Struct(_)) {
                    // Struct args are never widened (nominal/exact match)
                    // — flatten straight into the call's arg list, in the
                    // same K-`AbiParam`-per-struct-arg order `make_sig` uses.
                    arg_vals.extend(compile_expr_multi(a, bcx, vars, ctx));
                } else {
                    let mut v = compile_expr(a, bcx, vars, ctx);
                    if let Some(param_ty) = param_types.get(i) {
                        v = coerce_value(v, &a.item.ty, param_ty, bcx);
                    }
                    arg_vals.push(v);
                }
            }

            let call = bcx.ins().call(local_callee, &arg_vals);
            if return_ty == Type::Never {
                // The callee never actually hands control back here —
                // either it's `panic` (its FFI print call genuinely does
                // return, so this `trap` is what actually stops execution
                // — see `default_context`'s registration and
                // `declare_rt`'s `"panic"` alias) or a user-declared
                // `: Never` function, whose own tail can only ever be
                // reached on a dead block (`build_func_body`'s Never-body
                // handling), so it never really falls through to return
                // control either — the trap is dead code there, but keeps
                // this block's IR well-formed regardless. Mirrors
                // `Conditional`'s own `Type::Never` codegen path exactly.
                bcx.ins().trap(TrapCode::user(2).expect("2 is a valid user trap code"));
                let dead = bcx.create_block();
                bcx.switch_to_block(dead);
                bcx.seal_block(dead);
                return Vec::new();
            }
            if return_ty == Type::None {
                vec![bcx.ins().iconst(types::I64, 0)]
            } else if matches!(&return_ty, Type::Struct(_)) {
                // Each heap-typed leaf of a struct return crosses the ABI
                // boundary as a bare register value — the callee's own
                // shadow frame (which rooted it during its own execution)
                // is already popped by the time we get here, so it must be
                // re-rooted into *this* function's frame immediately,
                // exactly like the scalar Str/List case below.
                let results = bcx.inst_results(call).to_vec();
                let leafs = struct_fields(&return_ty, ctx.structs);
                for (v, (_, lty)) in results.iter().zip(leafs.iter()) {
                    if is_heap_ty(lty) {
                        root_heap_value(bcx, ctx, *v);
                    }
                }
                results
            } else {
                let result = bcx.inst_results(call)[0];
                if is_heap_ty(&return_ty) {
                    root_heap_value(bcx, ctx, result);
                }
                vec![result]
            }
        },

        TypedExprKind::Index { target, index } => {
            let list_val = compile_expr(target, bcx, vars, ctx);
            let idx_val  = compile_expr(index, bcx, vars, ctx);

            let leafs = struct_fields(&expr.item.ty, ctx.structs);
            let get_id = ctx.func_ids["frog_list_get"];
            let mut results = Vec::with_capacity(leafs.len());
            for (i, (_, lty)) in leafs.iter().enumerate() {
                let callee = ctx.module.declare_func_in_func(get_id, bcx.func);
                let off_val = bcx.ins().iconst(types::I64, i as i64);
                let call = bcx.ins().call(callee, &[list_val, idx_val, off_val]);
                let raw = bcx.inst_results(call)[0];
                let result = from_i64_repr(bcx, lty, raw);
                if is_heap_ty(lty) {
                    root_heap_value(bcx, ctx, result);
                }
                results.push(result);
            }
            results
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
            vec![result]
        },

        TypedExprKind::Range { start, end } => {
            let start_val = compile_expr(start, bcx, vars, ctx);
            let end_val   = compile_expr(end, bcx, vars, ctx);

            let id     = ctx.func_ids["frog_range"];
            let callee = ctx.module.declare_func_in_func(id, bcx.func);
            let call   = bcx.ins().call(callee, &[start_val, end_val]);
            let result = bcx.inst_results(call)[0];
            root_heap_value(bcx, ctx, result);
            vec![result]
        },

        TypedExprKind::Assign { name, value } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                vec![bcx.ins().iconst(types::I64, 0)]
            } else {
                let vals = compile_expr_multi(value, bcx, vars, ctx);
                let leafs = struct_fields(&value.item.ty, ctx.structs);
                for (v, (path, lty)) in vals.iter().zip(leafs.iter()) {
                    let key = var_key(name, path);
                    let var = get_or_declare_var(bcx, vars, ctx, &key, lty);
                    bcx.def_var(var, *v);
                }
                vals
            }
        },

        TypedExprKind::Block(stmts) => {
            let mut last = vec![bcx.ins().iconst(types::I64, 0)];
            for stmt in stmts {
                last = compile_expr_multi(stmt, bcx, vars, ctx);
            }
            last
        },

        TypedExprKind::Function { .. } => {
            vec![bcx.ins().iconst(types::I64, 0)]
        },

        TypedExprKind::List(elems) => {
            let elem_ty = match &expr.item.ty {
                Type::List(inner) => (**inner).clone(),
                _ => Type::Int,
            };
            let leafs = struct_fields(&elem_ty, ctx.structs);
            let stride = (leafs.len().max(1)) as i64;
            let mut ptr_mask: i64 = 0;
            for (i, (_, lty)) in leafs.iter().enumerate() {
                if is_heap_ty(lty) { ptr_mask |= 1i64 << i; }
            }

            let n = elems.len() as i64;
            let cap_val    = bcx.ins().iconst(types::I64, n.max(1));
            let stride_val = bcx.ins().iconst(types::I64, stride);
            let mask_val   = bcx.ins().iconst(types::I64, ptr_mask);

            let alloc_id = ctx.func_ids["frog_alloc_list"];
            let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[cap_val, stride_val, mask_val]);
            let list_ptr = bcx.inst_results(alloc_call)[0];
            // Root the list itself *before* compiling its elements: an
            // element expression (e.g. a Str) can allocate and trigger a
            // collection, and the list must already be reachable by then.
            root_heap_value(bcx, ctx, list_ptr);

            for elem in elems {
                // A struct element compiles to `leafs.len()` values, pushed
                // back-to-back — matching `stride` exactly is what makes the
                // list's flat backing store self-describing (see FrogList's
                // doc comment in runtime/gc.rs).
                let evs = compile_expr_multi(elem, bcx, vars, ctx);
                for (ev, (_, lty)) in evs.iter().zip(leafs.iter()) {
                    // The list's backing store is a flat i64 buffer (see
                    // FrogList in runtime/gc.rs); Float and Bool elements need
                    // the same bitcast/zero-extend conversion applied to every
                    // other i64-wire-format value (see to_i64_repr). Without
                    // this, pushing an F64 or I8 SSA value into an i64-typed
                    // call argument is a Cranelift type mismatch — a "Verifier
                    // errors" panic, not a bug in the pushed value itself.
                    let ev = to_i64_repr(bcx, lty, *ev);
                    emit_list_push(bcx, ctx, list_ptr, ev);
                }
            }

            vec![list_ptr]
        },

        TypedExprKind::ForLoop { var, iterable, cond, body } => {
            compile_for_loop(var, iterable, cond, body, None, bcx, vars, ctx);
            vec![bcx.ins().iconst(types::I64, 0)]
        },

        TypedExprKind::Comprehension { var, iterable, cond, body } => {
            let leafs = struct_fields(&body.item.ty, ctx.structs);
            let stride = (leafs.len().max(1)) as i64;
            let mut ptr_mask: i64 = 0;
            for (i, (_, lty)) in leafs.iter().enumerate() {
                if is_heap_ty(lty) { ptr_mask |= 1i64 << i; }
            }

            let cap_val    = bcx.ins().iconst(types::I64, 1);
            let stride_val = bcx.ins().iconst(types::I64, stride);
            let mask_val   = bcx.ins().iconst(types::I64, ptr_mask);

            let alloc_id = ctx.func_ids["frog_alloc_list"];
            let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[cap_val, stride_val, mask_val]);
            let result_list = bcx.inst_results(alloc_call)[0];
            // Root the result list before the loop runs at all: it must
            // already be reachable by the time the first pushed element
            // (or the iterable itself) can trigger a collection.
            root_heap_value(bcx, ctx, result_list);

            compile_for_loop(var, iterable, cond, body, Some(result_list), bcx, vars, ctx);
            vec![result_list]
        },

        TypedExprKind::StructInit { fields, .. } => {
            // Already reordered into declared-field order by typeck — just
            // concatenate each field's own flattened leaf values in order.
            let mut out = Vec::new();
            for (_, v) in fields {
                out.extend(compile_expr_multi(v, bcx, vars, ctx));
            }
            out
        },

        TypedExprKind::FieldAccess { target, field, enum_name } => {
            match enum_name {
                Some(ename) => {
                    // A common field, read out of heap memory — unlike a
                    // struct's `Variable`-backed leaf, this is a fresh
                    // `Value` on every read, so each heap-typed slot roots
                    // itself (see `for_each_heap_producer`'s matching arm).
                    let ptr = compile_expr(target, bcx, vars, ctx);
                    let (offset, leaf_types) = enum_field_leaf_types(ename, None, field, ctx.structs, ctx.unions);
                    read_variant_slots(ptr, offset, &leaf_types, bcx, ctx)
                },
                None => {
                    let target_vals = compile_expr_multi(target, bcx, vars, ctx);
                    let (start, len) = field_slice_range(&target.item.ty, field, ctx.structs);
                    target_vals[start..start + len].to_vec()
                },
            }
        },

        TypedExprKind::FieldAssign { base, field, value } => {
            // Overwrite just the touched leaf `Variable`(s) — every other
            // field of `base` keeps its existing binding untouched. This
            // works precisely because a struct is a flat set of named
            // bindings, not one aggregate value: no read-modify-reconstruct
            // of the whole struct is needed, unlike a boxed representation.
            let vals = compile_expr_multi(value, bcx, vars, ctx);
            let leafs = struct_fields(&value.item.ty, ctx.structs);
            for (v, (sub_path, lty)) in vals.iter().zip(leafs.iter()) {
                let full_path = if sub_path.is_empty() { field.clone() } else { format!("{}.{}", field, sub_path) };
                let key = var_key(base, &full_path);
                let var = get_or_declare_var(bcx, vars, ctx, &key, lty);
                bcx.def_var(var, *v);
            }
            vec![bcx.ins().iconst(types::I64, 0)]
        },

        TypedExprKind::VariantInit { fields, tag, .. } => {
            // A variant with no fields at all — neither its own nor common
            // ones its enum declares — carries no information beyond its
            // tag, so it needs no heap object: emit the tag as an unboxed
            // immediate. `fields` is the enum's common fields followed by
            // this variant's own (see `check_and_lower`'s variant-call arm),
            // so it being empty is exactly the "nothing to store" test.
            // See gc.rs's "Immediate (unboxed) values" for the encoding and
            // why the GC can tell the two apart.
            if fields.is_empty() {
                return vec![bcx.ins().iconst(types::I64, gc::immediate_variant(*tag))];
            }

            // Compute every field's flattened leaf values first (mirrors
            // `StructInit` exactly) — each heap-typed leaf among them
            // roots itself already, via its own producer's codegen.
            let mut flat_vals: Vec<Value> = Vec::new();
            let mut flat_types: Vec<Type> = Vec::new();
            for (_, v) in fields {
                flat_vals.extend(compile_expr_multi(v, bcx, vars, ctx));
                flat_types.extend(struct_fields(&v.item.ty, ctx.structs).into_iter().map(|(_, t)| t));
            }
            vec![box_into_variant(*tag, &flat_vals, &flat_types, bcx, ctx)]
        },

        TypedExprKind::IsVariant { target, enum_name, tag, .. } => {
            let val = compile_expr(target, bcx, vars, ctx);
            let def = ctx.unions.get(enum_name).expect("known union in codegen").clone();
            vec![emit_is_variant(bcx, val, &def, *tag)]
        },

        TypedExprKind::VariantField { target, enum_name, variant, field } => {
            let ptr = compile_expr(target, bcx, vars, ctx);
            let (offset, leaf_types) = enum_field_leaf_types(enum_name, Some(variant), field, ctx.structs, ctx.unions);
            read_variant_slots(ptr, offset, &leaf_types, bcx, ctx)
        },

        TypedExprKind::Return(value) => {
            let results = match value {
                Some(v) => compile_expr_multi(v, bcx, vars, ctx),
                None => Vec::new(),
            };
            // Every path out of the function pops the shadow frame first —
            // this is an *early* exit, so it must do the same thing
            // `build_func_body`'s own tail `return_` does, not skip it.
            teardown_shadow_frame(bcx, ctx.module, ctx.func_ids, ctx.heap_slot);
            bcx.ins().return_(&results);
            // Cranelift requires every block to end in exactly one
            // terminator, and `return_` is one — so whatever IR follows
            // this `Return` in the source (there is always some: it sits
            // inside a `Block`/`Conditional` whose caller keeps building)
            // needs a fresh block to land in. Nothing ever jumps to it —
            // the branch/block that contains an unconditional `return`
            // has `Type::Never`, and the `Conditional` join (below) checks
            // for exactly that to skip emitting the jump — so this block
            // is genuinely unreachable, which Cranelift's verifier permits
            // as long as it's syntactically well-formed.
            let dead = bcx.create_block();
            bcx.switch_to_block(dead);
            bcx.seal_block(dead);
            Vec::new()
        },

        TypedExprKind::NoneLit => vec![bcx.ins().iconst(types::I64, 1)],

        TypedExprKind::Widen { value, tag } => {
            if value.item.ty == Type::None {
                // `None`'s own compiled form (the generic immediate `1` —
                // see `TypedExprKind::NoneLit`) isn't reused directly: this
                // union's own sorted member list may place `None` at a
                // different tag than `NoneLit`'s own site-independent
                // encoding, so it's re-encoded with *this* union's tag.
                // Still compile `value` first for any side effects (none
                // today, but `Widen` shouldn't assume that).
                let _ = compile_expr_multi(value, bcx, vars, ctx);
                vec![bcx.ins().iconst(types::I64, gc::immediate_variant(*tag))]
            } else {
                // Box `value` — a scalar or a plain struct — into a
                // `FrogVariant`-shaped cell the same way a nominal union's
                // non-nullary member already is. See `box_into_variant`.
                let flat_vals = compile_expr_multi(value, bcx, vars, ctx);
                let flat_types: Vec<Type> = struct_fields(&value.item.ty, ctx.structs).into_iter().map(|(_, t)| t).collect();
                vec![box_into_variant(*tag, &flat_vals, &flat_types, bcx, ctx)]
            }
        },

        TypedExprKind::Narrow { value, .. } => {
            if expr.item.ty == Type::None {
                // `value` here is an immediate, not a pointer — `None` has
                // no payload to unbox, and the caller already knows (from
                // a preceding `TypeTag`) which member this is. A single
                // dummy slot keeps this consistent with every other
                // member's arity (`struct_fields`'s generic 1-leaf
                // fallback for a non-struct type).
                let _ = compile_expr(value, bcx, vars, ctx);
                vec![bcx.ins().iconst(types::I64, 0)]
            } else {
                let ptr = compile_expr(value, bcx, vars, ctx);
                let leaf_types: Vec<Type> = struct_fields(&expr.item.ty, ctx.structs).into_iter().map(|(_, t)| t).collect();
                read_variant_slots(ptr, 0, &leaf_types, bcx, ctx)
            }
        },

        TypedExprKind::TypeTag { target, tag } => {
            let val = compile_expr(target, bcx, vars, ctx);
            let members = match &target.item.ty {
                Type::Union(members) => members,
                other => unreachable!("TypeTag target must be an anonymous union, got {}", other),
            };
            let target_is_immediate = members.get(*tag as usize) == Some(&Type::None);
            let any_immediate = members.iter().any(|m| *m == Type::None);
            vec![emit_tag_test(bcx, val, target_is_immediate, any_immediate, *tag)]
        },
    }
}

/// Read `leaf_types.len()` consecutive payload slots starting at `offset`
/// out of the `FrogVariant` at `ptr`, converting each back from its
/// `i64`-wire representation and — for a heap-typed leaf — rooting the
/// freshly-read pointer (it's only reachable via `ptr`, which may itself
/// go unrooted before this value is done being used, exactly like a
/// list-element read — see `for_each_heap_producer`).
fn read_variant_slots(ptr: Value, offset: usize, leaf_types: &[Type], bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Vec<Value> {
    let mut out = Vec::with_capacity(leaf_types.len());
    for (i, lty) in leaf_types.iter().enumerate() {
        // Only a variant that has payload slots to read is ever boxed, so
        // `ptr` here is always a real pointer, never an unboxed immediate.
        let raw = bcx.ins().load(types::I64, heap_mem(), ptr, variant_slot_offset(offset + i));
        let v = from_i64_repr(bcx, lty, raw);
        if is_heap_ty(lty) { root_heap_value(bcx, ctx, v); }
        out.push(v);
    }
    out
}

/// Locate field `field` within one enum's runtime payload layout: common
/// fields (declared order) first, then — if `variant` is given — that
/// variant's own fields (declared order) appended right after. Returns the
/// starting slot offset and the field's own flattened leaf types (len 1
/// for a scalar/heap-pointer field, >1 for a nested-struct field) — mirrors
/// `field_slice_range` for structs, generalized to the enum's two-part
/// (common, variant) layout. `variant: None` is used for an ordinary
/// common-field `FieldAccess` (the field must be common — enforced during
/// typeck); `variant: Some(v)` is used for a match-bound `VariantField`.
fn enum_field_leaf_types(enum_name: &str, variant: Option<&str>, field: &str, structs: &StructDefs, unions: &UnionDefs) -> (usize, Vec<Type>) {
    let def = unions.get(enum_name).expect("known enum in codegen");
    let mut offset = 0;
    for (fname, fty) in &def.common {
        let leaves = struct_fields(fty, structs);
        if fname == field {
            return (offset, leaves.into_iter().map(|(_, t)| t).collect());
        }
        offset += leaves.len();
    }
    if let Some(vname) = variant {
        if let Some((_, vfields)) = def.variants.iter().find(|(n, _)| n == vname) {
            for (fname, fty) in vfields {
                let leaves = struct_fields(fty, structs);
                if fname == field {
                    return (offset, leaves.into_iter().map(|(_, t)| t).collect());
                }
                offset += leaves.len();
            }
        }
    }
    panic!("field '{}' not found on enum {} in codegen", field, enum_name);
}

/// Locate field `field` within struct-typed `struct_ty`'s flattened leaf
/// layout, returning `(start, len)` — the slice of `struct_fields(struct_ty)`
/// (and, correspondingly, of `compile_expr_multi`'s output for any
/// expression of that type) that `field` occupies. `len` is `1` for a
/// scalar/heap-pointer field, >1 for a nested-struct field.
fn field_slice_range(struct_ty: &Type, field: &str, structs: &StructDefs) -> (usize, usize) {
    let name = match struct_ty {
        Type::Struct(n) => n,
        _ => unreachable!("field_slice_range called on non-struct type {:?}", struct_ty),
    };
    let decl_fields = structs.get(name).cloned().unwrap_or_default();
    let mut offset = 0;
    for (fname, fty) in &decl_fields {
        let leaf_count = struct_fields(fty, structs).len();
        if fname == field {
            return (offset, leaf_count);
        }
        offset += leaf_count;
    }
    panic!("field '{}' not found on struct {} in codegen", field, name);
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
    let elem_leafs = struct_fields(&elem_ty, ctx.structs);

    let len_id = ctx.func_ids["frog_list_len"];
    let len_callee = ctx.module.declare_func_in_func(len_id, bcx.func);
    let len_call = bcx.ins().call(len_callee, &[list_val]);
    let len_val = bcx.inst_results(len_call)[0];
    let stride_val = list_stride(bcx, list_val);

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

    // Read each leaf field of the current element (1 call for a scalar
    // element, one per leaf for a struct element) and bind `var`'s
    // corresponding `Variable`(s) — mirrors `Assign`'s multi-leaf binding.
    // `i` is an element index the loop header has already bounded by
    // `frog_list_len`, so — unlike a user-written `xs[i]`, which still goes
    // through `frog_list_get` for negative-index and range handling — the
    // read needs no bounds check, just the slot arithmetic `frog_list_get`
    // would have done: `i * stride + leaf_idx`.
    let base_slot = bcx.ins().imul(i, stride_val);
    for (leaf_idx, (leaf_path, lty)) in elem_leafs.iter().enumerate() {
        let slot = bcx.ins().iadd_imm(base_slot, leaf_idx as i64);
        let addr = list_slot_addr(bcx, list_val, slot);
        let raw = bcx.ins().load(types::I64, heap_mem(), addr, 0);
        let elem_val = from_i64_repr(bcx, lty, raw);
        if is_heap_ty(lty) {
            root_heap_value(bcx, ctx, elem_val);
        }
        let key = var_key(var, leaf_path);
        let var_id = get_or_declare_var(bcx, vars, ctx, &key, lty);
        bcx.def_var(var_id, elem_val);
    }

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

    let body_vals = compile_expr_multi(body, bcx, vars, ctx);
    if let Some(list_ptr) = result_list {
        let body_leafs = struct_fields(&body.item.ty, ctx.structs);
        for (v, (_, lty)) in body_vals.iter().zip(body_leafs.iter()) {
            let pushed = to_i64_repr(bcx, lty, *v);
            emit_list_push(bcx, ctx, list_ptr, pushed);
        }
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
        builder.symbol("frog_str_repr_print", ffi::frog_str_repr_print as *const u8);
        builder.symbol("frog_bytes_print", ffi::frog_bytes_print as *const u8);
        builder.symbol("frog_str_println", ffi::frog_str_println as *const u8);
        builder.symbol("frog_int_println", ffi::frog_int_println as *const u8);
        builder.symbol("frog_float_println", ffi::frog_float_println as *const u8);
        builder.symbol("frog_bool_println", ffi::frog_bool_println as *const u8);
        builder.symbol("frog_int_print", ffi::frog_int_print as *const u8);
        builder.symbol("frog_float_print", ffi::frog_float_print as *const u8);
        builder.symbol("frog_bool_print", ffi::frog_bool_print as *const u8);
        builder.symbol("frog_list_print", ffi::frog_list_print as *const u8);
        builder.symbol("frog_list_println", ffi::frog_list_println as *const u8);
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
        builder.symbol("frog_alloc_variant", ffi::frog_alloc_variant as *const u8);
        builder.symbol("frog_variant_tag", ffi::frog_variant_tag as *const u8);
        builder.symbol("frog_variant_get", ffi::frog_variant_get as *const u8);
        builder.symbol("frog_variant_set", ffi::frog_variant_set as *const u8);

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
        declare_rt(&mut module, &mut func_ids, "frog_str_repr_print", "frog_str_repr_print", &[I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_bytes_print", "frog_bytes_print", &[I64, I64], None);
        // "print" in froglang calls frog_str_println (with newline).
        declare_rt(&mut module, &mut func_ids, "frog_str_println","print",           &[I64],           None);
        // `panic`'s own message-printing reuses the same runtime entry
        // point as `print(a_str_value)` — see `default_context`'s
        // registration and the generic `Call` codegen's `Type::Never`
        // handling, which is what actually makes the call diverge (a trap
        // after it returns).
        declare_rt(&mut module, &mut func_ids, "frog_str_println","panic",           &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_int_println", "frog_int_println", &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_float_println", "frog_float_println", &[types::F64], None);
        declare_rt(&mut module, &mut func_ids, "frog_bool_println", "frog_bool_println", &[types::I8],  None);
        declare_rt(&mut module, &mut func_ids, "frog_int_print", "frog_int_print", &[I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_float_print", "frog_float_print", &[types::F64], None);
        declare_rt(&mut module, &mut func_ids, "frog_bool_print", "frog_bool_print", &[types::I8], None);
        declare_rt(&mut module, &mut func_ids, "frog_list_print", "frog_list_print", &[I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_list_println", "frog_list_println", &[I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_alloc_list", "frog_alloc_list", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_len",   "frog_list_len",   &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_get",   "frog_list_get",   &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_set",   "frog_list_set",   &[I64, I64, I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_list_push",  "frog_list_push",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_slice", "frog_list_slice", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_range",      "frog_range",      &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_gc_dump",    "gc_dump",         &[],               None);
        declare_rt(&mut module, &mut func_ids, "frog_frame_push", "frog_frame_push", &[I64, I64],      None);
        declare_rt(&mut module, &mut func_ids, "frog_frame_pop",  "frog_frame_pop",  &[],              None);
        declare_rt(&mut module, &mut func_ids, "frog_alloc_variant", "frog_alloc_variant", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_variant_tag", "frog_variant_tag", &[I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_variant_get", "frog_variant_get", &[I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_variant_set", "frog_variant_set", &[I64, I64, I64], None);

        Codegen {
            module,
            func_ids,
            builder_ctx: FunctionBuilderContext::new(),
        }
    }

    /// A struct-typed param or return value expands to one `AbiParam` per
    /// flattened leaf field (`struct_fields`), in declared-field order —
    /// Cranelift signatures natively support multiple params/returns, so
    /// this is a direct extension of the pre-struct one-param-per-value
    /// signature shape (every non-struct type still contributes exactly one).
    fn make_sig(&self, params: &[(String, Type)], return_type: &Type, structs: &StructDefs) -> cranelift_codegen::ir::Signature {
        let mut sig = self.module.make_signature();
        for (_, ty) in params {
            for (_, lty) in struct_fields(ty, structs) {
                sig.params.push(AbiParam::new(cl_type(&lty)));
            }
        }
        if *return_type != Type::None {
            for (_, lty) in struct_fields(return_type, structs) {
                sig.returns.push(AbiParam::new(cl_type(&lty)));
            }
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
        structs: &StructDefs,
        unions: &UnionDefs,
    ) {
        let mut bcx = FunctionBuilder::new(&mut cl_ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);

        let mut vars: HashMap<String, Variable> = HashMap::new();
        let mut var_counter: u32 = 0;
        let entry_params: Vec<Value> = bcx.block_params(entry).to_vec();
        // A struct-typed param consumes as many consecutive entry params as
        // it has flattened leaf fields — `make_sig` laid these out in the
        // exact same per-param `struct_fields` order.
        let mut cursor = 0usize;
        for (name, ty) in params {
            for (path, lty) in struct_fields(ty, structs) {
                let key = var_key(name, &path);
                declare_and_def_var(&mut bcx, &mut vars, &mut var_counter, &key, &lty, entry_params[cursor]);
                cursor += 1;
            }
        }

        let n = count_heap_slots(body, structs);
        let heap_slot = setup_shadow_frame(&mut bcx, module, func_ids, n);
        let mut ctx = Ctx { func_ids, module, string_arena, heap_slot, heap_cursor: 0, heap_max: n, var_counter, structs, unions };
        let results = compile_expr_multi(body, &mut bcx, &mut vars, &mut ctx);
        teardown_shadow_frame(&mut bcx, module, func_ids, heap_slot);

        if *return_type != Type::None {
            // If the body's own type is `Never`, it already returned
            // unconditionally (see `TypedExprKind::Return`'s codegen), and
            // `results` is the empty `Vec` that arm produces — we're now
            // positioned in the dead block it switched to. Cranelift still
            // verifies that block's own terminator against the function
            // signature even though nothing ever reaches it at runtime, so
            // it needs a value list of the right shape; the values
            // themselves are never observed.
            let results = if body.item.ty == Type::Never {
                struct_fields(return_type, structs).iter()
                    .map(|(_, t)| placeholder_value(&mut bcx, cl_type(t)))
                    .collect()
            } else {
                results
            };
            bcx.ins().return_(&results);
        } else {
            bcx.ins().return_(&[]);
        }

        bcx.seal_all_blocks();
        bcx.finalize();
    }

    /// Build the `__frog_main[_N]` body. The function takes one `i64` pointer
    /// parameter (`out_ptr`, unused if there are no top-level bindings) and
    /// writes each top-level `let`/`func`-free `Assign`'s value into
    /// `out_ptr`, back-to-back in source order — this is how the caller
    /// (`FrogState::eval`) learns the values of *every* binding made in this
    /// entry, not just the last one. A binding's width in `i64` slots is
    /// `struct_fields(ty, structs).len()` — 1 for every non-struct type, so
    /// nothing changes there; a struct-typed binding writes all of its
    /// flattened leaf values, not just a single slot. Returns the ordered
    /// `(name, type)` list so the caller can recompute each binding's slot
    /// range the same way and decode `out_ptr`'s contents.
    ///
    /// **Struct scope note**: only *named top-level bindings* (this
    /// function's `pre_env`/`out_ptr` protocol) and struct-typed
    /// params/returns/locals/list-elements round-trip correctly. The
    /// entry's own bare *final result* (`__frog_main`'s single-`i64` return
    /// value, decoded by `FrogValue::from_bits`) still only reports a
    /// struct's first flattened leaf — that's a separate, narrower gap
    /// (the JIT ABI's single scalar return, not the persistence layer) left
    /// as future work. Ending an entry with `some_struct` bare will show a
    /// truncated result in the REPL; `let x = some_struct` (then referring
    /// to `x`) is unaffected and round-trips fully.
    fn build_main_body(
        builder_ctx: &mut FunctionBuilderContext,
        cl_ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        stmts: &[Spanned<TypedExpr>],
        string_arena: &mut Vec<Vec<u8>>,
        pre_env: &HashMap<String, Vec<i64>>,
        env_types: &HashMap<String, Type>,
        structs: &StructDefs,
        unions: &UnionDefs,
    ) -> Vec<(String, Type)> {
        let mut bcx = FunctionBuilder::new(&mut cl_ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let out_ptr = bcx.block_params(entry)[0];

        let mut vars: HashMap<String, Variable> = HashMap::new();
        let mut var_counter: u32 = 0;

        // Pre-seed vars from prior REPL entries as iconst values — one per
        // flattened leaf field, matching how `FrogState::eval` decoded them.
        for (name, bits) in pre_env {
            let ty = env_types.get(name).unwrap_or(&Type::Int);
            let leafs = struct_fields(ty, structs);
            for ((path, lty), &leaf_bits) in leafs.iter().zip(bits.iter()) {
                let val = match lty {
                    Type::Float => bcx.ins().f64const(f64::from_bits(leaf_bits as u64)),
                    Type::Bool  => bcx.ins().iconst(types::I8, leaf_bits),
                    _           => bcx.ins().iconst(types::I64, leaf_bits),
                };
                let key = var_key(name, path);
                declare_and_def_var(&mut bcx, &mut vars, &mut var_counter, &key, lty, val);
            }
        }
        let mut last_val = bcx.ins().iconst(types::I64, 0);
        let mut last_ty = &Type::Int;

        let n: usize = stmts.iter().map(|s| count_heap_slots(s, structs)).sum();
        let heap_slot = setup_shadow_frame(&mut bcx, module, func_ids, n);
        let mut ctx = Ctx { func_ids, module, string_arena, heap_slot, heap_cursor: 0, heap_max: n, var_counter, structs, unions };

        let mut bindings: Vec<(String, Type)> = Vec::new();
        let mut slot_cursor: usize = 0;

        for stmt in stmts {
            if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                    continue;
                }
                let vals = compile_expr_multi(stmt, &mut bcx, &mut vars, &mut ctx);
                last_val = vals[0];
                // Use `value.item.ty`, not `stmt.item.ty` (the Assign
                // expression's own — possibly annotation-widened — type):
                // `vals` was produced by flattening `value`, so its length
                // is exactly `struct_fields(value.item.ty).len()`. Zipping
                // against leafs derived from a different type could silently
                // truncate the store loop below and desync every later
                // binding's `out_ptr` slot from what `FrogState::eval` (the
                // read side, `state.rs`) expects.
                last_ty = &value.item.ty;
                let leafs = struct_fields(last_ty, structs);
                assert_eq!(vals.len(), leafs.len(), "compile_expr_multi produced {} values for {} leaf fields", vals.len(), leafs.len());
                for (v, (_, lty)) in vals.iter().zip(leafs.iter()) {
                    let repr = to_i64_repr(&mut bcx, lty, *v);
                    let offset = (slot_cursor * 8) as i32;
                    bcx.ins().store(MemFlags::new(), repr, out_ptr, offset);
                    slot_cursor += 1;
                }
                bindings.push((name.clone(), last_ty.clone()));
                continue;
            }
            let vals = compile_expr_multi(stmt, &mut bcx, &mut vars, &mut ctx);
            last_val = vals[0];
            last_ty = &stmt.item.ty;
        }

        teardown_shadow_frame(&mut bcx, module, func_ids, heap_slot);

        // __frog_main[_N] always returns a single i64 (see this function's
        // doc comment — a struct-typed final result only reports its first
        // leaf here).
        let leaf0_ty = struct_fields(last_ty, structs).into_iter().next().map(|(_, t)| t).unwrap_or(Type::Int);
        let last_val = to_i64_repr(&mut bcx, &leaf0_ty, last_val);

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
        pre_env: &HashMap<String, Vec<i64>>,
        env_types: &HashMap<String, Type>,
        structs: &StructDefs,
        unions: &UnionDefs,
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
                    let sig = self.make_sig(params, return_type, structs);
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
            let sig = self.make_sig(params, &return_type, structs);
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
                structs,
                unions,
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
            structs,
            unions,
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
        typed, &mut string_arena, 0, &HashMap::new(), &HashMap::new(), tc.struct_defs(), tc.union_defs(),
    );

    let ptr = codegen.module.get_finalized_function(main_id);
    let f: fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };
    // Each binding occupies `struct_fields(ty, structs).len()` i64 slots in
    // `out_ptr` (1 for every non-struct type) — see `build_main_body`'s doc
    // comment for the write side of this protocol.
    let total_slots: usize = bindings.iter()
        .map(|(_, ty)| struct_fields(ty, tc.struct_defs()).len())
        .sum();
    let mut out_buf: Vec<i64> = vec![0i64; total_slots];
    f(out_buf.as_mut_ptr() as i64)
    // string_arena and out_buf dropped here, after f() returns
}
