use std::collections::HashMap;
use std::mem::{offset_of, size_of};

use cranelift_codegen::ir::{condcodes::{FloatCC, IntCC}, types, AbiParam, BlockArg, InstBuilder, MachMemFlags, StackSlotData, StackSlotKind, TrapCode, Value};
use cranelift_codegen::{settings, settings::Configurable, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::frontend::liveness;
use crate::frontend::tokens::{Span, Spanned, Token};
use crate::frontend::typed_ast::{Arg, Place, PlaceSeg, TypedExpr, TypedExprKind, TypedExprRef};
use crate::frontend::typeck::{UnionDef, UnionDefs, StructDefs, Type, numeric_join, is_positional_fields};
use crate::runtime::{ffi, gc};
use crate::runtime::gc::{FrogList, FrogVariant};

/// One entry per top-level `func`/lambda declared by `compile_entry`'s Pass
/// 1 — `plans/DATA.md` Stage 3's "fn-ptr → span" table. Keyed by declaration
/// order, not by address: nothing here needs a binary search today, since
/// the only consumer so far is `FrogState::source_map`, a linear lookup by
/// name or entry. `entry_id` is the same number `compile_entry` mangles into
/// the JIT symbol (`{name}__frogfn{entry_id}`) — pair it with
/// `FrogState::entry_sources` to recover the actual source text and
/// filename this declaration came from.
#[derive(Debug, Clone)]
pub struct FnSourceInfo {
    pub name: String,
    pub span: Span,
    pub entry_id: usize,
}

pub struct Codegen {
    pub module: JITModule,
    func_ids: HashMap<String, FuncId>,
    builder_ctx: FunctionBuilderContext,
    /// Names registered via `new_with_hosts` — every one of them also has a
    /// `func_ids` entry, like any other callable, but `compile_call` needs
    /// to know *which* names go through the uniform `(ctx, args, out)` shim
    /// calling convention instead of a plain Cranelift call. See
    /// `plans/EMBEDDING.md`.
    host_fns: std::collections::HashSet<String>,
    /// `plans/DATA.md` Stage 3's source map — one entry per top-level
    /// `func`/lambda ever declared, across every entry. Append-only within
    /// an entry; a failed entry truncates back via `restore_source_map`,
    /// mirroring `func_ids`' own checkpoint/restore pair.
    source_map: Vec<FnSourceInfo>,
}

/// Per-function-compilation context threaded through `compile_expr`.
///
/// GC roots are Cranelift's business now, not this module's: every value
/// whose static type is a GC-scannable column (`is_heap_ty`) is handed to
/// `declare_gc_value`/`declare_gc_var`, Cranelift computes which of them are
/// live at each safepoint, and the collector reads them off the native stack
/// (`gc.rs`, "Precise roots"). What used to live here — a shadow-frame stack
/// slot, a bump cursor into it, and a predicted slot count that had to stay
/// in exact lockstep with the codegen walk — is gone along with the two
/// use-after-frees that lockstep requirement produced. See RUNTIME.md Part 2.
struct Ctx<'a> {
    func_ids:      &'a HashMap<String, FuncId>,
    module:        &'a mut JITModule,
    string_arena:  &'a mut Vec<Vec<u8>>,
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
    /// Anonymous-union "shapes" (keyed by their `Debug`-formatted member
    /// list — stable identity for the same union type) currently being
    /// printed, innermost call last — see `print_union`. A union-typed
    /// struct field is stored as a single opaque boxed pointer, never
    /// flattened (that's what makes a self-referential type like
    /// `data Node is Add(lhs: Node, ...) | ...` representable at all), so
    /// printing one recursively re-derives the same union type at codegen
    /// time with no static bound on depth — `print_union` uses this stack
    /// to detect that recursion and fail clearly instead of emitting an
    /// unbounded branch tree.
    printing_unions: Vec<String>,
    /// The same stack for `eq_union`, which recurses through a union member's
    /// fields exactly as printing does and hits the same wall on a
    /// self-referential type. Kept separate from `printing_unions` because
    /// the two walks nest independently — a comparison inside a `print`
    /// argument is not a recursion.
    comparing_unions: Vec<String>,
    /// This function's own `mut` parameters (name, type), in declaration
    /// order — empty for `build_main_body`'s entry function, which never
    /// has parameters. Consulted by every `return_`-emitting site
    /// (`build_func_body`'s tail, and `compile_return`'s early exit) via
    /// `mut_param_copyout` to append each one's final value after the
    /// ordinary return — see `TypedExprKind::Function`'s doc comment.
    mut_params:    Vec<(String, Type)>,
    /// This function/entry's move-vs-copy analysis (`liveness::analyze_body`/
    /// `analyze_entry`), consulted by `compile_expr_multi`'s `TypedExprKind::Var`
    /// arm: a `Copy`-classified read of a `List` binding is marked shared
    /// (`mark_shared_if_aliased`), so a later write through any path to it
    /// copies first — see MUTABILITY.md Stage 7 and RUNTIME.md. `Move` means
    /// this is the name's last use, so the value is being transferred rather
    /// than duplicated and nothing needs marking.
    liveness:      liveness::Liveness,
    /// See `Codegen::host_fns`.
    host_fns:      &'a std::collections::HashSet<String>,
    /// `FROG_COW_VERIFY` — emit a `frog_cow_verify` call on the write
    /// barrier's unshared path, checking against the heap that nothing else
    /// can actually reach the object. Read once per `Codegen`, so an
    /// ordinary build emits no call at all rather than one that returns
    /// early.
    cow_verify:    bool,
}

/// True iff a slot of this type is a GC-scannable column — a word the
/// collector reads and hands to `gc::is_heap_ptr`.
///
/// `Type::Union` is included for both shapes: a boxed union's word is a
/// `FrogVariant` pointer or an immediate, and an inline union's *pointer*
/// columns are exactly what `struct_fields` labels with the union type (its
/// scalar columns are labelled `Type::Int` and are deliberately excluded —
/// a raw `Int` carries no tag bits and must never be scanned).
pub fn is_heap_ty(ty: &Type) -> bool {
    matches!(ty, Type::Str | Type::Union(_)) || ty.is_list()
}

/// The largest number of members a union can have and still be laid out
/// inline. Member tags occupy the low 3 bits of the tag word (see gc.rs's
/// "Word encoding"), where `0` is reserved for a plain untagged pointer and
/// `7` for immediates — leaving `1..=6`. A wider union falls back to the
/// boxed one-slot representation.
pub const MAX_INLINE_UNION_MEMBERS: usize = 6;

/// The runtime tag for member `index` of an inline union's *normalized*
/// member list. `Type::normalize` flattens, dedups and sorts, so this is
/// stable for a given type regardless of how it was spelled.
pub(crate) fn member_tag(index: usize) -> u32 {
    (index + 1) as u32
}

/// True iff the union `members` is laid out inline (flattened into
/// `UnionLayout`'s slots) rather than boxed into a one-slot `FrogVariant`.
///
/// Boxing is forced by two things, both static properties of the type
/// alone — representation must never depend on *where* a value sits, or
/// `let d: Discount = order.discount` would need a conversion at every
/// field read:
///
///   * more than `MAX_INLINE_UNION_MEMBERS` members — there is no tag left
///     to give them;
///   * self-reference. `data Tree is Leaf | Node(v: Int, l: Tree, r: Tree)`
///     cannot be flattened into a finite slot count. RUNTIME.md sketches
///     unboxing the node while leaving its children boxed; that needs a
///     box/unbox conversion at every field boundary, so this implementation
///     boxes the whole type instead — exactly today's behaviour for such a
///     type, i.e. conservative, not a regression.
pub fn union_is_inline(members: &[Type], structs: &StructDefs) -> bool {
    members.len() <= MAX_INLINE_UNION_MEMBERS && !union_is_recursive(members, structs)
}

/// Is this union reachable from itself by following member types and their
/// struct fields? Only *inline* containment counts: a `List(Tree)` field is
/// a plain pointer, so it breaks the cycle, and so does a nested union that
/// is itself boxed. Struct cycles that never pass through a union are
/// already rejected by `hoist_data_decls`, so the `seen` set here is a
/// belt-and-braces terminator rather than the thing doing the work.
fn union_is_recursive(members: &[Type], structs: &StructDefs) -> bool {
    fn reaches(target: &[Type], ty: &Type, structs: &StructDefs, seen: &mut Vec<Type>) -> bool {
        // `as_struct_name` returns `None` for `List` (it excludes it by
        // construction), so a `List(Tree)` field is never recursed into —
        // it's a pointer regardless of element type, so it breaks the
        // cycle, same as before `Type::Named` collapsed `List`/`Struct`.
        if ty.as_struct_name().is_some() {
            // Cycle detection is over the *type*, not the bare name: two
            // instantiations of one generic struct share a name but are
            // different layouts, and treating the second as "already seen"
            // would cut the walk short.
            if seen.iter().any(|s| s == ty) { return false; }
            seen.push(ty.clone());
            let hit = structs.get(ty).is_some_and(|fields| {
                fields.iter().any(|(_, f)| reaches(target, f, structs, seen))
            });
            seen.pop();
            return hit;
        }
        match ty {
            Type::Union(ms) => {
                ms.as_slice() == target
                    || ms.iter().any(|m| reaches(target, m, structs, seen))
            },
            _ => false,
        }
    }
    members.iter().any(|m| reaches(members, m, structs, &mut Vec::new()))
}

/// True iff a word of this leaf type carries no tag bits of its own, so an
/// enclosing union may overlay its own member tag onto the same word.
///
/// A `Str`/`List` pointer is 8-byte aligned with three spare low bits. A
/// union leaf is not: an inline union's slot 0 already holds *its* tag, and
/// a boxed union's word may be an immediate (`(t << 3) | 7`). Overlaying a
/// second tag on either corrupts it — RUNTIME.md's second open question.
fn overlay_safe(leaf_ty: &Type) -> bool {
    matches!(leaf_ty, Type::Str) || leaf_ty.is_list()
}

/// Slot layout of an inline union (`union_is_inline`).
///
/// Slot 0 always holds the member tag. Every member's fields are flattened
/// (`struct_fields`) and partitioned by static pointer-ness — that partition
/// is the point of the whole scheme, since it is what makes each slot's
/// pointer-ness a property of the *column* rather than of the value in it.
/// The columns are, in order:
///
///   [ tag+ptr_0 | ptr_1 .. ptr_{P-1} | scalar_0 .. scalar_{S-1} ]
///
/// with `P = max over members(#pointer leaves)` and `S = max over
/// members(#scalar leaves)`. The tag rides in the low 3 bits of pointer slot
/// 0 (a member with no pointer leaves leaves the rest of that word zero, so
/// its slot 0 is just the small tag `1..=6`, which masks to `0` and is
/// correctly not followed).
///
/// When some member's first pointer leaf is itself tagged (`overlay_safe`
/// is false for it), the tag cannot share that word and gets a column of its
/// own instead:
///
///   [ tag | ptr_0 .. ptr_{P-1} | scalar_0 .. scalar_{S-1} ]
///
/// costing one extra slot. Either way the tag is slot 0 and is read with
/// `w & 7`, and slots `0..ptr_end` are exactly the GC-scannable columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnionLayout {
    /// Number of pointer columns.
    pub ptrs: usize,
    /// The widest member's pointer-leaf count — `ptrs` before the forced
    /// minimum of one that hosting the tag imposes. `0` means *no* member
    /// ever puts a pointer in this union at all, so its tag column holds
    /// nothing but a small tag: not scannable, and not worth spilling
    /// around safepoints. `data Discount is NoDiscount | Percent(pct: Int)
    /// | ...` is the case that matters — it is a union by type and pure
    /// scalars by content.
    pub ptrs_used: usize,
    /// Number of scalar columns.
    pub scalars: usize,
    /// True when the tag has a column to itself (see above).
    pub dedicated_tag: bool,
}

impl UnionLayout {
    /// Slot index of this member's pointer leaf `i`.
    pub fn ptr_slot(&self, i: usize) -> usize {
        if self.dedicated_tag { 1 + i } else { i }
    }
    /// Slot index of this member's scalar leaf `i`.
    pub fn scalar_slot(&self, i: usize) -> usize {
        self.ptr_end() + i
    }
    /// One past the last GC-scannable slot. Slot 0 is included even when it
    /// is a dedicated tag: a bare tag `1..=6` masks to zero, so scanning it
    /// is harmless and keeps the mask contiguous.
    pub fn ptr_end(&self) -> usize {
        if self.dedicated_tag { 1 + self.ptrs } else { self.ptrs }
    }
    pub fn width(&self) -> usize {
        self.ptr_end() + self.scalars
    }
    /// True iff the tag shares slot 0 with a pointer, so reading that
    /// pointer back out needs the tag bits masked off.
    pub fn tag_shares_slot0(&self) -> bool {
        !self.dedicated_tag
    }
    /// Can slot `i` ever hold a heap pointer? False for the scalar columns,
    /// and false for the tag column whenever the tag has it to itself — a
    /// word that only ever holds `1..=6` is not worth scanning, and (much
    /// more expensively) not worth spilling and reloading around every
    /// safepoint the way a real root is.
    pub fn slot_is_scannable(&self, i: usize) -> bool {
        if i >= self.ptr_end() { return false; }
        if self.dedicated_tag { i >= 1 } else { self.ptrs_used > 0 }
    }
}

/// The flattened leaves one union member contributes. `Type::None` (and
/// `Type::Never`, which cannot occur in a value) carry nothing at all — a
/// payload-less member *is* its tag.
fn member_leaf_types(member: &Type, structs: &StructDefs) -> Vec<Type> {
    if matches!(member, Type::None | Type::Never) {
        return Vec::new();
    }
    struct_fields(member, structs).into_iter().map(|(_, t)| t).collect()
}

/// Partition one member's leaves into `(pointer leaves, scalar leaves)`,
/// each as `(leaf_index, leaf_type)` so a caller can put them back in
/// declaration order.
fn partition_member_leaves(member: &Type, structs: &StructDefs) -> (Vec<(usize, Type)>, Vec<(usize, Type)>) {
    let mut ptrs = Vec::new();
    let mut scalars = Vec::new();
    for (i, t) in member_leaf_types(member, structs).into_iter().enumerate() {
        if is_heap_ty(&t) { ptrs.push((i, t)); } else { scalars.push((i, t)); }
    }
    (ptrs, scalars)
}

/// Compute `members`'s inline layout. Only valid when `union_is_inline`.
pub fn union_layout(members: &[Type], structs: &StructDefs) -> UnionLayout {
    let mut ptrs = 0usize;
    let mut scalars = 0usize;
    let mut dedicated_tag = false;
    for m in members {
        let (p, s) = partition_member_leaves(m, structs);
        ptrs = ptrs.max(p.len());
        scalars = scalars.max(s.len());
        if let Some((_, first)) = p.first() {
            if !overlay_safe(first) { dedicated_tag = true; }
        }
    }
    let ptrs_used = ptrs;
    if !dedicated_tag {
        // Slot 0 doubles as pointer column 0, so there is always at least
        // one pointer column to host the tag.
        ptrs = ptrs.max(1);
    }
    UnionLayout { ptrs, ptrs_used, scalars, dedicated_tag }
}

/// For each of `member`'s leaves, in declaration order, the slot it occupies
/// in the enclosing inline union — plus whether that slot's low bits also
/// hold the tag (so a reader must mask, and a writer must `bor` the tag in).
pub(crate) fn member_slot_map(member: &Type, layout: &UnionLayout, structs: &StructDefs) -> Vec<(usize, bool)> {
    let leaves = member_leaf_types(member, structs);
    let mut out = vec![(0usize, false); leaves.len()];
    let (mut p, mut s) = (0usize, 0usize);
    for (i, t) in leaves.iter().enumerate() {
        if is_heap_ty(t) {
            let slot = layout.ptr_slot(p);
            out[i] = (slot, slot == 0 && layout.tag_shares_slot0());
            p += 1;
        } else {
            out[i] = (layout.scalar_slot(s), false);
            s += 1;
        }
    }
    out
}

/// Pack one member's flattened leaf values into an inline union's slots.
///
/// `leaf_vals` are `member_leaf_types(member_ty)`'s values in declaration
/// order (empty for a payload-less member — a payload-less member *is* its
/// tag). Every slot the member does not occupy is written as `0`, which
/// fails `gc::is_heap_ptr` and so is always safe for the collector to see.
///
/// No rooting happens here and none is needed: a pointer column holds a
/// value that was already rooted at its own producer site, and OR-ing the
/// tag into its spare low bits does not change which object it names.
fn pack_union_member(
    members: &[Type],
    member_ty: &Type,
    tag: u32,
    leaf_vals: &[Value],
    bcx: &mut FunctionBuilder,
    structs: &StructDefs,
) -> Vec<Value> {
    let layout = union_layout(members, structs);
    let leaf_tys = member_leaf_types(member_ty, structs);
    debug_assert_eq!(
        leaf_tys.len(), leaf_vals.len(),
        "union member {} contributes {} leaves but {} values were supplied",
        member_ty, leaf_tys.len(), leaf_vals.len(),
    );
    let map = member_slot_map(member_ty, &layout, structs);

    let mut slots: Vec<Option<Value>> = vec![None; layout.width()];
    for (i, (slot, shares_tag)) in map.iter().enumerate() {
        let mut w = to_i64_repr(bcx, &leaf_tys[i], leaf_vals[i]);
        if *shares_tag {
            // OR-ing the tag in produces a *new* SSA value that is still a
            // pointer to the same object, and the un-tagged one may die
            // immediately — so the tagged word needs declaring in its own
            // right. `gc::is_heap_ptr` masks the tag off, so the collector
            // reads it correctly.
            w = bcx.ins().bor_imm_s(w, tag as i64);
            declare_gc_ptr(bcx, w);
        }
        slots[*slot] = Some(w);
    }
    // Slot 0 always carries the tag. It is already written when this
    // member's first pointer leaf landed there; otherwise the tag stands
    // alone, and `1..=6` masks to `0` so the collector leaves it be.
    if slots[0].is_none() {
        slots[0] = Some(bcx.ins().iconst(types::I64, tag as i64));
    }
    let zero = bcx.ins().iconst(types::I64, 0);
    slots.into_iter().map(|o| o.unwrap_or(zero)).collect()
}

/// Runtime analog of `pack_union_member`, for building an inline union's
/// slots directly from Rust (`host.rs`'s `ToFrog for Result<T, E>`) rather
/// than emitting Cranelift IR. `leaf_vals` are already in wire format —
/// unlike `pack_union_member`'s `Value`s, there is no `to_i64_repr`
/// conversion to do here, only slot placement and tag OR-ing.
pub(crate) fn pack_union_member_runtime(
    members: &[Type],
    member_ty: &Type,
    tag: u32,
    leaf_vals: &[i64],
    structs: &StructDefs,
) -> Vec<i64> {
    let layout = union_layout(members, structs);
    let map = member_slot_map(member_ty, &layout, structs);
    debug_assert_eq!(
        map.len(), leaf_vals.len(),
        "union member {} contributes {} leaves but {} values were supplied",
        member_ty, map.len(), leaf_vals.len(),
    );
    let mut slots: Vec<Option<i64>> = vec![None; layout.width()];
    for (i, (slot, shares_tag)) in map.iter().enumerate() {
        let mut w = leaf_vals[i];
        if *shares_tag {
            w |= tag as i64;
        }
        slots[*slot] = Some(w);
    }
    if slots[0].is_none() {
        slots[0] = Some(tag as i64);
    }
    slots.into_iter().map(|o| o.unwrap_or(0)).collect()
}

/// The inverse of `pack_union_member`: recover `member_ty`'s flattened leaf
/// values from an inline union's `slots`, in declaration order.
///
/// The caller must already know — from a preceding tag test — that `slots`
/// really holds this member. Nothing is rooted: whatever a pointer column
/// names stays reachable through the union's own root for as long as the
/// union does, and masking the tag off does not change which object that is.
fn unpack_union_member(
    members: &[Type],
    member_ty: &Type,
    slots: &[Value],
    bcx: &mut FunctionBuilder,
    structs: &StructDefs,
) -> Vec<Value> {
    let layout = union_layout(members, structs);
    let leaf_tys = member_leaf_types(member_ty, structs);
    let map = member_slot_map(member_ty, &layout, structs);
    let mut out = Vec::with_capacity(leaf_tys.len());
    for (i, (slot, shares_tag)) in map.iter().enumerate() {
        let mut w = slots[*slot];
        if *shares_tag {
            // Masking the tag off produces a new SSA value naming the same
            // object; the tagged word it came from may die immediately, so
            // this one is declared too (see `pack_union_member`).
            w = bcx.ins().band_imm_s(w, !gc::TAG_MASK);
            declare_gc_ptr(bcx, w);
        }
        out.push(from_i64_repr(bcx, &leaf_tys[i], w));
    }
    out
}

/// Emit the runtime test "this inline union currently holds the member at
/// normalized index `index`" — one `and` and one compare against a constant,
/// with no load and no branch on representation.
fn emit_inline_tag_test(bcx: &mut FunctionBuilder, slots: &[Value], index: usize) -> Value {
    let tag_bits = bcx.ins().band_imm_s(slots[0], gc::TAG_MASK);
    bcx.ins().icmp_imm_s(IntCC::Equal, tag_bits, member_tag(index) as i64)
}

/// The normalized-member index of nominal union `enum_name`'s variant
/// `variant`.
///
/// A nominal union's declaration order and its `Type::Union`'s member order
/// are different things — `Type::normalize` sorts members by display string
/// — and the typed AST carries the *declaration* index (`VariantInit.tag`,
/// `IsVariant.tag`). Inline layout keys the runtime tag and the slot map off
/// the normalized position, so every nominal-union site converts here.
fn nominal_member_index(members: &[Type], enum_name: &str, variant: &str) -> usize {
    let want = Type::strukt(format!("{}.{}", enum_name, variant));
    members.iter().position(|m| *m == want).unwrap_or_else(|| {
        panic!("variant {}.{} is not a member of its own union's normalized member list", enum_name, variant)
    })
}

/// True iff a value of this type crosses an ABI boundary or a `Conditional`
/// merge block as more than one flat Cranelift value (see `struct_fields`):
/// a struct, or an inline union.
fn is_multi_leaf_type(ty: &Type, structs: &StructDefs) -> bool {
    if ty.is_struct() { return true; }
    match ty {
        Type::Union(members) => union_is_inline(members, structs),
        _ => false,
    }
}

/// Compute the GC scan mask for a flattened leaf list (`struct_fields`'s
/// output) about to be embedded in a GC-scanned aggregate (a boxed
/// `FrogVariant`'s payload, or a `List`'s element stride).
///
/// Bit `i` set means "the collector reads slot `i` and applies the uniform
/// `gc::is_heap_ptr`/`gc::heap_ptr` rule". Every word in the system uses one
/// encoding (gc.rs, "Word encoding"), so there is nothing conditional left to
/// express — this replaces the old `(ptr_mask, cond_mask, boxed_tags)` triple
/// and the one-scalar-union-shape-per-aggregate restriction that came with
/// it. A scalar column is *not* marked: a raw `Int` carries no tag bits and
/// could otherwise be mistaken for an address.
fn gc_mask<'a>(leafs: impl IntoIterator<Item = &'a Type>) -> i64 {
    let mut mask: i64 = 0;
    for (i, t) in leafs.into_iter().enumerate() {
        if is_heap_ty(t) { mask |= 1i64 << i; }
    }
    mask
}

/// Recursively flatten `ty` into its ordered leaf `(dotted_path, Type)`
/// list. For any non-struct type, returns a single `("", ty)` pair — the
/// empty path lets `var_key` degrade to exactly today's plain `vars["name"]`
/// scheme for every existing scalar type, so nothing about non-struct
/// codegen changes. For a struct type, recurses into each declared
/// field (in declaration order) so a struct-typed field is expanded inline
/// rather than nested, e.g. `Company{ceo: Person{name, age}}` flattens to
/// `[("ceo.name", Str), ("ceo.age", Int)]`.
pub fn struct_fields(ty: &Type, structs: &StructDefs) -> Vec<(String, Type)> {
    if ty.is_struct() {
        // A generic instantiation's concrete layout (`TRAITS.md` Stage 3a)
        // is registered under this same key — the `Type` itself — by
        // `TypeChecker::materialize_struct`/`instantiate_struct` while
        // typeck runs, so by the time codegen calls this, every
        // instantiation appearing anywhere in the typed program is
        // already present.
        let fields = structs.get(ty).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for (fname, fty) in fields {
            for (sub_path, sub_ty) in struct_fields(&fty, structs) {
                let path = if sub_path.is_empty() { fname.clone() } else { format!("{}.{}", fname, sub_path) };
                out.push((path, sub_ty));
            }
        }
        return out;
    }
    match ty {
        // An inline union (`union_is_inline`) is flattened into its
        // `UnionLayout` columns instead of boxing: pointer columns first —
        // slot 0 carrying the member tag — then scalar columns. Each leaf
        // gets a distinct synthetic path (`$p0`, `$s1`, ...) so `var_key`
        // gives it its own binding, and each carries a leaf *type* that
        // states its column's static pointer-ness: the union type itself
        // for a pointer column (`is_heap_ty` is true for it), plain `Int`
        // for a scalar column. That is the whole trick — every consumer
        // downstream (`gc_mask`, `heap_roots_in_leaves`, `root_flat_leaves`)
        // reads pointer-ness off the column and needs no tag at all.
        Type::Union(members) if union_is_inline(members, structs) => {
            let l = union_layout(members, structs);
            let mut out = Vec::with_capacity(l.width());
            for i in 0..l.ptr_end() {
                // A column no member ever puts a pointer in is labelled a
                // plain `Int`, so nothing downstream scans it, roots it, or
                // spills it — see `UnionLayout::slot_is_scannable`.
                let col = if l.slot_is_scannable(i) { ty.clone() } else { Type::Int };
                out.push((format!("$p{}", i), col));
            }
            for i in 0..l.scalars  { out.push((format!("$s{}", i), Type::Int)); }
            out
        },
        _ => vec![(String::new(), ty.clone())],
    }
}

/// Pick out, from one value's flattened leaves, the raw bits an embedder
/// must hand to `GcHeap::push_root` — i.e. the leaves whose column is
/// GC-scannable.
///
/// `vals` and `leaf_tys` are aligned 1:1, `leaf_tys` being `struct_fields`'s
/// output for the value's type. Under the uniform word encoding (gc.rs) this
/// is a straight filter on the column type: the collector applies
/// `is_heap_ptr` itself, so a tag-only or immediate word roots harmlessly.
/// It used to need the sibling tag word to decide whether a two-slot union's
/// payload was a pointer at all; that whole mechanism is gone.
///
/// `FrogState::eval` uses this to re-derive its explicit root set from
/// `env` after every entry. Before it existed, that code tested for
/// `Str | List` only and silently dropped every union-typed binding, so a
/// `data`-union value bound in one REPL entry was collected out from under
/// the next one.
pub fn heap_roots_in_leaves(vals: &[i64], leaf_tys: &[(String, Type)]) -> Vec<i64> {
    let mut out = Vec::new();
    for (i, (_, ty)) in leaf_tys.iter().enumerate() {
        let Some(&v) = vals.get(i) else { break };
        if is_heap_ty(ty) { out.push(v); }
    }
    out
}

/// Which slots of the `out_ptr` buffer `__frog_main` writes its top-level
/// bindings into are GC-scannable columns.
///
/// `bindings` is the list `compile_entry` returns, laid out back to back,
/// each occupying `struct_fields(ty).len()` slots. Only the scannable
/// columns are listed: the rest hold raw scalars, which carry no tag bits
/// and must never be handed to the collector. See
/// `gc::GcHeap::push_scanned_span` for why the buffer is scanned at all.
pub fn gc_slots_of_bindings(bindings: &[(String, Type)], structs: &StructDefs) -> Vec<usize> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (_, ty) in bindings {
        for (_, leaf_ty) in struct_fields(ty, structs) {
            if is_heap_ty(&leaf_ty) { out.push(cursor); }
            cursor += 1;
        }
    }
    out
}

/// Build the `vars` map key for leaf `leaf_path` (from `struct_fields`) of
/// the binding named `base`. For a scalar binding (`leaf_path == ""`) this
/// is just `base` — identical to every key used before structs existed.
fn var_key(base: &str, leaf_path: &str) -> String {
    if leaf_path.is_empty() { base.to_string() } else { format!("{}.{}", base, leaf_path) }
}

/// Read back the current (possibly rebound, by a `PlaceAssign` or plain
/// reassignment inside the body) value of each of `ctx.mut_params`'
/// flattened `Variable`(s), in parameter order — the copy-out half of a
/// `mut` parameter. Called at every `return_`-emitting site
/// (`build_func_body`'s tail, `compile_return`'s early exit) to append
/// after the ordinary return values, matching the extra `AbiParam`s
/// `make_sig` appends to the signature in the same order.
fn mut_param_copyout(bcx: &mut FunctionBuilder, vars: &HashMap<String, Variable>, ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, ty) in &ctx.mut_params {
        for (path, _) in struct_fields(ty, ctx.structs) {
            let key = var_key(name, &path);
            out.push(bcx.use_var(vars[&key]));
        }
    }
    out
}

/// Tell Cranelift that `val` must appear in stack maps: it is a word the
/// collector will read and hand to `gc::is_heap_ptr`, so it must be spilled
/// to a known offset around every safepoint it is live across.
///
/// The invariant this maintains is simply **every SSA value whose static
/// type is a GC-scannable column is declared**. Cranelift decides the rest:
/// which of them are live where, which spill slot each gets, and which
/// safepoints record which. A value that is genuinely dead after its
/// definition is correctly absent from every map; a value we fail to declare
/// is a missed root, which is why this is stated as a whole-program rule
/// rather than a judgement made site by site.
fn declare_gc_value(bcx: &mut FunctionBuilder, ty: &Type, val: Value) {
    if is_heap_ty(ty) {
        declare_gc_ptr(bcx, val);
    }
}

/// `declare_gc_value` for a value already known to be a GC-visible word —
/// a fresh allocation, or a leaf whose column type the caller has already
/// checked.
fn declare_gc_ptr(bcx: &mut FunctionBuilder, val: Value) {
    bcx.declare_value_needs_stack_map(val);
}

/// `declare_gc_value` for a whole flattened value: `vals` aligned 1:1 with
/// `leaf_tys` (`struct_fields`'s output types).
fn declare_gc_leaves(bcx: &mut FunctionBuilder, vals: &[Value], leaf_tys: &[Type]) {
    for (v, t) in vals.iter().zip(leaf_tys.iter()) {
        declare_gc_value(bcx, t, *v);
    }
}

/// `declare_gc_value` for a `Variable`, which propagates to every value
/// `use_var`/`def_var` ever produces for it *and* to the block parameters
/// Cranelift's SSA construction inserts to route it between blocks — that
/// last part is why froglang needs cranelift >= 0.135, see
/// `tests/test_cranelift_stack_maps.rs`.
///
/// Must be called before the variable's first definition; Cranelift asserts
/// this, since an earlier definition would be silently omitted.
fn declare_gc_var(bcx: &mut FunctionBuilder, ty: &Type, var: Variable) {
    if is_heap_ty(ty) {
        bcx.declare_var_needs_stack_map(var);
    }
}

/// The raw funnel for reading a binding named `name`: one `use_var` per
/// flattened leaf, no cloning. Factored out of `compile_expr_multi`'s `Var`
/// arm so `compile_expr_multi_transient` can call it directly and bypass
/// that arm's cloning — see its doc comment for why.
#[inline]
fn read_var_raw(name: &str, ty: &Type, bcx: &mut FunctionBuilder, vars: &HashMap<String, Variable>, structs: &StructDefs) -> Vec<Value> {
    struct_fields(ty, structs).iter().map(|(path, _)| {
        let key = var_key(name, path);
        let var = *vars.get(&key)
            .unwrap_or_else(|| panic!("unbound variable in codegen: {}", key));
        bcx.use_var(var)
    }).collect()
}

/// MUTABILITY.md Stage 7: mark `vals` (the just-compiled value of `expr`) as
/// aliased if `expr` is a `Var` read of **exactly** `List` (`Type::is_list`)
/// and `Ownership::Copy` (see `Ctx::liveness`). Called from the `Var` arm of
/// `compile_expr_multi` — every *non*-transient consumer of a binding's value
/// (a bind, a call argument, a return, a struct/list/variant literal's field
/// or element, a `Widen`, a `Block`'s tail, a `Conditional` branch...)
/// reaches it that way automatically, since they all read the binding through
/// an ordinary `compile_expr`/`compile_expr_multi` call on a `Var` node.
///
/// **Why `Ownership::Copy` is exactly the right trigger.** `Copy` means the
/// source name is still live after this read — which is precisely "a second
/// path to this object now exists", the condition the `shared` flag records.
/// `Move` means this read was the name's last, so the value is being
/// transferred rather than duplicated and nothing needs marking; that is
/// tier 2 of MUTABILITY.md §4, and it is what keeps `push` in a loop
/// allocation-free (`liveness.rs`'s `Call` arm classifies a `mut` argument's
/// root as `Move` unconditionally).
///
/// Stage 6 did a deep `frog_clone` here instead. That was correct but paid
/// O(n) on every aliasing read whether or not anything ever wrote — 78% of
/// `benches/life.frog`, which passes a `List<List<Int>>` to a function nine
/// times per cell and never mutates it. The copy now happens at the write
/// instead (`emit_unshare`), so an alias costs one byte store.
///
/// **Why every `List` leaf, not just a `List`-typed value**: this exists to
/// protect against a mutation becoming visible through an alias, and the
/// mutable places a write can name (`typed_ast::Place`) reach *into*
/// aggregates — `b.items[0] = v` and `push(mut b.items, v)` both mutate a
/// list held in a struct field. So copying a struct aliases every list
/// hanging off it, and each one has to be marked; only the non-`List`
/// leaves are inert, since a struct or union value can itself only ever be
/// *rebound*, never mutated in place.
///
/// Marking just the whole-value-is-a-`List` case was a soundness hole, not
/// merely a missed one: `mut b = Box(items=[1, 2]); let c = b;
/// b.items[0] = 99` left `c.items` reading `[99, 2]`. It predates Stage 8 —
/// `flatten_place` has always accepted a field path — and survived the
/// Stage 7 audit because this function's own doc comment asserted that a
/// mutation could never name a struct field.
///
/// A non-`Var` expression (a literal, a call result, an `if`-merge, ...) is
/// always freshly produced and never needs this — nothing else can alias a
/// value that was just computed.
#[inline]
fn mark_shared_if_aliased(expr: &Spanned<TypedExpr>, vals: Vec<Value>, bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Vec<Value> {
    let TypedExprKind::Var(_) = &expr.item.kind else { return vals };
    if ctx.liveness.ownership(expr.item.id) != liveness::Ownership::Copy { return vals; }

    mark_shared_extracted(bcx, &expr.item.ty, &vals, ctx.structs);
    vals
}

/// Compile `expr` for a *transient* consumer: one that reads a pointer only
/// to address through it (a list index, a struct/union field, a loop's
/// `iterable`) and stores nothing new, so it must never trigger
/// `mark_shared_if_aliased` — that logic assumes the value is being
/// duplicated into a new persistent home, which is false here by
/// construction, and marking it would make every later write to the list
/// copy for nothing. Under Stage 6's eager cloning this distinction was
/// load-bearing rather than merely tidy: a scattered read inside a loop
/// (`xs[j]` for many `j`) is `Copy`-classified on nearly every occurrence
/// (the name is used again next iteration, by the next `j`), so cloning at
/// every `Var` occurrence turned an O(n) read pass into an O(n^2) storm.
///
/// Bypasses cloning only when `expr` is directly a `Var` node — a
/// `FieldAccess`/`Index` nested inside a transient target (which can't
/// happen structurally today, but if it ever could) still goes through the
/// ordinary cloning path via plain recursion into `compile_expr_multi`.
#[inline]
fn compile_expr_multi_transient(expr: &Spanned<TypedExpr>, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    match &expr.item.kind {
        TypedExprKind::Var(name) => read_var_raw(name, &expr.item.ty, bcx, vars, ctx.structs),
        _ => compile_expr_multi(expr, bcx, vars, ctx),
    }
}

/// `compile_expr_multi_transient` for a single-leaf transient target.
#[inline]
fn compile_expr_transient(expr: &Spanned<TypedExpr>, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Value {
    let vals = compile_expr_multi_transient(expr, bcx, vars, ctx);
    assert_eq!(vals.len(), 1, "compile_expr_transient on a multi-leaf expression");
    vals[0]
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
fn heap_mem() -> MachMemFlags { MachMemFlags::trusted() }

/// Address of raw slot `slot` in `list`'s flat data buffer. Reloads `data`
/// on each use rather than hoisting it, since a push can reallocate the
/// buffer out from under a cached copy.
fn list_slot_addr(bcx: &mut FunctionBuilder, list: Value, slot: Value) -> Value {
    let data = bcx.ins().load(types::I64, heap_mem(), list, offset_of!(FrogList, data) as i32);
    let byte_off = bcx.ins().imul_imm_s(slot, 8);
    bcx.ins().iadd(data, byte_off)
}

/// A list's per-element slot count, normalized to at least 1 exactly as
/// `frog_list_len` and `frog_list_get` do.
fn list_stride(bcx: &mut FunctionBuilder, list: Value) -> Value {
    let raw = bcx.ins().load(types::I32, heap_mem(), list, offset_of!(FrogList, stride) as i32);
    let s = bcx.ins().uextend(types::I64, raw);
    let is_zero = bcx.ins().icmp_imm_s(IntCC::Equal, s, 0);
    let one = bcx.ins().iconst(types::I64, 1);
    bcx.ins().select(is_zero, one, s)
}

/// Whether `FROG_COW_VERIFY` asked for the write barrier's unshared path to
/// be checked against the heap — see `ffi::frog_cow_verify`. Read per
/// `Codegen`, not per barrier.
fn cow_verify_enabled() -> bool {
    std::env::var_os("FROG_COW_VERIFY").is_some()
}

/// Byte offset of the copy-on-write `shared` flag within any GC object.
/// Every one begins with a `GcHeader` at offset 0 (`#[repr(C)]`), so this is
/// the same for `FrogList`, `FrogStr` and `FrogVariant`.
fn shared_flag_offset() -> i32 {
    (offset_of!(FrogList, header) + offset_of!(gc::GcHeader, shared)) as i32
}

/// Mark `val` as reachable by more than one live path, so a later write
/// through any of them copies first — MUTABILITY.md Stage 7's aliasing half.
/// One byte store, unconditionally: the flag is monotone, so re-marking an
/// already-shared object is a no-op and testing first would only add a
/// branch.
///
/// `val` must be a plain (untagged, non-null) heap pointer. Every caller
/// holds a value of exactly `Type::List`, which is never tagged (only a
/// union's word carries tag bits) and never null (`alloc_list` always
/// returns an object, even for `[]`).
fn emit_mark_shared(bcx: &mut FunctionBuilder, val: Value) {
    let one = bcx.ins().iconst(types::I8, 1);
    bcx.ins().store(heap_mem(), one, val, shared_flag_offset());
}

/// MUTABILITY.md Stage 7: mark every `List`-typed leaf of a value that was
/// just read *out of a container* — a list element, a struct or variant
/// field, a `for`-loop's element binding.
///
/// Unlike `mark_shared_if_aliased` there is no liveness test to make: the
/// container keeps its own path to that list whatever happens to the name
/// being bound, so the extracted value is aliased by construction.
///
/// This is what closes the gap Stage 6 recorded and deferred. Extraction is
/// not a `Var` read, so it never cloned, and `mut inner = rows[0]` followed
/// by `push(mut inner, ...)` mutated `rows` — a real value-semantics
/// violation, along with the same shape through a `for`-loop binding and
/// through a struct field. Closing it under eager cloning would have meant a
/// deep copy per element read, which is exactly the O(n^2) storm
/// `compile_expr_multi_transient` exists to avoid; under copy-on-write it is
/// a byte store, which is the whole reason this is affordable now.
fn mark_shared_extracted(bcx: &mut FunctionBuilder, ty: &Type, vals: &[Value], structs: &StructDefs) {
    for (v, (_, lty)) in vals.iter().zip(struct_fields(ty, structs).iter()) {
        if lty.is_list() {
            emit_mark_shared(bcx, *v);
        }
    }
}

/// The write barrier: yield a pointer to a version of `val` that no other
/// live path can observe, copying it first if it is shared. Returns the
/// pointer the write must go through — which the caller **must** store back
/// into the root's `Variable`, or the mutation lands on a copy nobody reads.
///
/// This is the whole cost of copy-on-write at a mutation site: a load, a
/// test, and a well-predicted branch. It is also self-extinguishing —
/// `frog_clone` produces a fresh, unshared object, so a loop that pushes
/// repeatedly pays at most one copy on its first iteration.
fn emit_unshare(bcx: &mut FunctionBuilder, ctx: &mut Ctx, val: Value) -> Value {
    emit_unshare_nested(bcx, ctx, val, 1)
}

/// `emit_unshare`, told how many live references legitimately reach `val`
/// at this point — see `ffi::frog_cow_verify`. Only `FROG_COW_VERIFY` reads
/// `allowed`; the emitted code is otherwise identical.
fn emit_unshare_nested(bcx: &mut FunctionBuilder, ctx: &mut Ctx, val: Value, allowed: i64) -> Value {
    let shared = bcx.ins().load(types::I8, heap_mem(), val, shared_flag_offset());

    let copy_bb = bcx.create_block();
    let keep_bb = bcx.create_block();
    let done_bb = bcx.create_block();
    bcx.append_block_param(done_bb, types::I64);
    bcx.ins().brif(shared, copy_bb, &[], keep_bb, &[]);

    // The unshared path: this write is about to land in place. Under
    // `FROG_COW_VERIFY` that claim is checked against the heap first.
    bcx.switch_to_block(keep_bb);
    bcx.seal_block(keep_bb);
    if ctx.cow_verify {
        let verify_id = ctx.func_ids["frog_cow_verify"];
        let callee = ctx.module.declare_func_in_func(verify_id, bcx.func);
        let allowed_val = bcx.ins().iconst(types::I64, allowed);
        bcx.ins().call(callee, &[val, allowed_val]);
    }
    bcx.ins().jump(done_bb, &[BlockArg::from(val)]);

    bcx.switch_to_block(copy_bb);
    bcx.seal_block(copy_bb);
    let clone_id = ctx.func_ids["frog_clone"];
    let callee = ctx.module.declare_func_in_func(clone_id, bcx.func);
    let call = bcx.ins().call(callee, &[val]);
    let cloned = bcx.inst_results(call)[0];
    declare_gc_ptr(bcx, cloned);
    bcx.ins().jump(done_bb, &[BlockArg::from(cloned)]);

    bcx.switch_to_block(done_bb);
    bcx.seal_block(done_bb);
    let out = bcx.block_params(done_bb)[0];
    declare_gc_ptr(bcx, out);
    out
}

/// What a `for` loop does with each body value: discard it (a statement
/// loop) or collect it into a fresh list (a comprehension). `Collect`
/// carries the element layout the result list must be allocated with;
/// `compile_for_loop` does that allocation itself, once it knows how long
/// the iterable is.
enum LoopOutput {
    Discard,
    Collect { stride: i64, ptr_mask: i64 },
}

/// Byte offset of payload slot `slot` within a `FrogVariant`.
fn variant_slot_offset(slot: usize) -> i32 {
    (size_of::<FrogVariant>() + slot * 8) as i32
}

/// Append one raw slot to `list`. The common case — spare capacity, so the
/// push is a store plus a length bump — is inline; growing the buffer still
/// goes through `frog_list_push`, which has to reallocate and report the new
/// bytes to the GC.
/// Push one *element*'s worth of slots — `leafs.len()` values from `vals`,
/// each converted to wire format — matching the `stride` an allocation
/// site computed as `leafs.len().max(1)` (`compile_list_lit`,
/// `Comprehension`, `push`). A field-less struct element (`data E()`) has
/// `leafs.len() == 0`, so a plain `zip` pushes nothing per element and
/// `len` never advances even though `stride` is 1 — every list of such
/// elements then reports length 0 to `frog_list_len` regardless of how
/// many were pushed. Pushing one dummy `0` slot per element instead keeps
/// `len` advancing in step with `stride`, matching what the allocation
/// site already promised.
fn push_element(bcx: &mut FunctionBuilder, ctx: &mut Ctx, list: Value, vals: &[Value], leafs: &[(String, Type)]) {
    if leafs.is_empty() {
        let zero = bcx.ins().iconst(types::I64, 0);
        emit_list_push(bcx, ctx, list, &[zero]);
        return;
    }
    let wires: Vec<Value> = vals.iter().zip(leafs.iter())
        .map(|(v, (_, lty))| to_i64_repr(bcx, lty, *v))
        .collect();
    emit_list_push(bcx, ctx, list, &wires);
}

/// Append `vals` — one whole element's worth of slots — to `list`.
///
/// The capacity test, the `data` reload and the length store are done once
/// for the element rather than once per slot: for a struct element every
/// slot is contiguous and the whole group either fits or doesn't, so the
/// per-slot versions were re-deriving the same address base and re-storing
/// the same length `stride` times over. `benches/orders.frog` pushes a
/// 4-leaf `Item`, so that was four bounds checks and four length stores per
/// element where one of each will do.
fn emit_list_push(bcx: &mut FunctionBuilder, ctx: &mut Ctx, list: Value, vals: &[Value]) {
    let n = vals.len() as i64;
    let len = bcx.ins().load(types::I32, heap_mem(), list, offset_of!(FrogList, len) as i32);
    let cap = bcx.ins().load(types::I32, heap_mem(), list, offset_of!(FrogList, cap) as i32);
    // Widened to I64 before the arithmetic: `len + n` in I32 would wrap for
    // a list near `u32::MAX` slots and wrongly report room. (`frog_list_push`
    // aborts before a list can actually get there, so this is belt-and-braces
    // — but the check is what that abort relies on being reachable.)
    let len64 = bcx.ins().uextend(types::I64, len);
    let cap64 = bcx.ins().uextend(types::I64, cap);
    let need = bcx.ins().iadd_imm_s(len64, n);
    let has_room = bcx.ins().icmp(IntCC::UnsignedLessThanOrEqual, need, cap64);

    let fast_bb = bcx.create_block();
    let slow_bb = bcx.create_block();
    let done_bb = bcx.create_block();
    bcx.ins().brif(has_room, fast_bb, &[], slow_bb, &[]);

    bcx.switch_to_block(fast_bb);
    bcx.seal_block(fast_bb);
    let base = list_slot_addr(bcx, list, len64);
    for (k, v) in vals.iter().enumerate() {
        bcx.ins().store(heap_mem(), *v, base, (k * 8) as i32);
    }
    let next_len = bcx.ins().ireduce(types::I32, need);
    bcx.ins().store(heap_mem(), next_len, list, offset_of!(FrogList, len) as i32);
    bcx.ins().jump(done_bb, &[]);

    // Not enough room for the whole element. `frog_list_push` grows the
    // buffer a slot at a time, so hand it every slot — the ones that did
    // fit take its own fast path.
    bcx.switch_to_block(slow_bb);
    bcx.seal_block(slow_bb);
    let push_id = ctx.func_ids["frog_list_push"];
    let push_ref = ctx.module.declare_func_in_func(push_id, bcx.func);
    for v in vals {
        bcx.ins().call(push_ref, &[list, *v]);
    }
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
        return bcx.ins().icmp_imm_s(IntCC::Equal, val, gc::immediate_variant(tag));
    }

    if !any_immediate {
        let actual = bcx.ins().load(types::I32, heap_mem(), val, offset_of!(FrogVariant, tag) as i32);
        return bcx.ins().icmp_imm_s(IntCC::Equal, actual, tag as i64);
    }

    let boxed_bb = bcx.create_block();
    let done_bb  = bcx.create_block();
    bcx.append_block_param(done_bb, types::I8);

    let tag_bits = bcx.ins().band_imm_s(val, gc::TAG_MASK);
    let is_immediate = bcx.ins().icmp_imm_s(IntCC::Equal, tag_bits, gc::TAG_IMMEDIATE);
    let no = bcx.ins().iconst(types::I8, 0);
    bcx.ins().brif(is_immediate, done_bb, &[BlockArg::from(no)], boxed_bb, &[]);

    bcx.switch_to_block(boxed_bb);
    bcx.seal_block(boxed_bb);
    let actual = bcx.ins().load(types::I32, heap_mem(), val, offset_of!(FrogVariant, tag) as i32);
    let matched = bcx.ins().icmp_imm_s(IntCC::Equal, actual, tag as i64);
    bcx.ins().jump(done_bb, &[BlockArg::from(matched)]);

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
    let tag_val    = bcx.ins().iconst(types::I64, tag as i64);
    let nslots_val = bcx.ins().iconst(types::I64, flat_vals.len() as i64);
    let mask_val   = bcx.ins().iconst(types::I64, gc_mask(flat_types));

    let alloc_id  = ctx.func_ids["frog_alloc_variant"];
    let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
    let call      = bcx.ins().call(alloc_ref, &[tag_val, nslots_val, mask_val]);
    let ptr       = bcx.inst_results(call)[0];
    // Root the new object itself before populating it — matches the
    // traversal order `for_each_heap_producer` uses for both callers
    // (fields'/value's own producers first, then `f()` for this box).
    declare_gc_ptr(bcx, ptr);

    for (i, (v, t)) in flat_vals.iter().zip(flat_types.iter()).enumerate() {
        let wire = to_i64_repr(bcx, t, *v);
        bcx.ins().store(heap_mem(), wire, ptr, variant_slot_offset(i));
    }
    ptr
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
    name: &str,
    ty: &Type,
) -> Variable {
    if let Some(&v) = vars.get(name) {
        return v;
    }
    let v = bcx.declare_var(cl_type(ty));
    // Before the first `def_var`, which Cranelift requires — an earlier
    // definition would be silently left out of every stack map.
    declare_gc_var(bcx, ty, v);
    vars.insert(name.to_string(), v);
    v
}

/// Declare-and-bind a name's `Variable` without a `Ctx` — used for
/// function-parameter and pre-seeded-REPL-binding setup, which both run
/// before `Ctx` is constructed (see `build_func_body`, `build_main_body`).
fn declare_and_def_var(
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    name: &str,
    ty: &Type,
    val: Value,
) {
    let v = bcx.declare_var(cl_type(ty));
    declare_gc_var(bcx, ty, v);
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

/// Wrap SSA values as block arguments. Cranelift 0.135 distinguishes a
/// `BlockArg` (which may also be an exception-table payload) from a plain
/// `Value`; every argument froglang passes is an ordinary value.
fn block_args(vals: &[Value]) -> Vec<BlockArg> {
    vals.iter().copied().map(BlockArg::from).collect()
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
        Type::Float => bcx.ins().bitcast(types::I64, MachMemFlags::new(), val),
        Type::Bool  => bcx.ins().uextend(types::I64, val),
        _           => val,
    }
}

/// Inverse of `to_i64_repr`: convert a raw i64 wire value (e.g. read back out
/// of a `FrogList`'s flat i64 buffer via `frog_list_get`) into `ty`'s native
/// Cranelift representation.
fn from_i64_repr(bcx: &mut FunctionBuilder, ty: &Type, val: Value) -> Value {
    match ty {
        Type::Float => bcx.ins().bitcast(types::F64, MachMemFlags::new(), val),
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

/// Emit the fault check that must precede an integer `sdiv`.
///
/// Cranelift's `sdiv` traps on the two inputs whose result isn't
/// representable — `rv == 0`, and `Int.MIN / -1` — and a trap is SIGILL,
/// which killed the process with exit 132 and no diagnostic at all. Both
/// conditions are folded into one branch here so the common path costs a
/// handful of well-predicted ALU ops and a not-taken jump; the fault path
/// calls `ffi::frog_div_error`, which reports and exits cleanly. The flag
/// passed to it distinguishes the two messages, so one call site covers both.
///
/// Emitting this leaves the builder positioned in a fresh "ok" block, so the
/// caller's following `sdiv` lands after the guard.
/// The constant `v` holds, if it is one — used to specialize guards on a
/// literal operand. Reads the IR back rather than threading constant-ness
/// down from the typed AST, so it sees every constant reaching this point
/// however it got here.
fn const_i64(bcx: &FunctionBuilder, v: Value) -> Option<i64> {
    use cranelift_codegen::ir::{instructions::InstructionData, Opcode, ValueDef};
    match bcx.func.dfg.value_def(v) {
        ValueDef::Result(inst, _) => match bcx.func.dfg.insts[inst] {
            InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
            _ => None,
        },
        _ => None,
    }
}

fn emit_int_div_guard(bcx: &mut FunctionBuilder, ctx: &mut Ctx, lv: Value, rv: Value) {
    // A literal divisor decides both fault conditions at compile time.
    // `x / 100` is common in ordinary code (`benches/orders.frog`'s `apply`
    // is a percentage calculation in the hot loop), and emitting five
    // instructions, a branch and two blocks to re-establish that 100 is
    // neither 0 nor -1 is pure overhead.
    match const_i64(bcx, rv) {
        // Divides by a constant that can never fault: no guard at all.
        Some(c) if c != 0 && c != -1 => return,
        // `x / -1` faults only for `Int::MIN`, so only that test is left.
        Some(-1) => {
            let is_min = bcx.ins().icmp_imm_s(IntCC::Equal, lv, i64::MIN);
            emit_div_fault_branch(bcx, ctx, is_min, false);
            return;
        }
        // `x / 0` always faults. Keep the call, drop the test around it.
        Some(_) => {
            let always = bcx.ins().iconst(types::I8, 1);
            emit_div_fault_branch(bcx, ctx, always, true);
            return;
        }
        None => {}
    }

    let is_zero = bcx.ins().icmp_imm_s(IntCC::Equal, rv, 0);
    let is_neg1 = bcx.ins().icmp_imm_s(IntCC::Equal, rv, -1);
    let is_min  = bcx.ins().icmp_imm_s(IntCC::Equal, lv, i64::MIN);
    let is_ovf  = bcx.ins().band(is_neg1, is_min);
    let is_bad  = bcx.ins().bor(is_zero, is_ovf);
    emit_div_fault_branch(bcx, ctx, is_bad, is_zero);
}

/// Branch to `frog_div_error` when `is_bad` holds, and carry on otherwise.
/// `is_zero` is the flag that call takes: which of the two faults this is.
fn emit_div_fault_branch(
    bcx: &mut FunctionBuilder,
    ctx: &mut Ctx,
    is_bad: Value,
    is_zero: impl Into<DivFaultKind>,
) {
    let fault_bb = bcx.create_block();
    let ok_bb    = bcx.create_block();
    bcx.ins().brif(is_bad, fault_bb, &[], ok_bb, &[]);

    bcx.switch_to_block(fault_bb);
    bcx.seal_block(fault_bb);
    let flag = match is_zero.into() {
        DivFaultKind::Dynamic(v) => bcx.ins().uextend(types::I64, v),
        DivFaultKind::Known(b) => bcx.ins().iconst(types::I64, b as i64),
    };
    let id = ctx.func_ids["frog_div_error"];
    let callee = ctx.module.declare_func_in_func(id, bcx.func);
    bcx.ins().call(callee, &[flag]);
    // `frog_div_error` is `-> !` and never comes back, but Cranelift still
    // needs this block terminated — and unlike a program-triggerable fault,
    // *this* really is unreachable, which is what `trap` is for.
    bcx.ins().trap(TrapCode::user(3).expect("3 is a valid user trap code"));

    bcx.switch_to_block(ok_bb);
    bcx.seal_block(ok_bb);
}

/// Which integer-division fault a guard reports: computed at runtime, or
/// already decided because the divisor was a literal.
enum DivFaultKind { Dynamic(Value), Known(bool) }
impl From<Value> for DivFaultKind { fn from(v: Value) -> Self { DivFaultKind::Dynamic(v) } }
impl From<bool> for DivFaultKind { fn from(b: bool) -> Self { DivFaultKind::Known(b) } }

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

/// Print a list by emitting the element loop here, in codegen, where the
/// *static* element type is still known — rather than handing the runtime a
/// pointer and a one-byte kind tag, which is what the deleted
/// `frog_list_print`/`list_elem_kind` pair did. The runtime has no type
/// information left, so that version printed `<struct>`/`<list>`
/// placeholders for every element that wasn't a scalar; recursing into
/// `print_value` instead means the type-directed walk reaches through a
/// list, which is what makes nested lists, struct elements and union
/// elements print at all (plans/DATA.md stage 0).
///
/// The loop mirrors `compile_for_loop`'s element read: `frog_list_len` gives
/// the element count, and leaf `k` of element `i` lives at slot
/// `i * stride + k`, needing no bounds check because the header already
/// bounded `i`.
fn print_list(elem_ty: &Type, list_val: Value, bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    print_fragment("[", bcx, ctx);

    let elem_leafs = struct_fields(elem_ty, ctx.structs);
    let len_callee = ctx.module.declare_func_in_func(ctx.func_ids["frog_list_len"], bcx.func);
    let len_call = bcx.ins().call(len_callee, &[list_val]);
    let len_val = bcx.inst_results(len_call)[0];
    let stride_val = list_stride(bcx, list_val);

    let header_bb = bcx.create_block();
    let body_bb   = bcx.create_block();
    let exit_bb   = bcx.create_block();
    bcx.append_block_param(header_bb, types::I64);

    let zero = bcx.ins().iconst(types::I64, 0);
    bcx.ins().jump(header_bb, &[BlockArg::from(zero)]);

    // Sealed only after the back edge below exists, as in `compile_for_loop`.
    bcx.switch_to_block(header_bb);
    let i = bcx.block_params(header_bb)[0];
    let in_range = bcx.ins().icmp(IntCC::SignedLessThan, i, len_val);
    bcx.ins().brif(in_range, body_bb, &[], exit_bb, &[]);

    bcx.switch_to_block(body_bb);
    bcx.seal_block(body_bb);

    // Separator before every element but the first.
    let sep_bb  = bcx.create_block();
    let elem_bb = bcx.create_block();
    let is_first = bcx.ins().icmp_imm_s(IntCC::Equal, i, 0);
    bcx.ins().brif(is_first, elem_bb, &[], sep_bb, &[]);
    bcx.switch_to_block(sep_bb);
    bcx.seal_block(sep_bb);
    print_fragment(", ", bcx, ctx);
    bcx.ins().jump(elem_bb, &[]);
    bcx.switch_to_block(elem_bb);
    bcx.seal_block(elem_bb);

    let base_slot = bcx.ins().imul(i, stride_val);
    let mut elem_vals = Vec::with_capacity(elem_leafs.len());
    for (leaf_idx, (_, lty)) in elem_leafs.iter().enumerate() {
        let slot = bcx.ins().iadd_imm_s(base_slot, leaf_idx as i64);
        let addr = list_slot_addr(bcx, list_val, slot);
        let raw = bcx.ins().load(types::I64, heap_mem(), addr, 0);
        elem_vals.push(from_i64_repr(bcx, lty, raw));
    }
    // Root every leaf before it is used, exactly as `compile_for_loop`'s
    // identical element read does. Nothing `print_value` emits allocates
    // today, so no collection can actually happen between these loads and
    // their last use — but "which callees allocate" is not a judgement this
    // site is allowed to make: `declare_gc_value`'s invariant is
    // whole-program ("every SSA value whose static type is a GC-scannable
    // column is declared"), precisely so that teaching printing to allocate
    // later — `repr`, which has to build a `Str`, is the obvious candidate —
    // cannot silently turn a live `Str`/`List` element pointer into a
    // use-after-free. Declaring a value that is never live across a
    // safepoint costs nothing: Cranelift omits it from every stack map.
    let elem_leaf_tys: Vec<Type> = elem_leafs.iter().map(|(_, t)| t.clone()).collect();
    declare_gc_leaves(bcx, &elem_vals, &elem_leaf_tys);
    let mut cursor = 0;
    print_value(elem_ty, &elem_vals, &mut cursor, bcx, ctx);

    // `print_value` may have emitted its own blocks (a union's tag
    // dispatch); the back edge goes from wherever it left the builder.
    let i_next = bcx.ins().iadd_imm_s(i, 1);
    bcx.ins().jump(header_bb, &[BlockArg::from(i_next)]);
    bcx.seal_block(header_bb);

    bcx.switch_to_block(exit_bb);
    bcx.seal_block(exit_bb);

    print_fragment("]", bcx, ctx);
}

/// Print one value without a trailing newline. Structs are represented as a
/// sequence of flattened leaf values, so this recursively consumes that
/// sequence according to the declared field layout.
fn print_value(ty: &Type, values: &[Value], cursor: &mut usize, bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    // `Range` gets its own round-trippable notation (`0..10`, matching
    // source syntax) rather than falling into the generic struct printer
    // below (which would otherwise print `Range(start=0, end=10)` — `Range`
    // is structurally a 2-field struct to `as_struct_name`, so this has to
    // be checked first).
    if let Some(elem_ty) = ty.as_range_elem() {
        let elem_ty = elem_ty.clone();
        print_value(&elem_ty, values, cursor, bcx, ctx);
        print_fragment("..", bcx, ctx);
        print_value(&elem_ty, values, cursor, bcx, ctx);
        return;
    }
    if let Some(name) = ty.as_struct_name() {
        print_fragment(&format!("{}(", name), bcx, ctx);
        let fields = ctx.structs.get(ty).expect("known struct in codegen");
        // A positionally-declared ("tuple struct") field has no
        // source-level name — `field_name_or_positional` gave it its
        // index instead (`is_positional_fields`) — so print it bare
        // (`Point(1, 2)`), not with that synthetic name attached
        // (`Point(0=1, 1=2)`).
        let positional = is_positional_fields(fields);
        for (i, (field, field_ty)) in fields.iter().enumerate() {
            if i != 0 { print_fragment(", ", bcx, ctx); }
            if !positional { print_fragment(&format!("{}=", field), bcx, ctx); }
            print_value(field_ty, values, cursor, bcx, ctx);
        }
        print_fragment(")", bcx, ctx);
        return;
    }
    if let Some(inner) = ty.as_list_elem() {
        let inner = inner.clone();
        let list_val = values[*cursor];
        *cursor += 1;
        print_list(&inner, list_val, bcx, ctx);
        return;
    }
    match ty {
        Type::Union(members) => {
            // Mirrors `struct_fields`'s `Union` arm: a boxed union
            // consumes 1 leaf (the pointer/immediate), an inline one
            // consumes its whole `UnionLayout` width.
            let n = struct_fields(&Type::Union(members.clone()), ctx.structs).len();
            print_union(members, &values[*cursor..*cursor + n], bcx, ctx);
            *cursor += n;
        }
        Type::Str => {
            let id = ctx.func_ids["frog_str_repr_print"];
            let callee = ctx.module.declare_func_in_func(id, bcx.func);
            bcx.ins().call(callee, &[values[*cursor]]);
            *cursor += 1;
        }
        // `None` carries no data — its slot exists only so leaf counts line
        // up (`struct_fields` gives it one, and `print_union_member`
        // materialises a dummy for it), so consume it and print the literal.
        //
        // Lowercase `none`: this prints a *value*, and the value literal is
        // the one that has to read back. `None` is the type's name, which is
        // not spellable in expression position (plans/DATA.md stage 1).
        Type::None => {
            print_fragment("none", bcx, ctx);
            *cursor += 1;
        }
        Type::Int | Type::Float | Type::Bool => {
            let id = match ty {
                Type::Int => "frog_int_print",
                Type::Float => "frog_float_print",
                Type::Bool => "frog_bool_print",
                _ => unreachable!(),
            };
            let callee = ctx.module.declare_func_in_func(ctx.func_ids[id], bcx.func);
            bcx.ins().call(callee, &[values[*cursor]]);
            *cursor += 1;
        }
        // An element type left unresolved by inference — which only happens
        // for a list that is provably empty (`print([])`), since any element
        // would have fixed the variable. The loop body is therefore dead;
        // emit a `<...>` form rather than a panic, because a placeholder may
        // exist but must never look like notation (plans/DATA.md stage 1).
        Type::TypeVar { .. } => {
            print_fragment("<?>", bcx, ctx);
            *cursor += 1;
        }
        other => panic!("print codegen does not support {:?}", other),
    }
}

/// Print an anonymous union's actual member at runtime — `arg_vals` is
/// `compile_expr_multi`'s output for the union-typed expression: `[ptr_or_
/// immediate]` for a boxed union, or the whole `UnionLayout` width for an
/// inline one, whose tag rides in slot 0's low bits instead of in a boxed
/// header. Branches on the tag at runtime — one
/// comparison block per member except the last, which needs none since the
/// tag is guaranteed to be one of `members`' indices — and prints whichever
/// member actually matched, each in its own block so only that one runs.
fn print_union(members: &[Type], arg_vals: &[Value], bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    // A union-typed struct field (e.g. `Add(lhs: Node, rhs: Node)`'s own
    // `lhs`/`rhs`) is stored as a single opaque boxed pointer, never
    // flattened — that's exactly what lets a self-referential type like
    // `Node` exist at all (see `hoist_data_decls`'s self-reference check,
    // which only rejects an *unboxed* cycle). Printing recurses through
    // `print_union_member` → `print_value` back into `print_union` for that
    // same field type, so a genuinely recursive union would need unbounded
    // branch trees at codegen time — reject it clearly instead of
    // hanging the compiler.
    let shape = format!("{:?}", members);
    if ctx.printing_unions.contains(&shape) {
        panic!(
            "unsupported: printing a recursive union type ({}) isn't supported yet \
             — write a recursive function that formats it field-by-field instead.",
            Type::Union(members.to_vec())
        );
    }
    ctx.printing_unions.push(shape);
    print_union_body(members, arg_vals, bcx, ctx);
    ctx.printing_unions.pop();
}

/// Resolve a `Type::Union` back to the nominal `data X is A | B | ...` it
/// stands for, if it is one. Every `UnionDef` stores the exact normalized
/// `Type::Union` its name denotes (`UnionDef.ty`), so this is a direct
/// lookup rather than a reconstruction — the same relation
/// `TypeChecker::resolve_union` provides on the front-end side.
///
/// The distinction is not cosmetic. A nominal union's runtime tag is its
/// variant's **declaration** index (that's what `VariantInit` writes),
/// whereas a `Type::Union`'s members are sorted by display string
/// (`Type::normalize` step 3). Treating the sorted position as the tag
/// therefore mislabels every variant whose declared order isn't
/// alphabetical, and treating a payload-less variant as boxed dereferences
/// its immediate.
fn resolve_nominal_union<'a>(members: &[Type], unions: &'a UnionDefs) -> Option<(&'a str, &'a UnionDef)> {
    unions.iter()
        .find(|(_, def)| matches!(&def.ty, Type::Union(m) if m.as_slice() == members))
        .map(|(name, def)| (name.as_str(), def))
}

fn print_union_body(members: &[Type], arg_vals: &[Value], bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    let inline = union_is_inline(members, ctx.structs);
    let (nominal, cases) = union_dispatch_cases(members, inline, ctx.unions);

    let merge_bb = bcx.create_block();
    let last = cases.len() - 1;

    for (i, (member_ty, tag)) in cases.iter().enumerate() {
        let body_bb = bcx.create_block();
        // The final case needs no test: the tag is guaranteed to be one of
        // these, so whatever is left must be it.
        let cont_bb = if i < last { Some(bcx.create_block()) } else { None };

        if let Some(cont_bb) = cont_bb {
            let is_match = emit_union_tag_test(members, member_ty, *tag, arg_vals, inline, nominal, bcx);
            bcx.ins().brif(is_match, body_bb, &[], cont_bb, &[]);
        } else {
            bcx.ins().jump(body_bb, &[]);
        }

        bcx.switch_to_block(body_bb);
        bcx.seal_block(body_bb);
        print_union_member(members, member_ty, arg_vals, inline, bcx, ctx);
        bcx.ins().jump(merge_bb, &[]);

        if let Some(cont_bb) = cont_bb {
            bcx.switch_to_block(cont_bb);
            bcx.seal_block(cont_bb);
        }
    }

    bcx.switch_to_block(merge_bb);
    bcx.seal_block(merge_bb);
}

/// Narrow a union member's carrier out of `slots` — the same unboxing
/// `TypedExprKind::Narrow` does, inline or boxed — into that member's own
/// flattened leaves, ready for `print_value`/`eq_value`.
///
/// `None` (and `Never`, which is only ever a layout placeholder) carries no
/// data, so it gets a dummy leaf: every walk over a union's members expects
/// one value per member slot, and `struct_fields` counts one for it.
fn unpack_union_slots(members: &[Type], member_ty: &Type, slots: &[Value], inline: bool, bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Vec<Value> {
    if inline {
        if matches!(member_ty, Type::None | Type::Never) {
            vec![bcx.ins().iconst(types::I64, 0)]
        } else {
            unpack_union_member(members, member_ty, slots, bcx, ctx.structs)
        }
    } else if *member_ty == Type::None {
        vec![bcx.ins().iconst(types::I64, 0)]
    } else if matches!(member_ty, Type::Int | Type::Float | Type::Bool) {
        vec![from_i64_repr(bcx, member_ty, slots[0])]
    } else {
        let leaf_types: Vec<Type> = struct_fields(member_ty, ctx.structs).into_iter().map(|(_, t)| t).collect();
        read_variant_slots(slots[0], 0, &leaf_types, bcx, ctx)
    }
}

fn print_union_member(members: &[Type], member_ty: &Type, slots: &[Value], inline: bool, bcx: &mut FunctionBuilder, ctx: &mut Ctx) {
    let values = unpack_union_slots(members, member_ty, slots, inline, bcx, ctx);
    let mut cursor = 0;
    print_value(member_ty, &values, &mut cursor, bcx, ctx);
}

/// The member list a union dispatch has to walk, each paired with the tag
/// that identifies it at runtime — shared by `print_union_body` and
/// `eq_union_body` so the two agree on tag provenance, which is the thing
/// that has already gone wrong once (see `resolve_nominal_union`).
///
/// A nominal union walks its variants in *declared* order, since that is
/// what `emit_is_variant` and the boxed `FrogVariant` header agree on; an
/// anonymous one walks the normalized member list. An inline union needs
/// neither: its tag is slot 0's low bits and its members are exactly the
/// normalized list, so declared order is irrelevant.
///
/// Takes `unions` rather than the whole `Ctx` so the returned `UnionDef`
/// borrow lives as long as the definitions themselves, leaving the caller
/// free to keep using `ctx` mutably while it emits.
fn union_dispatch_cases<'a>(members: &[Type], inline: bool, unions: &'a UnionDefs) -> (Option<(&'a str, &'a UnionDef)>, Vec<(Type, u32)>) {
    let nominal = if inline { None } else { resolve_nominal_union(members, unions) };
    let cases = match nominal {
        Some((name, def)) => def.variants.iter().enumerate()
            .map(|(i, (variant, _))| (Type::strukt(format!("{}.{}", name, variant)), i as u32))
            .collect(),
        None => members.iter().cloned().zip(0u32..).collect(),
    };
    (nominal, cases)
}

/// Emit the runtime test "does this union value hold the member at `tag`?".
/// The three representations need three different tests, and picking the
/// wrong one is silently wrong rather than loud — see `resolve_nominal_union`.
fn emit_union_tag_test(
    members: &[Type], member_ty: &Type, tag: u32, vals: &[Value],
    inline: bool, nominal: Option<(&str, &UnionDef)>, bcx: &mut FunctionBuilder,
) -> Value {
    if inline {
        emit_inline_tag_test(bcx, vals, tag as usize)
    } else if let Some((_, def)) = nominal {
        emit_is_variant(bcx, vals[0], def, tag)
    } else {
        // Anonymous boxed unions only ever have `None` as an immediate
        // member — see `TypedExprKind::Widen`.
        let any_immediate = members.contains(&Type::None);
        emit_tag_test(bcx, vals[0], *member_ty == Type::None, any_immediate, tag)
    }
}

/// Structural `==` for two lists of static element type `elem_ty`, emitted
/// here for the same reason `print_list` is: the runtime sees a pointer and
/// a stride, and could only compare identity or raw slots, which is wrong
/// for a `Str` element (two equal strings, two pointers) and wrong again for
/// a nested list.
///
/// Unequal lengths decide it without touching an element; otherwise this is
/// `print_list`'s loop with an early exit — the first unequal element jumps
/// straight to the merge with `false`, so a long common prefix costs only
/// what it compares. Returns an `I8` boolean.
fn eq_list(elem_ty: &Type, lv: Value, rv: Value, bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Value {
    let elem_leafs = struct_fields(elem_ty, ctx.structs);
    let len_id = ctx.func_ids["frog_list_len"];

    let len_callee = ctx.module.declare_func_in_func(len_id, bcx.func);
    let l_len_call = bcx.ins().call(len_callee, &[lv]);
    let l_len = bcx.inst_results(l_len_call)[0];
    let r_len_call = bcx.ins().call(len_callee, &[rv]);
    let r_len = bcx.inst_results(r_len_call)[0];

    let merge_bb = bcx.create_block();
    bcx.append_block_param(merge_bb, types::I8);
    let loop_bb   = bcx.create_block();
    let header_bb = bcx.create_block();
    let body_bb   = bcx.create_block();
    bcx.append_block_param(header_bb, types::I64);

    let same_len = bcx.ins().icmp(IntCC::Equal, l_len, r_len);
    let no = bcx.ins().iconst(types::I8, 0);
    bcx.ins().brif(same_len, loop_bb, &[], merge_bb, &[BlockArg::from(no)]);

    bcx.switch_to_block(loop_bb);
    bcx.seal_block(loop_bb);
    // Same element type on both sides, so the two strides agree; each list
    // still carries its own, and the loads have to use the matching one.
    let l_stride = list_stride(bcx, lv);
    let r_stride = list_stride(bcx, rv);
    let zero = bcx.ins().iconst(types::I64, 0);
    bcx.ins().jump(header_bb, &[BlockArg::from(zero)]);

    // Sealed only after the back edge below exists, as in `print_list`.
    bcx.switch_to_block(header_bb);
    let i = bcx.block_params(header_bb)[0];
    let in_range = bcx.ins().icmp(IntCC::SignedLessThan, i, l_len);
    // Ran off the end with nothing unequal: the lists are equal.
    let yes = bcx.ins().iconst(types::I8, 1);
    bcx.ins().brif(in_range, body_bb, &[], merge_bb, &[BlockArg::from(yes)]);

    bcx.switch_to_block(body_bb);
    bcx.seal_block(body_bb);
    let l_base = bcx.ins().imul(i, l_stride);
    let r_base = bcx.ins().imul(i, r_stride);
    let mut l_vals = Vec::with_capacity(elem_leafs.len());
    let mut r_vals = Vec::with_capacity(elem_leafs.len());
    for (leaf_idx, (_, lty)) in elem_leafs.iter().enumerate() {
        for (base, list, out) in [(l_base, lv, &mut l_vals), (r_base, rv, &mut r_vals)] {
            let slot = bcx.ins().iadd_imm_s(base, leaf_idx as i64);
            let addr = list_slot_addr(bcx, list, slot);
            let raw = bcx.ins().load(types::I64, heap_mem(), addr, 0);
            out.push(from_i64_repr(bcx, lty, raw));
        }
    }
    // Root both operands' leaves before either is used — see `print_list`'s
    // identical declaration for why this is not a per-site judgement about
    // whether `eq_value`'s callees happen to allocate.
    let elem_leaf_tys: Vec<Type> = elem_leafs.iter().map(|(_, t)| t.clone()).collect();
    declare_gc_leaves(bcx, &l_vals, &elem_leaf_tys);
    declare_gc_leaves(bcx, &r_vals, &elem_leaf_tys);
    let mut cursor = 0;
    let eq = eq_value(elem_ty, &l_vals, &r_vals, &mut cursor, bcx, ctx);

    // `eq_value` may have emitted its own blocks (a nested list's loop);
    // the back edge goes from wherever it left the builder.
    let i_next = bcx.ins().iadd_imm_s(i, 1);
    let no = bcx.ins().iconst(types::I8, 0);
    bcx.ins().brif(eq, header_bb, &[BlockArg::from(i_next)], merge_bb, &[BlockArg::from(no)]);
    bcx.seal_block(header_bb);

    bcx.switch_to_block(merge_bb);
    bcx.seal_block(merge_bb);
    bcx.block_params(merge_bb)[0]
}

/// `x in xs` — scans `haystack` (a `List<elem_ty>`) for an element equal to
/// `needle` (`needle`'s already-flattened leaves), short-circuiting on the
/// first match. Mirrors `eq_list`'s loop skeleton, but compares one fixed
/// value against each element in turn instead of two lists pairwise.
/// Returns an `I8` boolean.
fn list_contains(elem_ty: &Type, needle: &[Value], haystack: Value, bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Value {
    let elem_leafs = struct_fields(elem_ty, ctx.structs);
    let len_id = ctx.func_ids["frog_list_len"];
    let len_callee = ctx.module.declare_func_in_func(len_id, bcx.func);
    let len_call = bcx.ins().call(len_callee, &[haystack]);
    let len = bcx.inst_results(len_call)[0];

    let merge_bb = bcx.create_block();
    bcx.append_block_param(merge_bb, types::I8);
    let header_bb = bcx.create_block();
    let body_bb   = bcx.create_block();
    bcx.append_block_param(header_bb, types::I64);

    let stride = list_stride(bcx, haystack);
    let zero = bcx.ins().iconst(types::I64, 0);
    bcx.ins().jump(header_bb, &[BlockArg::from(zero)]);

    // Sealed only after the back edge below exists, as in `eq_list`.
    bcx.switch_to_block(header_bb);
    let i = bcx.block_params(header_bb)[0];
    let in_range = bcx.ins().icmp(IntCC::SignedLessThan, i, len);
    // Ran off the end without a match: not found.
    let no = bcx.ins().iconst(types::I8, 0);
    bcx.ins().brif(in_range, body_bb, &[], merge_bb, &[BlockArg::from(no)]);

    bcx.switch_to_block(body_bb);
    bcx.seal_block(body_bb);
    let base = bcx.ins().imul(i, stride);
    let mut vals = Vec::with_capacity(elem_leafs.len());
    for (leaf_idx, (_, lty)) in elem_leafs.iter().enumerate() {
        let slot = bcx.ins().iadd_imm_s(base, leaf_idx as i64);
        let addr = list_slot_addr(bcx, haystack, slot);
        let raw = bcx.ins().load(types::I64, heap_mem(), addr, 0);
        vals.push(from_i64_repr(bcx, lty, raw));
    }
    let elem_leaf_tys: Vec<Type> = elem_leafs.iter().map(|(_, t)| t.clone()).collect();
    declare_gc_leaves(bcx, &vals, &elem_leaf_tys);
    let mut cursor = 0;
    let eq = eq_value(elem_ty, needle, &vals, &mut cursor, bcx, ctx);

    // `eq_value` may have emitted its own blocks; jump from wherever it
    // left the builder.
    let i_next = bcx.ins().iadd_imm_s(i, 1);
    let yes = bcx.ins().iconst(types::I8, 1);
    bcx.ins().brif(eq, merge_bb, &[BlockArg::from(yes)], header_bb, &[BlockArg::from(i_next)]);
    bcx.seal_block(header_bb);

    bcx.switch_to_block(merge_bb);
    bcx.seal_block(merge_bb);
    bcx.block_params(merge_bb)[0]
}

/// Structural `==` for two values of the same union type — the equality
/// counterpart of `print_union`, and the reason a `data` union compares by
/// contents rather than by box address.
///
/// Both operands are dispatched at runtime, because which member each holds
/// is only known then. The shape is: find the member the *left* one holds
/// (one tag test per case but the last, which needs none — the tag is
/// guaranteed to be one of them), then ask whether the right one holds that
/// same member; if it doesn't the answer is `false` without unpacking
/// anything, and if it does, unpack both carriers and compare them with
/// `eq_value`. Returns an `I8` boolean.
fn eq_union(members: &[Type], l: &[Value], r: &[Value], bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Value {
    // A union-typed struct field is stored as a single opaque boxed pointer,
    // never flattened, which is what makes `data Node is Add(lhs: Node, ...)`
    // representable — and what makes this walk re-derive the same union type
    // with no static bound on depth. `TypeChecker::check_comparable` predicts
    // this and reports it with a span; the guard stays as the backstop, in
    // the same shape as `print_union`'s.
    let shape = format!("{:?}", members);
    if ctx.comparing_unions.contains(&shape) {
        panic!(
            "unsupported: comparing a recursive union type ({}) isn't supported yet \
             — write a recursive function that compares it field-by-field instead.",
            Type::Union(members.to_vec())
        );
    }
    ctx.comparing_unions.push(shape);
    let result = eq_union_body(members, l, r, bcx, ctx);
    ctx.comparing_unions.pop();
    result
}

fn eq_union_body(members: &[Type], l: &[Value], r: &[Value], bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Value {
    let inline = union_is_inline(members, ctx.structs);
    let (nominal, cases) = union_dispatch_cases(members, inline, ctx.unions);

    let merge_bb = bcx.create_block();
    bcx.append_block_param(merge_bb, types::I8);
    let last = cases.len() - 1;

    for (i, (member_ty, tag)) in cases.iter().enumerate() {
        let body_bb = bcx.create_block();
        let cont_bb = if i < last { Some(bcx.create_block()) } else { None };

        if let Some(cont_bb) = cont_bb {
            let is_match = emit_union_tag_test(members, member_ty, *tag, l, inline, nominal, bcx);
            bcx.ins().brif(is_match, body_bb, &[], cont_bb, &[]);
        } else {
            bcx.ins().jump(body_bb, &[]);
        }

        bcx.switch_to_block(body_bb);
        bcx.seal_block(body_bb);

        // The right operand is tested in *every* case, the last included:
        // "left holds this member" says nothing about the right one, and
        // two different members are never equal.
        let same_bb = bcx.create_block();
        let r_match = emit_union_tag_test(members, member_ty, *tag, r, inline, nominal, bcx);
        let no = bcx.ins().iconst(types::I8, 0);
        bcx.ins().brif(r_match, same_bb, &[], merge_bb, &[BlockArg::from(no)]);

        bcx.switch_to_block(same_bb);
        bcx.seal_block(same_bb);
        let l_vals = unpack_union_slots(members, member_ty, l, inline, bcx, ctx);
        let r_vals = unpack_union_slots(members, member_ty, r, inline, bcx, ctx);
        let mut cursor = 0;
        let eq = eq_value(member_ty, &l_vals, &r_vals, &mut cursor, bcx, ctx);
        // `eq_value` may have emitted its own blocks; jump from wherever it
        // left the builder.
        bcx.ins().jump(merge_bb, &[BlockArg::from(eq)]);

        if let Some(cont_bb) = cont_bb {
            bcx.switch_to_block(cont_bb);
            bcx.seal_block(cont_bb);
        }
    }

    bcx.switch_to_block(merge_bb);
    bcx.seal_block(merge_bb);
    bcx.block_params(merge_bb)[0]
}

/// Structural `==` for one value of type `ty`, given both operands' flattened
/// leaves — `print_value`'s counterpart, consuming the same leaf sequence
/// through the same `cursor`. Returns an `I8` boolean.
///
/// Only reached from `eq_list` (an element, and recursively that element's
/// fields): a top-level struct comparison is desugared into a per-field
/// conjunction of source-level `==` back in `TypeChecker::desugar_struct_eq`,
/// which short-circuits. This one folds with a bitwise `and` instead — the
/// leaves are already loaded and comparing them has no side effects, so
/// there is nothing to be gained by branching per field.
fn eq_value(ty: &Type, l: &[Value], r: &[Value], cursor: &mut usize, bcx: &mut FunctionBuilder, ctx: &mut Ctx) -> Value {
    if ty.is_struct() {
        let fields = ctx.structs.get(ty).expect("known struct in codegen").clone();
        let mut acc: Option<Value> = None;
        for (_, field_ty) in &fields {
            let sub = eq_value(field_ty, l, r, cursor, bcx, ctx);
            acc = Some(match acc {
                None => sub,
                Some(prev) => bcx.ins().band(prev, sub),
            });
        }
        // A field-less struct: all its values are equal.
        return acc.unwrap_or_else(|| bcx.ins().iconst(types::I8, 1));
    }
    if let Some(inner) = ty.as_list_elem() {
        let inner = inner.clone();
        let (lv, rv) = (l[*cursor], r[*cursor]);
        *cursor += 1;
        return eq_list(&inner, lv, rv, bcx, ctx);
    }
    let (lv, rv) = (l.get(*cursor).copied(), r.get(*cursor).copied());
    match ty {
        Type::Str => {
            *cursor += 1;
            let callee = ctx.module.declare_func_in_func(ctx.func_ids["frog_str_eq"], bcx.func);
            let call = bcx.ins().call(callee, &[lv.expect("Str leaf"), rv.expect("Str leaf")]);
            let result = bcx.inst_results(call)[0];
            bcx.ins().ireduce(types::I8, result)
        }
        Type::Int | Type::Bool => {
            *cursor += 1;
            bcx.ins().icmp(IntCC::Equal, lv.expect("scalar leaf"), rv.expect("scalar leaf"))
        }
        Type::Float => {
            *cursor += 1;
            bcx.ins().fcmp(FloatCC::Equal, lv.expect("Float leaf"), rv.expect("Float leaf"))
        }
        // An element type inference never fixed, which only happens for a
        // provably empty list (`[] == []`) — the loop body is dead, so the
        // answer here is never observed. See `print_value`'s same arm.
        Type::TypeVar { .. } => {
            *cursor += 1;
            bcx.ins().iconst(types::I8, 1)
        }
        // Mirrors `print_value`'s `Union` arm: a boxed union consumes 1 leaf
        // (the pointer/immediate), an inline one its whole `UnionLayout`
        // width. Which member is held is a runtime question, so this is a
        // dispatch, not a comparison — see `eq_union`.
        Type::Union(members) => {
            let members = members.clone();
            let n = struct_fields(&Type::Union(members.clone()), ctx.structs).len();
            let (ls, rs) = (l[*cursor..*cursor + n].to_vec(), r[*cursor..*cursor + n].to_vec());
            *cursor += n;
            eq_union(&members, &ls, &rs, bcx, ctx)
        }
        // `None` has exactly one value, so two of them are equal without
        // looking. The leaf exists only so counts line up — `struct_fields`
        // gives it one and `unpack_union_slots` materialises a dummy for it,
        // the same as printing does.
        Type::None => {
            *cursor += 1;
            bcx.ins().iconst(types::I8, 1)
        }
        other => panic!("equality codegen does not support {:?}", other),
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
/// Every SSA value whose static type is GC-scannable is declared to
/// Cranelift (`declare_gc_value`/`declare_gc_var`) at or near the point it
/// is produced, so it stays visible to a collection triggered anywhere it
/// remains live — see `gc.rs`'s "Precise roots".
/// The dotted field path made of `segs`, which must all be
/// `PlaceSeg::Field` — `""` for an empty run.
fn dotted_fields(segs: &[PlaceSeg]) -> String {
    segs.iter()
        .map(|s| match s {
            PlaceSeg::Field(f) => f.as_str(),
            PlaceSeg::Index { .. } => unreachable!("caller slices on `Index` boundaries"),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Where a place's leaf actually lives, once `emit_place_ref` has walked
/// and unshared everything above it. Both of the language's mutations, and
/// every `mut` argument, address storage through one of these two shapes.
enum PlaceRef {
    /// Flattened `Variable`s keyed `root[.field]*` — the leaf isn't on the
    /// heap at all. A struct is a flat set of named bindings, so a pure
    /// field path is a rebind, not a store.
    Vars(String),
    /// Slots `offset..` of element `index` of the heap-allocated `list`.
    Slot { list: Value, index: Value, offset: usize },
}

/// Walk a place, unsharing every list *above* its leaf — `MUTABILITY.md`
/// Stage 8 — and return where the leaf lives.
///
/// Every list on the path has to be unshared, not just the innermost one:
/// writing into `rows[y]`'s buffer is observable through any other binding
/// that can still reach `rows`. And because unsharing may *replace* a list
/// with a private copy, each new pointer is stored back into the slot it
/// came from before the walk descends — that write-back is what keeps the
/// chain connected without reference counts.
///
/// The walk is O(depth), not O(size): `emit_unshare` copies only when the
/// `shared` bit is actually set, and `GcHeap::clone_obj` is a *deep* clone,
/// so once an outer list has been copied every list below it is private
/// already and the remaining steps are pure tests.
///
/// The leaf itself is left alone. A caller that is about to mutate it in
/// place unshares it too (`push` does); one that is handing it to a callee
/// does not, because the callee's own barrier will, and copying here would
/// defeat the point.
fn emit_place_ref(
    place: &Place,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> PlaceRef {
    let root = place.root.as_str();
    let path = &place.path;
    let Some(last) = path.iter().rposition(|s| matches!(s, PlaceSeg::Index { .. })) else {
        return PlaceRef::Vars(var_key(root, &dotted_fields(path)));
    };
    let PlaceSeg::Index { index, elem_ty } = &path[last] else {
        unreachable!("`last` was found by matching `Index`")
    };

    let segs = &path[..last];
    let first = segs.iter().position(|s| matches!(s, PlaceSeg::Index { .. }));

    // Fields before the first `[index]` — the whole run, when there is no
    // index — name one flattened leaf `Variable` holding the outermost
    // list. It was declared when `root` was bound: a `List`-typed leaf
    // never recurses further in `struct_fields`, so this names exactly one.
    let list_key = var_key(root, &dotted_fields(&segs[..first.unwrap_or(segs.len())]));
    let list_var = *vars.get(&list_key)
        .unwrap_or_else(|| panic!("place root '{}' is unbound in codegen", list_key));
    let mut cur = bcx.use_var(list_var);
    cur = emit_unshare(bcx, ctx, cur);
    bcx.def_var(list_var, cur);

    // Every further `[index]` step reads a nested list out of its parent,
    // unshares it, and writes it back. A field run after an index names a
    // slot within that element's own flattened layout (`grid[y].cells[x]`),
    // so it contributes an offset, exactly as the trailing run does below.
    let mut rest = first.map_or(&[][..], |i| &segs[i..]);
    while let Some((PlaceSeg::Index { index, elem_ty }, tail)) = rest.split_first() {
        let next_index = tail.iter().position(|s| matches!(s, PlaceSeg::Index { .. })).unwrap_or(tail.len());
        let offset = field_run_offset(elem_ty, &tail[..next_index], ctx);
        let idx_val = compile_expr(index, bcx, vars, ctx);
        let inner = emit_slot_load(bcx, ctx, cur, idx_val, offset);
        let inner = emit_unshare_nested(bcx, ctx, inner, 2);
        emit_slot_store(bcx, ctx, cur, idx_val, offset, inner);
        cur = inner;
        rest = &tail[next_index..];
    }

    let index = compile_expr(index, bcx, vars, ctx);
    let offset = field_run_offset(elem_ty, &path[last + 1..], ctx);
    PlaceRef::Slot { list: cur, index, offset }
}

/// The flattened-leaf offset a run of `.field` steps names within `ty`.
fn field_run_offset(ty: &Type, segs: &[PlaceSeg], ctx: &Ctx) -> usize {
    let fields = dotted_fields(segs);
    if fields.is_empty() { 0 } else { dotted_leaf_range(ty, &fields, ctx.structs).0 }
}

fn emit_slot_load(bcx: &mut FunctionBuilder, ctx: &mut Ctx, list: Value, index: Value, offset: usize) -> Value {
    let off_val = bcx.ins().iconst(types::I64, offset as i64);
    let get_id = ctx.func_ids["frog_list_get"];
    let callee = ctx.module.declare_func_in_func(get_id, bcx.func);
    let call = bcx.ins().call(callee, &[list, index, off_val]);
    let raw = bcx.inst_results(call)[0];
    declare_gc_ptr(bcx, raw);
    raw
}

fn emit_slot_store(bcx: &mut FunctionBuilder, ctx: &mut Ctx, list: Value, index: Value, offset: usize, val: Value) {
    let off_val = bcx.ins().iconst(types::I64, offset as i64);
    let set_id = ctx.func_ids["frog_list_set"];
    let callee = ctx.module.declare_func_in_func(set_id, bcx.func);
    bcx.ins().call(callee, &[list, index, off_val, val]);
}

/// Read a place's leaf, one value per flattened leaf of `ty`.
fn place_load(
    pref: &PlaceRef,
    ty: &Type,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Vec<Value> {
    let leafs = struct_fields(ty, ctx.structs);
    match pref {
        PlaceRef::Vars(prefix) => leafs.iter()
            .map(|(sub, lty)| {
                let var = get_or_declare_var(bcx, vars, &var_key(prefix, sub), lty);
                bcx.use_var(var)
            })
            .collect(),
        PlaceRef::Slot { list, index, offset } => leafs.iter().enumerate()
            .map(|(i, (_, lty))| {
                let raw = emit_slot_load(bcx, ctx, *list, *index, offset + i);
                from_i64_repr(bcx, lty, raw)
            })
            .collect(),
    }
}

/// Write `vals` — one per flattened leaf of `ty` — into a place's leaf.
fn place_store(
    pref: &PlaceRef,
    ty: &Type,
    vals: &[Value],
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) {
    let leafs = struct_fields(ty, ctx.structs);
    match pref {
        PlaceRef::Vars(prefix) => {
            for (v, (sub, lty)) in vals.iter().zip(leafs.iter()) {
                let var = get_or_declare_var(bcx, vars, &var_key(prefix, sub), lty);
                bcx.def_var(var, *v);
            }
        },
        PlaceRef::Slot { list, index, offset } => {
            for (i, (v, (_, lty))) in vals.iter().zip(leafs.iter()).enumerate() {
                let raw = to_i64_repr(bcx, lty, *v);
                emit_slot_store(bcx, ctx, *list, *index, offset + i, raw);
            }
        },
    }
}

/// Resolve a place to the `List` at its leaf, unshared and ready to be
/// mutated in place — `push`'s receiver. The unshared pointer is written
/// back to the place, so the mutation is reachable from the root.
fn emit_mutable_list(
    place: &Place,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Value {
    let pref = emit_place_ref(place, bcx, vars, ctx);
    // How many live references legitimately reach this leaf, for
    // `FROG_COW_VERIFY` (see `ffi::frog_cow_verify`). A leaf reached through
    // a parent list has a second one — that parent's own slot.
    //
    // So does a `mut` *parameter*, and for a reason the callee cannot see:
    // the caller may have passed a place that lives inside a container
    // (`add(mut rows[1])`), which contributes a reference this function has
    // no way to know about. That is exactly the limitation Racordon et al.
    // (JOT 2022, §6) record for `inout` — "the callee has no way to
    // determine whether that pointer refers to a value inside of a shared
    // buffer". Their answer is a defensive copy by the caller; ours is to
    // let the verifier expect the extra reference, since the mutation is
    // sound either way: the caller unshared the whole chain down to that
    // slot on the way in, and copies the result back out afterwards.
    let from_mut_param = place.path.is_empty()
        && ctx.mut_params.iter().any(|(n, _)| *n == place.root);
    let allowed = match pref {
        PlaceRef::Slot { .. } => 2,
        PlaceRef::Vars(_) if from_mut_param => 2,
        PlaceRef::Vars(_) => 1,
    };
    let list = place_load(&pref, &Type::list(Type::Int), bcx, vars, ctx)[0];
    let list = emit_unshare_nested(bcx, ctx, list, allowed);
    place_store(&pref, &Type::list(Type::Int), &[list], bcx, vars, ctx);
    list
}

/// Codegen for `TypedExprKind::PlaceAssign` — resolve the place, store the
/// value into it. The two shapes a place can resolve to (`PlaceRef`) are
/// what used to be this function's two branches.
fn compile_place_assign(
    place: &Place,
    value: &Spanned<TypedExpr>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) {
    let pref = emit_place_ref(place, bcx, vars, ctx);
    let vals = compile_expr_multi(value, bcx, vars, ctx);
    place_store(&pref, &value.item.ty, &vals, bcx, vars, ctx);
}

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
            declare_gc_ptr(bcx, result);
            vec![result]
        },

        // MUTABILITY.md Stage 7 / RUNTIME.md: this is the shared funnel for
        // every read of a binding — a bind, a call argument, a return, a
        // struct/list-literal field, a `Widen`, a `Block`'s tail, a
        // `Conditional` branch flowing to its merge — any of which can
        // duplicate this binding's value into a new persistent home, so
        // `mark_shared_if_aliased` (see its own doc comment) flags a
        // `Copy`-classified `List` leaf as aliased, making a later write
        // through any path to it copy first. The exceptions are the handful
        // of *transient* consumers — `Index`/`Slice`/`FieldAccess`/
        // `IsVariant`/`VariantField`'s target, a loop's `iterable` — which
        // read a pointer only to address through it and store nothing;
        // those call `compile_expr_transient`/`compile_expr_multi_transient`
        // instead, which bypasses this arm via `read_var_raw` directly.
        //
        // That distinction mattered enormously when this arm cloned eagerly
        // (a scattered `xs[j]` read is `Copy`-classified on nearly every
        // occurrence, so cloning here turned an O(n) read pass into an
        // O(n^2) clone storm) and is merely tidy now that it only sets a
        // byte — but it stays, because marking a transiently-read list as
        // shared would make every later write to it copy for no reason.
        TypedExprKind::Var(name) => {
            let raw = read_var_raw(name, &expr.item.ty, bcx, vars, ctx.structs);
            mark_shared_if_aliased(expr, raw, bcx, ctx)
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

        TypedExprKind::Binary { op, left, right } => compile_binary(op, left, right, bcx, vars, ctx),

        TypedExprKind::Conditional { cond, true_branch, false_branch } =>
            compile_conditional(expr, cond, true_branch, false_branch, bcx, vars, ctx),

        TypedExprKind::Call { callable, args } => compile_call(callable, args, bcx, vars, ctx),

        TypedExprKind::Index { target, index } => {
            let list_val = compile_expr_transient(target, bcx, vars, ctx);
            let idx_val  = compile_expr(index, bcx, vars, ctx);

            let leafs = struct_fields(&expr.item.ty, ctx.structs);
            let get_id = ctx.func_ids["frog_list_get"];
            let mut results = Vec::with_capacity(leafs.len());
            for (i, (_, lty)) in leafs.iter().enumerate() {
                let callee = ctx.module.declare_func_in_func(get_id, bcx.func);
                let off_val = bcx.ins().iconst(types::I64, i as i64);
                let call = bcx.ins().call(callee, &[list_val, idx_val, off_val]);
                let raw = bcx.inst_results(call)[0];
                results.push(from_i64_repr(bcx, lty, raw));
            }
            // Root only once every leaf has been read: `frog_list_get`
            // can't collect, so there's no window to lose one in.
            // Which leaves are GC-scannable is a property of the column
            // (`is_heap_ty`), so `root_flat_leaves` needs nothing else.
            let leaf_tys: Vec<Type> = leafs.iter().map(|(_, t)| t.clone()).collect();
            declare_gc_leaves(bcx, &results, &leaf_tys);
            mark_shared_extracted(bcx, &expr.item.ty, &results, ctx.structs);
            results
        },

        TypedExprKind::Slice { target, start, end } => {
            let list_val = compile_expr_transient(target, bcx, vars, ctx);
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
            declare_gc_ptr(bcx, result);
            vec![result]
        },

        // A `Range` is two plain `i64` leaves (`start`, `end`) — the exact
        // shape `struct_fields` already produces for `Range<Int>`'s
        // registered 2-field layout (see `lower_range`/`RANGE_NAME`). No
        // `declare_gc_ptr` here, deliberately: `is_heap_ty` already returns
        // `false` for any `Named` type other than `Str`/`Union`/`List`, so
        // neither leaf is ever a pointer needing a GC root — unlike the
        // neighboring arms above, which do root their result.
        TypedExprKind::Range { start, end } => {
            let start_val = compile_expr(start, bcx, vars, ctx);
            let end_val   = compile_expr(end, bcx, vars, ctx);
            vec![start_val, end_val]
        },

        TypedExprKind::Assign { name, value } => {
            if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                vec![bcx.ins().iconst(types::I64, 0)]
            } else {
                let vals = compile_expr_multi(value, bcx, vars, ctx);
                let leafs = struct_fields(&value.item.ty, ctx.structs);
                for (v, (path, lty)) in vals.iter().zip(leafs.iter()) {
                    let key = var_key(name, path);
                    let var = get_or_declare_var(bcx, vars, &key, lty);
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

        TypedExprKind::List(elems) => compile_list_lit(&expr.item.ty, elems, bcx, vars, ctx),

        TypedExprKind::ForLoop { var, iterable, cond, body } => {
            compile_for_loop(var, iterable, cond, body, LoopOutput::Discard, bcx, vars, ctx);
            vec![bcx.ins().iconst(types::I64, 0)]
        },

        TypedExprKind::Comprehension { var, iterable, cond, body } => {
            let leafs = struct_fields(&body.item.ty, ctx.structs);
            let collect = LoopOutput::Collect {
                ptr_mask: gc_mask(leafs.iter().map(|(_, t)| t)),
                stride: leafs.len().max(1) as i64,
            };
            let result_list = compile_for_loop(var, iterable, cond, body, collect, bcx, vars, ctx)
                .expect("LoopOutput::Collect always yields a list");
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
                    let (offset, leaf_types) = enum_field_leaf_types(ename, None, field, ctx.structs, ctx.unions);
                    if let Type::Union(members) = target.item.ty.clone() {
                        if union_is_inline(&members, ctx.structs) {
                            // A *common* field sits at the same leaf index in
                            // every member (each member's flattened field list
                            // is the common fields followed by its own), and
                            // the pointer/scalar partition preserves order
                            // within each column, so its slot is the same
                            // whichever member is live. Unpacking against the
                            // first member is therefore enough — and needs no
                            // runtime tag test, exactly as the boxed path
                            // needs none.
                            let slots = compile_expr_multi_transient(target, bcx, vars, ctx);
                            let leaves = unpack_union_member(&members, &members[0], &slots, bcx, ctx.structs);
                            let out = leaves[offset..offset + leaf_types.len()].to_vec();
                            mark_shared_extracted(bcx, &expr.item.ty, &out, ctx.structs);
                            return out;
                        }
                    }
                    // A common field of a boxed union, read out of heap
                    // memory — unlike a struct's `Variable`-backed leaf,
                    // this is a fresh `Value` on every read, so each
                    // heap-typed slot roots itself (see
                    // `for_each_heap_producer`'s matching arm).
                    let ptr = compile_expr_transient(target, bcx, vars, ctx);
                    let out = read_variant_slots(ptr, offset, &leaf_types, bcx, ctx);
                    mark_shared_extracted(bcx, &expr.item.ty, &out, ctx.structs);
                    out
                },
                None => {
                    let target_vals = compile_expr_multi_transient(target, bcx, vars, ctx);
                    let (start, len) = field_slice_range(&target.item.ty, field, ctx.structs);
                    let out = target_vals[start..start + len].to_vec();
                    mark_shared_extracted(bcx, &expr.item.ty, &out, ctx.structs);
                    out
                },
            }
        },

        TypedExprKind::PlaceAssign { place, value } => {
            compile_place_assign(place, value, bcx, vars, ctx);
            vec![bcx.ins().iconst(types::I64, 0)]
        },

        TypedExprKind::VariantInit { fields, tag, enum_name, variant } =>
            compile_variant_init(&expr.item.ty, enum_name, variant, fields, *tag, bcx, vars, ctx),

        TypedExprKind::IsVariant { target, enum_name, variant, tag } => {
            if let Type::Union(members) = target.item.ty.clone() {
                if union_is_inline(&members, ctx.structs) {
                    let slots = compile_expr_multi_transient(target, bcx, vars, ctx);
                    let idx = nominal_member_index(&members, enum_name, variant);
                    return vec![emit_inline_tag_test(bcx, &slots, idx)];
                }
            }
            let val = compile_expr_transient(target, bcx, vars, ctx);
            let def = ctx.unions.get(enum_name).expect("known union in codegen").clone();
            vec![emit_is_variant(bcx, val, &def, *tag)]
        },

        TypedExprKind::VariantField { target, enum_name, variant, field } => {
            let (offset, leaf_types) = enum_field_leaf_types(enum_name, Some(variant), field, ctx.structs, ctx.unions);
            if let Type::Union(members) = target.item.ty.clone() {
                if union_is_inline(&members, ctx.structs) {
                    // Already guarded by a preceding `IsVariant`, so which
                    // member is live is known statically here.
                    let slots = compile_expr_multi_transient(target, bcx, vars, ctx);
                    let member_ty = Type::strukt(format!("{}.{}", enum_name, variant));
                    let leaves = unpack_union_member(&members, &member_ty, &slots, bcx, ctx.structs);
                    let out = leaves[offset..offset + leaf_types.len()].to_vec();
                    mark_shared_extracted(bcx, &expr.item.ty, &out, ctx.structs);
                    return out;
                }
            }
            let ptr = compile_expr_transient(target, bcx, vars, ctx);
            let out = read_variant_slots(ptr, offset, &leaf_types, bcx, ctx);
            mark_shared_extracted(bcx, &expr.item.ty, &out, ctx.structs);
            out
        },

        TypedExprKind::Return(value) => compile_return(value, bcx, vars, ctx),

        TypedExprKind::NoneLit => vec![bcx.ins().iconst(types::I64, gc::IMMEDIATE_NONE)],

        TypedExprKind::Widen { value, tag } => compile_widen(&expr.item.ty, value, *tag, bcx, vars, ctx),

        TypedExprKind::Narrow { value, .. } => compile_narrow(&expr.item.ty, value, bcx, vars, ctx),

        TypedExprKind::TypeTag { target, tag } => {
            let members = match &target.item.ty {
                Type::Union(members) => members.clone(),
                other => unreachable!("TypeTag target must be a union, got {}", other),
            };
            if union_is_inline(&members, ctx.structs) {
                // The tag is a plain register field (slot 0's low 3 bits) —
                // no load, no branch on representation, just mask and compare.
                let slots = compile_expr_multi(target, bcx, vars, ctx);
                vec![emit_inline_tag_test(bcx, &slots, *tag as usize)]
            } else {
                let val = compile_expr(target, bcx, vars, ctx);
                let target_is_immediate = members.get(*tag as usize) == Some(&Type::None);
                let any_immediate = members.contains(&Type::None);
                vec![emit_tag_test(bcx, val, target_is_immediate, any_immediate, *tag)]
            }
        },

        // Coerce a `Trait::Truthy` value into `Bool` for condition position
        // — see `TypeChecker::coerce_truthy`. Never reached with
        // `value.item.ty == Type::Bool` (that case is a no-op at lowering
        // and never wrapped in this node).
        // A non-lossy numeric promotion — the same `coerce_value` the
        // binary-operator path uses to bring two operands to a common type,
        // applied here at a declared-slot boundary instead. See
        // `TypedExprKind::Coerce`.
        TypedExprKind::Coerce(value) => {
            let v = compile_expr(value, bcx, vars, ctx);
            vec![coerce_value(v, &value.item.ty, &expr.item.ty, bcx)]
        },

        TypedExprKind::Truthy(value) => compile_truthy(value, bcx, vars, ctx),
    }
}

/// The structural comparisons all compute equality; `!=` is its negation.
fn negate_if_ne(op: &Token, eq: Value, bcx: &mut FunctionBuilder) -> Value {
    if *op == Token::NotEq {
        let one = bcx.ins().iconst(types::I8, 1);
        bcx.ins().bxor(eq, one)
    } else {
        eq
    }
}

fn compile_binary(op: &Token, left: &Spanned<TypedExpr>, right: &Spanned<TypedExpr>, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
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
                declare_gc_ptr(bcx, result);
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
            Token::In => {
                let id     = ctx.func_ids["frog_str_contains"];
                let callee = ctx.module.declare_func_in_func(id, bcx.func);
                let call   = bcx.ins().call(callee, &[lv, rv]);
                let result = bcx.inst_results(call)[0];
                bcx.ins().ireduce(types::I8, result)
            },
            _ => unimplemented!("string binary op {:?}", op),
        }];
    }

    // ── Range membership (`x in a..b`) — O(1) bounds check ───────────
    //
    // Unlike List's `in`, this needs no scan at all: a `Range`'s two leaves
    // (`start`, `end`) are already loaded as plain `i64`s by `compile_expr_multi`
    // (no pointer, so `_transient` vs. non-`_transient` is moot — see the
    // `TypedExprKind::Range` codegen arm), so membership is just two `icmp`s.
    if *op == Token::In && right.item.ty.is_range() {
        let lv = compile_expr(left, bcx, vars, ctx);
        let range_vals = compile_expr_multi(right, bcx, vars, ctx);
        let (start_val, end_val) = (range_vals[0], range_vals[1]);
        let ge_start = bcx.ins().icmp(IntCC::SignedGreaterThanOrEqual, lv, start_val);
        let lt_end   = bcx.ins().icmp(IntCC::SignedLessThan, lv, end_val);
        return vec![bcx.ins().band(ge_start, lt_end)];
    }

    // ── List membership (`x in xs`) ──────────────────────────────────
    //
    // Unlike `==`, the two operands have different types (an element and a
    // list), so this can't share `eq_list`'s "both operands are This List"
    // framing — it scans `right` comparing each element to `left` via
    // `eq_value` (`list_contains`), short-circuiting on the first hit.
    if *op == Token::In && right.item.ty.is_list() {
        let elem = right.item.ty.as_list_elem().expect("is_list implies an element type").clone();
        let lv = compile_expr_multi(left, bcx, vars, ctx);
        let elem_leaf_tys: Vec<Type> = struct_fields(&elem, ctx.structs).into_iter().map(|(_, t)| t).collect();
        declare_gc_leaves(bcx, &lv, &elem_leaf_tys);
        let rv = compile_expr_transient(right, bcx, vars, ctx);
        return vec![list_contains(&elem, &lv, rv, bcx, ctx)];
    }

    // ── List equality (structural — see `eq_list`) ──────────────────
    //
    // Reached both from a source-level `[1, 2] == [1, 2]` and from a
    // `List`-typed field of a struct comparison, whose per-field `Binary`
    // nodes `build_struct_eq` synthesizes with the field's own type.
    if left.item.ty.is_list() && matches!(op, Token::EqEq | Token::NotEq) {
        let elem = left.item.ty.as_list_elem().expect("is_list implies an element type").clone();
        // Transient on both sides: a structural comparison reads the two
        // lists and stores nothing, so neither operand is aliased by it.
        // Marking them made `if xs == ys` inside a loop that also pushes
        // quadratic — worse than `len`'s version of the same mistake, since
        // it marked two lists per comparison.
        let lv = compile_expr_transient(left,  bcx, vars, ctx);
        let rv = compile_expr_transient(right, bcx, vars, ctx);
        let eq = eq_list(&elem, lv, rv, bcx, ctx);
        return vec![negate_if_ne(op, eq, bcx)];
    }

    // ── Union equality (runtime tag dispatch — see `eq_union`) ──────
    //
    // `compile_expr_multi`, not `compile_expr`: an inline union occupies its
    // whole layout width, and reaching this through the scalar path below is
    // what used to abort codegen on a multi-leaf operand.
    if matches!(left.item.ty, Type::Union(_)) && matches!(op, Token::EqEq | Token::NotEq) {
        let members = match &left.item.ty { Type::Union(m) => m.clone(), _ => unreachable!() };
        let lv = compile_expr_multi(left,  bcx, vars, ctx);
        let rv = compile_expr_multi(right, bcx, vars, ctx);
        let eq = eq_union(&members, &lv, &rv, bcx, ctx);
        return vec![negate_if_ne(op, eq, bcx)];
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
            bcx.ins().brif(lv, rhs_bb, &[], merge_bb, &[BlockArg::from(zero)]);
        } else {
            // `true or right` == true, without evaluating `right`.
            let one = bcx.ins().iconst(types::I8, 1);
            bcx.ins().brif(lv, merge_bb, &[BlockArg::from(one)], rhs_bb, &[]);
        }

        bcx.switch_to_block(rhs_bb);
        bcx.seal_block(rhs_bb);
        let rv = compile_expr(right, bcx, vars, ctx);
        bcx.ins().jump(merge_bb, &[BlockArg::from(rv)]);

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
        Token::Slash => if is_float {
            // IEEE division never faults — x/0.0 is ±inf, 0.0/0.0 is
            // NaN — so no guard here, only on the integer path.
            bcx.ins().fdiv(lv, rv)
        } else {
            emit_int_div_guard(bcx, ctx, lv, rv);
            bcx.ins().sdiv(lv, rv)
        },
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
}

/// `TypedExprKind::Conditional`'s codegen — `expr` is the whole conditional
/// node (needed for `expr.item.ty`, both to pick the `Never`/scalar/
/// multi-leaf join shape and, in the multi-leaf case, to lay out
/// `merge_bb`'s block params via `struct_fields`).
fn compile_conditional(
    expr: &Spanned<TypedExpr>,
    cond: &Spanned<TypedExpr>,
    true_branch: &Spanned<TypedExpr>,
    false_branch: &Option<TypedExprRef>,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Vec<Value> {
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

        // NOTE, historical: an earlier shadow-stack representation gave
        // both branches of a `Conditional` overlapping root-slot ranges on
        // the argument that only one of them ever runs. That was unsound —
        // a value produced in one branch can escape (assigned to an outer
        // `mut` binding), and a loop back-edge can re-run the conditional
        // while it is still live. Cranelift's stack maps make the question
        // moot: root liveness is now real live-range analysis, not a
        // structural mutual-exclusion argument, so nothing here shares or
        // needs to share anything. See RUNTIME.md Part 2 and, for the bug
        // this replaced,
        // iteration k+1 taking the other branch, and the GC frees a value
        // the program still holds. Slots are therefore never reused: each
        // producer site owns one for the whole function, which meets the
        // invariant vacuously. Recovering the sharing needs real live
        // ranges (share iff non-interfering), not a mutual-exclusion
        // argument — see MUTABILITY.md stage 6.
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
        // (`build_func_body`'s own tail `return_`
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
    // Struct-typed *and* inline-union-typed results both need the
    // K-block-param merge below — see `is_multi_leaf_type`.
    let is_multi = is_multi_leaf_type(&expr.item.ty, ctx.structs);

    if !is_multi {
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
                bcx.ins().jump(merge_bb, &[BlockArg::from(tv)]);
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
                    bcx.ins().jump(merge_bb, &[BlockArg::from(fv)]);
                } else {
                    bcx.ins().jump(merge_bb, &[]);
                }
            }
        } else if has_value {
            let fv = bcx.ins().iconst(result_ty, 0);
            bcx.ins().jump(merge_bb, &[BlockArg::from(fv)]);
        } else {
            bcx.ins().jump(merge_bb, &[]);
        }
        bcx.switch_to_block(merge_bb);
        bcx.seal_block(merge_bb);

        if has_value {
            // A fresh SSA value carrying whichever branch's result arrived;
            // the branch values that fed it are dead here, so if this is a
            // GC-scannable column it needs declaring in its own right (see
            // `declare_gc_value`'s whole-program rule).
            let merged = bcx.block_params(merge_bb)[0];
            declare_gc_value(bcx, &expr.item.ty, merged);
            vec![merged]
        } else {
            vec![bcx.ins().iconst(types::I64, 0)]
        }
    } else {
        // ── multi-leaf path: K block params, one per leaf field
        // (`struct_fields`). Struct unification is nominal/exact,
        // so both branches' leaf types are identical to expr's own;
        // an inline union's branches instead each carry their own
        // `Widen`, inserted by typeck at the join (see
        // `TypeChecker::lower_widen`), so this is still just value
        // plumbing — no new rooting decision happens here.
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
            bcx.ins().jump(merge_bb, &block_args(&tv));
        }
        bcx.switch_to_block(false_bb);
        bcx.seal_block(false_bb);
        match false_branch {
            Some(fb) => {
                let fv = compile_expr_multi(fb, bcx, vars, ctx);
                if fb.item.ty != Type::Never {
                    bcx.ins().jump(merge_bb, &block_args(&fv));
                }
            }
            None => {
                let fv: Vec<Value> = param_tys.iter().map(|&t| placeholder_value(bcx, t)).collect();
                bcx.ins().jump(merge_bb, &block_args(&fv));
            }
        };
        bcx.switch_to_block(merge_bb);
        bcx.seal_block(merge_bb);
        // The merge parameters are fresh SSA values carrying whichever
        // branch's leaves arrived, so each GC-scannable column among them
        // needs declaring in its own right — the branch values that fed
        // them are dead here (see `declare_gc_value`'s whole-program rule).
        let merged = bcx.block_params(merge_bb).to_vec();
        let leaf_tys: Vec<Type> = leafs.iter().map(|(_, t)| t.clone()).collect();
        declare_gc_leaves(bcx, &merged, &leaf_tys);
        merged
    }
}

fn compile_call(callable: &Spanned<TypedExpr>, args: &[Arg], bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let func_name = match &callable.item.kind {
        TypedExprKind::Var(name) => name.clone(),
        _ => panic!("only named function calls supported in codegen"),
    };

    // `push(mut place, v)` — a mutating builtin. It needs no node of its
    // own: its receiver is an ordinary `Arg::Mut`, so all that is special
    // here is what to *do* with the resolved list. The next mutating
    // builtin (`pop`, `insert`, a `Map` operation) is another arm here and
    // nothing else. See `typed_ast::Arg`.
    if func_name == "push" {
        let (Arg::Mut(place), Some(v_arg)) = (&args[0], args[1].value()) else {
            unreachable!("typeck's `finish_push` builds push's receiver as `Arg::Mut`")
        };
        let list_val = emit_mutable_list(place, bcx, vars, ctx);
        let leafs = struct_fields(&v_arg.item.ty, ctx.structs);
        let vvals = compile_expr_multi(v_arg, bcx, vars, ctx);
        // See `compile_list_lit`'s identical push via `push_element`.
        push_element(bcx, ctx, list_val, &vvals, &leafs);
        return vec![bcx.ins().iconst(types::I64, 0)];
    }

    // Everything below reads arguments as values. `print` and the host/user
    // call paths that follow take no `mut` place except through the
    // copy-out at the very end, which resolves them itself.
    let arg_exprs: Vec<&Spanned<TypedExpr>> = args.iter().filter_map(Arg::value).collect();

    // `print` accepts values of any type.  Its runtime entry
    // point is selected here, after type checking has established the
    // concrete argument type, so no invalid Str coercion is emitted.
    if func_name == "print" {
        let arg = arg_exprs[0];
        if arg.item.ty == Type::Never {
            // `print`'s builtin signature has no declared param
            // type, so typeck doesn't reject a `Never` argument
            // (e.g. `print(panic("x"))`) the way it would for an
            // ordinary function call. Compile the arg for its
            // trapping side effect via `compile_expr_multi` (not
            // `compile_expr`, which asserts against multi-valued/
            // zero-valued exprs) and propagate `Never` outward.
            let vals = compile_expr_multi(arg, bcx, vars, ctx);
            debug_assert!(vals.is_empty(), "Never-typed expr produced values");
            return Vec::new();
        }
        // Structs, unions and `none` all go through the same type-directed
        // walk as a nested field would — `print_value`'s `Union` arm does
        // the runtime tag dispatch itself, so the top level needs no
        // separate copy of it, and routing `None` here is what makes
        // `print(none)` work instead of falling into the scalar match's
        // panic below.
        if arg.item.ty.is_struct() || matches!(arg.item.ty, Type::Union(_) | Type::None) {
            // Transient: printing walks the value and stores nothing, so no
            // alias survives this — see the `List` case just below for what
            // marking it here would cost.
            let values = compile_expr_multi_transient(arg, bcx, vars, ctx);
            let mut cursor = 0;
            print_value(&arg.item.ty, &values, &mut cursor, bcx, ctx);
            print_fragment("\n", bcx, ctx);
            return vec![bcx.ins().iconst(types::I64, 0)];
        }
        if let Some(inner) = arg.item.ty.as_list_elem().cloned() {
            // Same type-directed walk the struct case above uses: the
            // element loop is emitted here, not delegated to a runtime
            // function that has lost `inner` (plans/DATA.md stage 0).
            // Transient, and this is the case where it matters:
            // `print(xs)` creates no alias, so marking `xs` shared here
            // would make the *next* `push(mut xs, ..)` copy the whole list
            // — and re-mark, and re-copy, turning an O(n) loop that prints
            // as it goes into an O(n^2) one. Same reasoning as `Index`'s.
            let list_val = compile_expr_transient(arg, bcx, vars, ctx);
            print_list(&inner, list_val, bcx, ctx);
            print_fragment("\n", bcx, ctx);
            return vec![bcx.ins().iconst(types::I64, 0)];
        }
        let arg_val = compile_expr(arg, bcx, vars, ctx);
        let rt_name = match &arg.item.ty {
            Type::Str => "print",
            Type::Int => "frog_int_println",
            Type::Float => "frog_float_println",
            Type::Bool => "frog_bool_println",
            ty => panic!("print codegen does not support {:?}", ty),
        };
        let func_id = ctx.func_ids[rt_name];
        let callee = ctx.module.declare_func_in_func(func_id, bcx.func);
        bcx.ins().call(callee, &[arg_val]);
        return vec![bcx.ins().iconst(types::I64, 0)];
    }

    // `len(xs)` / `xs.len()` — `List<T>`/`Str`, polymorphic over `T` per
    // typeck's `finish_len`. No `func_ids` entry backs "len" either, so
    // this is dispatched on the argument's concrete type the same way
    // `print` dispatches on its own — reusing `frog_list_len`/`frog_str_len`,
    // which already exist as internal runtime primitives for indexing and
    // iteration (`compile_index`, comprehension lowering).
    if func_name == "len" {
        let arg = arg_exprs[0];
        // Transient: `len` reads the header and discards the pointer, so it
        // creates no alias. Marking here made `for i in .. { xs.len(); push(mut xs, i) }`
        // — an entirely ordinary loop — quadratic, since every iteration
        // re-shared the list and the following push copied it.
        let arg_val = compile_expr_transient(arg, bcx, vars, ctx);
        let rt_name = if arg.item.ty.is_list() {
            "frog_list_len"
        } else {
            match &arg.item.ty {
                Type::Str => "frog_str_len",
                ty => panic!("len codegen does not support {:?}", ty),
            }
        };
        let func_id = ctx.func_ids[rt_name];
        let callee = ctx.module.declare_func_in_func(func_id, bcx.func);
        let call = bcx.ins().call(callee, &[arg_val]);
        return vec![bcx.inst_results(call)[0]];
    }

    // `to_list(range)` — the explicit `Range<T> -> List<T>` materialization
    // (see typeck's `finish_to_list`). This is exactly what the old
    // `TypedExprKind::Range` codegen arm used to do unconditionally for
    // every range value — `frog_range` still exists as a runtime primitive,
    // now reached only here, opt-in.
    if func_name == "to_list" {
        let arg = arg_exprs[0];
        let range_vals = compile_expr_multi(arg, bcx, vars, ctx);
        let (start_val, end_val) = (range_vals[0], range_vals[1]);
        let id     = ctx.func_ids["frog_range"];
        let callee = ctx.module.declare_func_in_func(id, bcx.func);
        let call   = bcx.ins().call(callee, &[start_val, end_val]);
        let result = bcx.inst_results(call)[0];
        declare_gc_ptr(bcx, result);
        return vec![result];
    }

    // A registered host function (`FrogStateBuilder::func`,
    // `plans/EMBEDDING.md`) — every one shares the uniform
    // `extern "C" fn(ctx, args, out)` shim signature regardless of its frog
    // type, so it's called through a stack-slot arg/out buffer instead of a
    // native Cranelift call. See "The uniform shim ABI" in the design doc.
    if ctx.host_fns.contains(&func_name) {
        return compile_host_call(&func_name, callable, &arg_exprs, bcx, vars, ctx);
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

    // A `mut` argument is passed by loading its place; the copy-out below
    // writes the callee's final value back into that same place.
    let mut arg_places: Vec<(PlaceRef, Type)> = Vec::new();
    let mut arg_vals: Vec<Value> = Vec::with_capacity(args.len());
    for (i, arg) in args.iter().enumerate() {
        let a = match arg {
            Arg::Value(e) => e,
            Arg::Mut(place) => {
                let param_ty = param_types.get(i).cloned().unwrap_or(Type::Int);
                let pref = emit_place_ref(place, bcx, vars, ctx);
                arg_vals.extend(place_load(&pref, &param_ty, bcx, vars, ctx));
                arg_places.push((pref, param_ty));
                continue;
            },
        };
        if is_multi_leaf_type(&a.item.ty, ctx.structs) {
            // Struct args are never widened (nominal/exact match),
            // and an inline-union arg is already the exact target
            // union type by the time codegen sees it (widening
            // happens earlier, at the `Widen` node itself) — either
            // way, flatten straight into the call's arg list, in
            // the same K-`AbiParam`-per-arg order `make_sig` uses.
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
    // Every `mut`-marked argument's final value follows the primary
    // return, one contiguous group per argument in call order, sized by
    // that argument's own flattened leaf count — `make_sig`/
    // `build_func_body` on the callee side append exactly these leaves,
    // in this order, after its own declared return (`Function`'s doc
    // comment in `typed_ast.rs`). Split them off before handling the
    // primary return so the three branches below don't need to know
    // about `mut` arguments at all.
    let all_results = bcx.inst_results(call).to_vec();
    let primary_leaf_count = if return_ty == Type::None { 0 } else { struct_fields(&return_ty, ctx.structs).len() };
    let (primary_raw, mut copyout_raw) = all_results.split_at(primary_leaf_count);

    let primary_results = if return_ty == Type::None {
        vec![bcx.ins().iconst(types::I64, 0)]
    } else if is_multi_leaf_type(&return_ty, ctx.structs) {
        // Each GC-scannable leaf of a struct return, or of an
        // inline union return, crosses the ABI boundary as a bare
        // register value, a fresh SSA value on this side of the call
        // that needs declaring in its own right, exactly like the
        // scalar Str/List case below. Which leaves those are is static
        // (`is_heap_ty` on the column), and the collector screens
        // each word with `gc::is_heap_ptr` itself, so a tag-only
        // inline-union word roots harmlessly.
        let leaf_tys: Vec<Type> = struct_fields(&return_ty, ctx.structs)
            .into_iter().map(|(_, t)| t).collect();
        declare_gc_leaves(bcx, primary_raw, &leaf_tys);
        primary_raw.to_vec()
    } else {
        let result = primary_raw[0];
        if is_heap_ty(&return_ty) {
            declare_gc_ptr(bcx, result);
        }
        vec![result]
    };

    // Copy-out: root each `mut` argument's returned leaves exactly like the
    // primary return above, then store them back into the place the
    // argument named. `emit_place_ref` already walked and unshared that
    // path on the way in, so the parent chain is private and this store is
    // reachable from the root — which is what lets a `mut` argument be
    // `f(mut b.items)` and not just `f(mut xs)`.
    for (pref, ty) in &arg_places {
        let leafs = struct_fields(ty, ctx.structs);
        let (this_arg, rest) = copyout_raw.split_at(leafs.len());
        copyout_raw = rest;
        let leaf_tys: Vec<Type> = leafs.iter().map(|(_, t)| t.clone()).collect();
        declare_gc_leaves(bcx, this_arg, &leaf_tys);
        place_store(pref, ty, this_arg, bcx, vars, ctx);
    }

    primary_results
}

/// Call a registered host function through the uniform shim ABI: flatten
/// every argument's leaves into a stack-allocated `args` buffer (raw i64
/// wire format — `to_i64_repr`, the same convention `FrogList`'s data
/// buffer and `__frog_main`'s `out_ptr` already use), call the shim as
/// `shim(frog_ctx_current(), &args, &out)`, then load `out`'s leaves back.
///
/// This is the codebase's first use of Cranelift stack slots — the shadow
/// frame's were deleted when GC roots moved to stack maps (RUNTIME.md Part
/// 2) — because a host shim's `extern "C"` signature can't itself return
/// more than one value (needed for a struct/union result) or accept
/// `Bool`/`Float` in their native Cranelift types (`cl_type` gives them
/// `I8`/`F64`, not `I64`). Neither buffer is covered by *this* function's
/// own stack map — a raw stack slot isn't a `Value` — which is exactly why
/// the shim on the other side must `RuntimeRoots::hold` every argument
/// slot itself (`plans/EMBEDDING.md`, "GC safety").
fn compile_host_call(func_name: &str, callable: &Spanned<TypedExpr>, args: &[&Spanned<TypedExpr>], bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let return_ty: Type = match &callable.item.ty {
        Type::Function { result, .. } => *result.clone(),
        _ => Type::Int,
    };

    // Flatten every argument into (wire-format value, leaf type) pairs, in
    // the same order `struct_fields` would enumerate them — matching
    // `make_sig`'s own per-arg flattening convention.
    let mut arg_leaves: Vec<(Value, Type)> = Vec::new();
    for a in args {
        if is_multi_leaf_type(&a.item.ty, ctx.structs) {
            let leafs = struct_fields(&a.item.ty, ctx.structs);
            let vals = compile_expr_multi(a, bcx, vars, ctx);
            for (v, (_, lty)) in vals.iter().zip(leafs.iter()) {
                arg_leaves.push((to_i64_repr(bcx, lty, *v), lty.clone()));
            }
        } else {
            let v = compile_expr(a, bcx, vars, ctx);
            arg_leaves.push((to_i64_repr(bcx, &a.item.ty, v), a.item.ty.clone()));
        }
    }

    let arg_slots = arg_leaves.len().max(1);
    let args_ss = bcx.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot, (arg_slots * 8) as u32, 3,
    ));
    for (i, (v, _)) in arg_leaves.iter().enumerate() {
        bcx.ins().stack_store(types::I64, *v, args_ss, (i * 8) as i32);
    }
    let args_addr = bcx.ins().stack_addr(types::I64, args_ss, 0);

    let ret_leaf_tys: Vec<Type> = struct_fields(&return_ty, ctx.structs).into_iter().map(|(_, t)| t).collect();
    let out_slots = ret_leaf_tys.len().max(1);
    let out_ss = bcx.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot, (out_slots * 8) as u32, 3,
    ));
    let out_addr = bcx.ins().stack_addr(types::I64, out_ss, 0);

    let ctx_id = ctx.func_ids["frog_ctx_current"];
    let ctx_callee = ctx.module.declare_func_in_func(ctx_id, bcx.func);
    let ctx_call = bcx.ins().call(ctx_callee, &[]);
    let ctx_val = bcx.inst_results(ctx_call)[0];

    let func_id = ctx.func_ids[func_name];
    let local_callee = ctx.module.declare_func_in_func(func_id, bcx.func);
    bcx.ins().call(local_callee, &[ctx_val, args_addr, out_addr]);

    if return_ty == Type::None {
        return vec![bcx.ins().iconst(types::I64, 0)];
    }
    let mut results = Vec::with_capacity(ret_leaf_tys.len());
    for (i, lty) in ret_leaf_tys.iter().enumerate() {
        let raw = bcx.ins().stack_load(types::I64, types::I64, out_ss, (i * 8) as i32);
        results.push(from_i64_repr(bcx, lty, raw));
    }
    declare_gc_leaves(bcx, &results, &ret_leaf_tys);
    results
}

fn compile_list_lit(list_ty: &Type, elems: &[Spanned<TypedExpr>], bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let elem_ty = list_ty.as_list_elem().cloned().unwrap_or(Type::Int);
    let leafs = struct_fields(&elem_ty, ctx.structs);
    let ptr_mask = gc_mask(leafs.iter().map(|(_, t)| t));
    let stride = (leafs.len().max(1)) as i64;

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
    declare_gc_ptr(bcx, list_ptr);

    for elem in elems {
        // A struct element compiles to `leafs.len()` values, pushed
        // back-to-back — matching `stride` exactly is what makes the
        // list's flat backing store self-describing (see FrogList's
        // doc comment in runtime/gc.rs).
        let evs = compile_expr_multi(elem, bcx, vars, ctx);
        // The list's backing store is a flat i64 buffer (see FrogList in
        // runtime/gc.rs); Float and Bool elements need the same
        // bitcast/zero-extend conversion applied to every other
        // i64-wire-format value (see to_i64_repr and push_element).
        push_element(bcx, ctx, list_ptr, &evs, &leafs);
    }

    vec![list_ptr]
}

fn compile_variant_init(
    union_ty: &Type,
    enum_name: &str,
    variant: &str,
    fields: &[(String, TypedExprRef)],
    tag: u32,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Vec<Value> {
    // `fields` is the enum's common fields followed by this variant's own
    // (see `check_and_lower`'s variant-call arm), i.e. exactly
    // `struct_fields(Type::strukt("Enum.Variant"))`'s order.
    let mut flat_vals: Vec<Value> = Vec::new();
    let mut flat_types: Vec<Type> = Vec::new();
    for (_, v) in fields {
        flat_vals.extend(compile_expr_multi(v, bcx, vars, ctx));
        flat_types.extend(struct_fields(&v.item.ty, ctx.structs).into_iter().map(|(_, t)| t));
    }

    if let Type::Union(members) = union_ty {
        if union_is_inline(members, ctx.structs) {
            // No heap object: the variant's fields go straight into the
            // union's columns, tagged in slot 0. This is what unboxes
            // `data Discount is NoDiscount | Percent(pct: Int) | ...` —
            // `roadmap.md`'s top perf item.
            let idx = nominal_member_index(members, enum_name, variant);
            let member_ty = Type::strukt(format!("{}.{}", enum_name, variant));
            return pack_union_member(members, &member_ty, member_tag(idx), &flat_vals, bcx, ctx.structs);
        }
    }

    // A boxed union's variant with no fields at all — neither its own nor
    // common ones its enum declares — carries no information beyond its
    // tag, so it needs no heap object: emit the tag as an immediate. See
    // gc.rs's "Word encoding" for why the GC can tell the two apart.
    if fields.is_empty() {
        return vec![bcx.ins().iconst(types::I64, gc::immediate_variant(tag))];
    }
    vec![box_into_variant(tag, &flat_vals, &flat_types, bcx, ctx)]
}

fn compile_return(value: &Option<TypedExprRef>, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let mut results = match value {
        Some(v) => compile_expr_multi(v, bcx, vars, ctx),
        None => Vec::new(),
    };
    // This is an *early* exit, so it must do the same thing
    // `build_func_body`'s own tail `return_` does, not skip it: carry
    // each `mut` parameter's current
    // value, exactly like the tail return does — an early `return` is
    // just as much an exit as falling off the end of the body.
    results.extend(mut_param_copyout(bcx, vars, ctx));
    bcx.ins().return_(&results);
    // Cranelift requires every block to end in exactly one
    // terminator, and `return_` is one — so whatever IR follows
    // this `Return` in the source (there is always some: it sits
    // inside a `Block`/`Conditional` whose caller keeps building)
    // needs a fresh block to land in. Nothing ever jumps to it —
    // the branch/block that contains an unconditional `return`
    // has `Type::Never`, and the `Conditional` join checks
    // for exactly that to skip emitting the jump — so this block
    // is genuinely unreachable, which Cranelift's verifier permits
    // as long as it's syntactically well-formed.
    let dead = bcx.create_block();
    bcx.switch_to_block(dead);
    bcx.seal_block(dead);
    Vec::new()
}

/// Coerce `value` (a strict, narrower member type) up into an anonymous
/// union carrying `tag` — see `TypedExprKind::Widen`. `tag` is `value`'s
/// index in the union's normalized member list.
fn compile_widen(union_ty: &Type, value: &Spanned<TypedExpr>, tag: u32, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let members = match union_ty {
        Type::Union(members) => members.clone(),
        other => unreachable!("Widen target must be a union, got {}", other),
    };

    if union_is_inline(&members, ctx.structs) {
        // No allocation at all: the value's own leaves are written into the
        // union's columns and the member tag rides in slot 0. This is what
        // retires `roadmap.md`'s top perf item — an `Int` widened into
        // `Int | PricingError` no longer round-trips through the heap.
        let vals = compile_expr_multi(value, bcx, vars, ctx);
        // A payload-less member contributes no leaves; `value` still
        // compiles (for its side effects, and because `None`'s own
        // single-slot form is not this union's representation of it).
        let leaf_vals: &[Value] = if matches!(value.item.ty, Type::None | Type::Never) { &[] } else { &vals };
        return pack_union_member(&members, &value.item.ty, member_tag(tag as usize), leaf_vals, bcx, ctx.structs);
    }

    // Boxed union (`union_is_inline` is false — too many members, or
    // self-referential). `tag` is the boxed representation's tag directly:
    // a payload-less member is an immediate, everything else a `FrogVariant`.
    if value.item.ty == Type::None {
        // `None`'s own compiled form (`gc::IMMEDIATE_NONE`) isn't reused
        // directly: this union's own sorted member list may place `None` at
        // a different tag than the standalone unit value's encoding, so it
        // is re-encoded with *this* union's tag. Still compile `value`
        // first for any side effects (none today, but `Widen` shouldn't
        // assume that).
        let _ = compile_expr_multi(value, bcx, vars, ctx);
        return vec![bcx.ins().iconst(types::I64, gc::immediate_variant(tag))];
    }
    let flat_vals = compile_expr_multi(value, bcx, vars, ctx);
    let flat_types: Vec<Type> = struct_fields(&value.item.ty, ctx.structs).into_iter().map(|(_, t)| t).collect();
    vec![box_into_variant(tag, &flat_vals, &flat_types, bcx, ctx)]
}

/// The inverse of `compile_widen`: unbox `value` (a union-typed expression,
/// already known — from a preceding `TypeTag` test — to currently hold
/// `target_ty`) back out as a plain value of that type.
fn compile_narrow(target_ty: &Type, value: &Spanned<TypedExpr>, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let members = match &value.item.ty {
        Type::Union(members) => members.clone(),
        other => unreachable!("Narrow source must be a union, got {}", other),
    };

    // `value`'s own leaves are read through, not duplicated: `Narrow`
    // returns the matched member's *own* fields (a fresh extraction,
    // handled — or not — by whatever binds them next, same as
    // `FieldAccess`/`VariantField`), never `value`'s raw pointer itself. A
    // `match` commonly re-reads its scrutinee this way from inside a loop,
    // so this must stay transient for the same reason `Index`'s target does
    // — see `compile_expr_transient`.
    if union_is_inline(&members, ctx.structs) {
        let slots = compile_expr_multi_transient(value, bcx, vars, ctx);
        if matches!(target_ty, Type::None | Type::Never) {
            // Nothing to read — the member carries no information beyond
            // its tag, already proven by the preceding `TypeTag`. One dummy
            // slot keeps arity consistent with `struct_fields`'s generic
            // one-leaf fallback for a non-struct type.
            return vec![bcx.ins().iconst(types::I64, 0)];
        }
        return unpack_union_member(&members, target_ty, &slots, bcx, ctx.structs);
    }

    if *target_ty == Type::None {
        // `value` here is an immediate, not a pointer.
        let _ = compile_expr_transient(value, bcx, vars, ctx);
        return vec![bcx.ins().iconst(types::I64, 0)];
    }
    let ptr = compile_expr_transient(value, bcx, vars, ctx);
    let leaf_types: Vec<Type> = struct_fields(target_ty, ctx.structs).into_iter().map(|(_, t)| t).collect();
    read_variant_slots(ptr, 0, &leaf_types, bcx, ctx)
}

fn compile_truthy(value: &Spanned<TypedExpr>, bcx: &mut FunctionBuilder, vars: &mut HashMap<String, Variable>, ctx: &mut Ctx) -> Vec<Value> {
    let v = compile_expr(value, bcx, vars, ctx);
    if value.item.ty.is_list() {
        let id     = ctx.func_ids["frog_list_len"];
        let callee = ctx.module.declare_func_in_func(id, bcx.func);
        let call   = bcx.ins().call(callee, &[v]);
        let len    = bcx.inst_results(call)[0];
        let zero   = bcx.ins().iconst(types::I64, 0);
        let truthy = bcx.ins().icmp(IntCC::NotEqual, len, zero);
        return vec![truthy];
    }
    let truthy = match &value.item.ty {
        Type::Int => {
            let zero = bcx.ins().iconst(types::I64, 0);
            bcx.ins().icmp(IntCC::NotEqual, v, zero)
        },
        Type::Float => {
            let zero = bcx.ins().f64const(0.0);
            bcx.ins().fcmp(FloatCC::NotEqual, v, zero)
        },
        // `None` is the immediate `1`, always falsey.
        Type::None => bcx.ins().iconst(types::I8, 0),
        Type::Str => {
            let id     = ctx.func_ids["frog_str_len"];
            let callee = ctx.module.declare_func_in_func(id, bcx.func);
            let call   = bcx.ins().call(callee, &[v]);
            let len    = bcx.inst_results(call)[0];
            let zero   = bcx.ins().iconst(types::I64, 0);
            bcx.ins().icmp(IntCC::NotEqual, len, zero)
        },
        other => unreachable!("Truthy on non-Truthy type {}", other),
    };
    vec![truthy]
}

/// Read `leaf_types.len()` consecutive payload slots starting at `offset`
/// out of the `FrogVariant` at `ptr`, converting each back from its
/// `i64`-wire representation and declaring each GC-scannable one to
/// Cranelift — a freshly-loaded pointer is a new SSA value, and `ptr`
/// itself may die before it does.
fn read_variant_slots(ptr: Value, offset: usize, leaf_types: &[Type], bcx: &mut FunctionBuilder, _ctx: &mut Ctx) -> Vec<Value> {
    let mut out = Vec::with_capacity(leaf_types.len());
    for (i, lty) in leaf_types.iter().enumerate() {
        // Only a variant that has payload slots to read is ever boxed, so
        // `ptr` here is always a real pointer, never an unboxed immediate.
        let raw = bcx.ins().load(types::I64, heap_mem(), ptr, variant_slot_offset(offset + i));
        out.push(from_i64_repr(bcx, lty, raw));
    }
    declare_gc_leaves(bcx, &out, leaf_types);
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
    let name = struct_ty.as_struct_name()
        .unwrap_or_else(|| unreachable!("field_slice_range called on non-struct type {:?}", struct_ty));
    let decl_fields = structs.get(struct_ty).cloned().unwrap_or_default();
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

/// The multi-level generalization of `field_slice_range`: the `(offset,
/// len)` range, within `ty`'s own `struct_fields` flattening, occupied by
/// the leaves under dotted path `dotted` (`"i.v"`, matching a nested
/// struct field, not just an immediate one). Used by `PlaceAssign`
/// codegen to find where a suffix field path (`xs[0].f = v`,
/// `xs[0].i.v = v`) lands within the indexed element's own layout —
/// `field_slice_range` itself isn't reusable there since it only compares
/// against one struct's *immediate* declared field names, not a dotted
/// path composed across several `.field` segments.
fn dotted_leaf_range(ty: &Type, dotted: &str, structs: &StructDefs) -> (usize, usize) {
    let leafs = struct_fields(ty, structs);
    let mut start = None;
    let mut count = 0;
    for (i, (path, _)) in leafs.iter().enumerate() {
        let matches = path == dotted || path.starts_with(&format!("{}.", dotted));
        if matches {
            if start.is_none() { start = Some(i); }
            count += 1;
        } else if start.is_some() {
            break;
        }
    }
    (
        start.unwrap_or_else(|| panic!("dotted field path '{}' not found on {:?} in codegen", dotted, ty)),
        count,
    )
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
    output: LoopOutput,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Option<Value> {
    // A `Range` iterable needs none of this function's list machinery
    // (`frog_list_len`, stride, slot addressing) — it's a plain counting
    // loop over two already-loaded `i64`s. Dispatch to a dedicated sibling
    // rather than threading a List/Range distinction through every step
    // below, most of which (stride/slot arithmetic, GC rooting of the
    // element) is genuinely List-specific and doesn't apply.
    if iterable.item.ty.is_range() {
        return compile_for_loop_range(var, iterable, cond, body, output, bcx, vars, ctx);
    }

    // Read-through, not a duplication: the loop walks this pointer directly
    // by index and never stores it into a new binding. See
    // `compile_expr_transient`.
    let list_val = compile_expr_transient(iterable, bcx, vars, ctx);
    let elem_ty = iterable.item.ty.as_list_elem().cloned()
        .unwrap_or_else(|| unreachable!("for-loop iterable must be a List after type checking, got {}", iterable.item.ty));
    let elem_leafs = struct_fields(&elem_ty, ctx.structs);

    let len_id = ctx.func_ids["frog_list_len"];
    let len_callee = ctx.module.declare_func_in_func(len_id, bcx.func);
    let len_call = bcx.ins().call(len_callee, &[list_val]);
    let len_val = bcx.inst_results(len_call)[0];
    let stride_val = list_stride(bcx, list_val);

    // A comprehension's result list is allocated here rather than at the
    // `Comprehension` arm, so it can be sized from the iterable's length —
    // which is only known once `frog_list_len` has run.
    //
    // The loop pushes at most one element per iteration, so `len_val` is an
    // exact capacity when there is no filter and an upper bound when there
    // is. Starting at 1 and doubling instead (which is what this did) meant
    // a `realloc` plus a full `memmove` of the buffer at every power of two
    // — `log2(n)` of them per comprehension, and that copying dominated
    // `benches/orders.frog`, whose inner loop rebuilds a ~1300-element list
    // 2000 times.
    //
    // Over-allocating on a selective filter is bounded by the iterable's
    // own element count, so the transient waste is never worse than a
    // second copy of a list the program already has materialized — a range
    // iterable included, since `frog_range` builds a real list too.
    let result_list = match output {
        LoopOutput::Discard => None,
        LoopOutput::Collect { stride, ptr_mask } => {
            let stride_arg = bcx.ins().iconst(types::I64, stride);
            let mask_arg   = bcx.ins().iconst(types::I64, ptr_mask);
            let alloc_id = ctx.func_ids["frog_alloc_list"];
            let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[len_val, stride_arg, mask_arg]);
            let list = bcx.inst_results(alloc_call)[0];
            // Root the result list before the loop runs at all: it must
            // already be reachable by the time the first pushed element
            // (or the iterable itself) can trigger a collection.
            declare_gc_ptr(bcx, list);
            Some(list)
        }
    };

    let header_bb = bcx.create_block();
    let body_bb   = bcx.create_block();
    let exit_bb   = bcx.create_block();
    bcx.append_block_param(header_bb, types::I64);

    let zero = bcx.ins().iconst(types::I64, 0);
    bcx.ins().jump(header_bb, &[BlockArg::from(zero)]);

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
    //
    // One address computation for the whole element rather than one per
    // leaf: an element's slots are contiguous, and nothing between these
    // loads can reallocate the buffer (they all happen before the body
    // runs), so `data` is loaded once and each leaf is a constant offset
    // off it. `list_slot_addr` would reload `data` and redo the index
    // arithmetic per leaf — four times over for a 4-leaf `Item`.
    let base_slot = bcx.ins().imul(i, stride_val);
    let base_addr = list_slot_addr(bcx, list_val, base_slot);
    let mut elem_vals = Vec::with_capacity(elem_leafs.len());
    for (leaf_idx, (_, lty)) in elem_leafs.iter().enumerate() {
        let raw = bcx.ins().load(types::I64, heap_mem(), base_addr, (leaf_idx * 8) as i32);
        elem_vals.push(from_i64_repr(bcx, lty, raw));
    }
    // Root every leaf before binding any of them: the loads above can't
    // collect, so there's no window to lose one in. Which leaves are
    // GC-scannable is a property of the column (`is_heap_ty`).
    let elem_leaf_tys: Vec<Type> = elem_leafs.iter().map(|(_, t)| t.clone()).collect();
    declare_gc_leaves(bcx, &elem_vals, &elem_leaf_tys);
    // The list keeps its own path to every element, so a `List`-typed element
    // is aliased the moment it is bound — see `mark_shared_extracted`.
    mark_shared_extracted(bcx, &elem_ty, &elem_vals, ctx.structs);
    for ((leaf_path, lty), elem_val) in elem_leafs.iter().zip(elem_vals) {
        let key = var_key(var, leaf_path);
        let var_id = get_or_declare_var(bcx, vars, &key, lty);
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
        let i_next = bcx.ins().iadd_imm_s(i, 1);
        bcx.ins().jump(header_bb, &[BlockArg::from(i_next)]);

        bcx.switch_to_block(do_bb);
        bcx.seal_block(do_bb);
    }

    let body_vals = compile_expr_multi(body, bcx, vars, ctx);
    if let Some(list_ptr) = result_list {
        let body_leafs = struct_fields(&body.item.ty, ctx.structs);
        push_element(bcx, ctx, list_ptr, &body_vals, &body_leafs);
    }

    let i_next = bcx.ins().iadd_imm_s(i, 1);
    bcx.ins().jump(header_bb, &[BlockArg::from(i_next)]);
    bcx.seal_block(header_bb);

    bcx.switch_to_block(exit_bb);
    bcx.seal_block(exit_bb);
    result_list
}

/// `compile_for_loop`'s Range case — a plain counting loop over two
/// already-loaded `i64`s (`start`, `end`), with none of the List version's
/// stride/slot arithmetic, `frog_list_len` call, or per-element GC rooting
/// (a range's element is always `Int`, never heap-scannable). The block
/// param carries the *current element value* directly (not a separate
/// `0..len` index), so advancing the loop and binding the loop variable are
/// the same value — no offset/index indirection at all.
#[allow(clippy::too_many_arguments)]
fn compile_for_loop_range(
    var: &str,
    iterable: &Spanned<TypedExpr>,
    cond: &Option<Box<Spanned<TypedExpr>>>,
    body: &Spanned<TypedExpr>,
    output: LoopOutput,
    bcx: &mut FunctionBuilder,
    vars: &mut HashMap<String, Variable>,
    ctx: &mut Ctx,
) -> Option<Value> {
    let range_vals = compile_expr_multi(iterable, bcx, vars, ctx);
    let (start_val, end_val) = (range_vals[0], range_vals[1]);
    let elem_ty = iterable.item.ty.as_range_elem().cloned()
        .unwrap_or_else(|| unreachable!("for-loop iterable must be a Range after type checking, got {}", iterable.item.ty));
    let elem_leafs = struct_fields(&elem_ty, ctx.structs);

    // A comprehension's result list needs a capacity — a range's length is
    // a plain subtraction (clamped to 0, matching `frog_range`'s old
    // "end <= start means empty" convention), no runtime call needed.
    let zero64 = bcx.ins().iconst(types::I64, 0);
    let diff   = bcx.ins().isub(end_val, start_val);
    let is_neg = bcx.ins().icmp(IntCC::SignedLessThan, diff, zero64);
    let len_val = bcx.ins().select(is_neg, zero64, diff);

    let result_list = match output {
        LoopOutput::Discard => None,
        LoopOutput::Collect { stride, ptr_mask } => {
            let stride_arg = bcx.ins().iconst(types::I64, stride);
            let mask_arg   = bcx.ins().iconst(types::I64, ptr_mask);
            let alloc_id = ctx.func_ids["frog_alloc_list"];
            let alloc_ref = ctx.module.declare_func_in_func(alloc_id, bcx.func);
            let alloc_call = bcx.ins().call(alloc_ref, &[len_val, stride_arg, mask_arg]);
            let list = bcx.inst_results(alloc_call)[0];
            declare_gc_ptr(bcx, list);
            Some(list)
        }
    };

    let header_bb = bcx.create_block();
    let body_bb   = bcx.create_block();
    let exit_bb   = bcx.create_block();
    bcx.append_block_param(header_bb, types::I64);

    bcx.ins().jump(header_bb, &[BlockArg::from(start_val)]);

    // Second predecessor is the back-edge below; sealed once that exists.
    bcx.switch_to_block(header_bb);
    let elem = bcx.block_params(header_bb)[0];
    let in_range = bcx.ins().icmp(IntCC::SignedLessThan, elem, end_val);
    bcx.ins().brif(in_range, body_bb, &[], exit_bb, &[]);

    bcx.switch_to_block(body_bb);
    bcx.seal_block(body_bb);

    for (leaf_path, lty) in elem_leafs.iter() {
        let key = var_key(var, leaf_path);
        let var_id = get_or_declare_var(bcx, vars, &key, lty);
        bcx.def_var(var_id, elem);
    }

    // Optional `if` filter: skip straight to the increment when false.
    if let Some(c) = cond {
        let do_bb   = bcx.create_block();
        let skip_bb = bcx.create_block();
        let cond_val = compile_expr(c, bcx, vars, ctx);
        bcx.ins().brif(cond_val, do_bb, &[], skip_bb, &[]);

        bcx.switch_to_block(skip_bb);
        bcx.seal_block(skip_bb);
        let elem_next = bcx.ins().iadd_imm_s(elem, 1);
        bcx.ins().jump(header_bb, &[BlockArg::from(elem_next)]);

        bcx.switch_to_block(do_bb);
        bcx.seal_block(do_bb);
    }

    let body_vals = compile_expr_multi(body, bcx, vars, ctx);
    if let Some(list_ptr) = result_list {
        let body_leafs = struct_fields(&body.item.ty, ctx.structs);
        push_element(bcx, ctx, list_ptr, &body_vals, &body_leafs);
    }

    let elem_next = bcx.ins().iadd_imm_s(elem, 1);
    bcx.ins().jump(header_bb, &[BlockArg::from(elem_next)]);
    bcx.seal_block(header_bb);

    bcx.switch_to_block(exit_bb);
    bcx.seal_block(exit_bb);
    result_list
}

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
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

    /// Snapshot `source_map`'s length before a `compile_entry` call that
    /// might panic partway through — pair with `restore_source_map` on
    /// failure, same discipline as `checkpoint_func_ids`. Append-only, so a
    /// length is enough; no need to clone the whole `Vec`.
    pub fn checkpoint_source_map(&self) -> usize {
        self.source_map.len()
    }

    pub fn restore_source_map(&mut self, len: usize) {
        self.source_map.truncate(len);
    }

    /// The `plans/DATA.md` Stage 3 source map: every top-level `func`/lambda
    /// declared across every entry so far, in declaration order.
    pub fn source_map(&self) -> &[FnSourceInfo] {
        &self.source_map
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
        // `hosts` is empty, so no name can collide with a runtime primitive.
        Self::new_with_hosts(&[]).expect("empty host list can't collide")
    }

    /// As `new`, but also registers `hosts` — every symbol and its uniform
    /// `(ctx, args, out)` import signature is declared here, before any
    /// frog type has been resolved, which is what lets `FrogStateBuilder`
    /// (`state.rs`) install their frog-visible names into the type checker
    /// afterward. See `plans/EMBEDDING.md`.
    ///
    /// Fails if a host function's name collides with a runtime primitive's
    /// `func_ids` key (e.g. `frog_clone`, `frog_str_len`) — registering one
    /// would silently overwrite that key, so later codegen that looks it up
    /// (`ctx.func_ids["frog_clone"]`, ...) would resolve to the host shim's
    /// `(ctx, args, out)` ABI instead, a wrong-ABI call rather than a
    /// diagnostic. Checked here, against the actual declared keys, rather
    /// than a hand-maintained name list that could drift from them.
    pub fn new_with_hosts(hosts: &[crate::host::HostFn]) -> Result<Self, String> {
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "false").expect("is_pic setting");
        // `opt_level` is deliberately left at Cranelift's default of
        // `none`. Measured at `speed` on 2026-08-29: orders 55.8 -> 56.1ms,
        // fib(32) 22.7 -> 23.5ms, both regressions — the extra compile time
        // (+1.3ms on orders) outweighs what GVN/LICM find, because froglang
        // programs are small and JIT compilation is on the critical path of
        // every run. Worth re-testing if whole-program compile time ever
        // stops being a per-run cost.
        // The collector finds its roots by walking the native stack frame
        // by frame (`gc.rs`, "Precise roots"), which needs every JIT frame
        // to actually have a frame pointer. Without this, Cranelift is free
        // to use the frame-pointer register as a general one and the chain
        // ends at the first function that does.
        flag_builder.set("preserve_frame_pointers", "true").expect("preserve_frame_pointers setting");
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
        builder.symbol("frog_str_contains", ffi::frog_str_contains as *const u8);
        builder.symbol("frog_str_print",   ffi::frog_str_print   as *const u8);
        builder.symbol("frog_str_repr_print", ffi::frog_str_repr_print as *const u8);
        builder.symbol("frog_bytes_print", ffi::frog_bytes_print as *const u8);
        builder.symbol("frog_str_println", ffi::frog_str_println as *const u8);
        builder.symbol("frog_panic",       ffi::frog_panic       as *const u8);
        builder.symbol("frog_div_error",   ffi::frog_div_error   as *const u8);
        builder.symbol("frog_int_println", ffi::frog_int_println as *const u8);
        builder.symbol("frog_float_println", ffi::frog_float_println as *const u8);
        builder.symbol("frog_bool_println", ffi::frog_bool_println as *const u8);
        builder.symbol("frog_int_print", ffi::frog_int_print as *const u8);
        builder.symbol("frog_float_print", ffi::frog_float_print as *const u8);
        builder.symbol("frog_bool_print", ffi::frog_bool_print as *const u8);
        builder.symbol("frog_alloc_list",  ffi::frog_alloc_list  as *const u8);
        builder.symbol("frog_list_len",    ffi::frog_list_len    as *const u8);
        builder.symbol("frog_list_get",    ffi::frog_list_get    as *const u8);
        builder.symbol("frog_list_set",    ffi::frog_list_set    as *const u8);
        builder.symbol("frog_list_push",   ffi::frog_list_push   as *const u8);
        builder.symbol("frog_list_slice",  ffi::frog_list_slice  as *const u8);
        builder.symbol("frog_range",       ffi::frog_range       as *const u8);
        builder.symbol("frog_gc_dump",     ffi::frog_gc_dump     as *const u8);
        builder.symbol("frog_alloc_variant", ffi::frog_alloc_variant as *const u8);
        builder.symbol("frog_variant_tag", ffi::frog_variant_tag as *const u8);
        builder.symbol("frog_variant_get", ffi::frog_variant_get as *const u8);
        builder.symbol("frog_variant_set", ffi::frog_variant_set as *const u8);
        builder.symbol("frog_cow_verify", ffi::frog_cow_verify as *const u8);
        builder.symbol("frog_clone",       ffi::frog_clone       as *const u8);
        builder.symbol("frog_ctx_current", crate::runtime::host::frog_ctx_current as *const u8);

        // Host functions (`FrogStateBuilder::func`, `plans/EMBEDDING.md`).
        // Registered before any frog type is resolved — each shim's JIT
        // signature is the uniform `(ctx, args, out)` triple regardless of
        // its frog type, which is exactly what makes that ordering
        // possible. A duplicate `symbol` name here would make
        // `JITBuilder::symbol` non-deterministic about which address wins;
        // `FrogStateBuilder::build` is what actually rejects a colliding or
        // reserved name, so this loop trusts its caller.
        for host in hosts {
            builder.symbol(host.symbol, host.shim);
        }

        let mut module   = JITModule::new(builder);
        let mut func_ids = HashMap::<String, FuncId>::new();
        let mut host_fns = std::collections::HashSet::new();

        use types::I64;
        // Declare Cranelift import signatures for each runtime function.
        declare_rt(&mut module, &mut func_ids, "frog_alloc_str",  "frog_alloc_str",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_len",    "frog_str_len",    &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_concat", "frog_str_concat", &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_eq",     "frog_str_eq",     &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_cmp",    "frog_str_cmp",    &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_contains", "frog_str_contains", &[I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_str_print",  "frog_str_print",  &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_str_repr_print", "frog_str_repr_print", &[I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_bytes_print", "frog_bytes_print", &[I64, I64], None);
        // "print" in froglang calls frog_str_println (with newline).
        declare_rt(&mut module, &mut func_ids, "frog_str_println","print",           &[I64],           None);
        // `panic` prints its message and exits the process from the runtime
        // side (`ffi::frog_panic`); it never returns. It used to be an alias
        // for `frog_str_println`, relying on the generic `Call` codegen's
        // `Type::Never` handling to emit a `trap` once the call came back —
        // but a `trap` is SIGILL, so `panic("boom")` printed its message and
        // then died with exit 132 instead of a clean 1. The `Never` trap
        // after the call is now dead code, which is exactly what it's for.
        declare_rt(&mut module, &mut func_ids, "frog_panic",     "panic",           &[I64],           None);
        // `!`'s desugaring (`TypeChecker::build_unwrap_arms`) resolves to
        // this reserved alias, not `"panic"`, so it can't be redirected by
        // a user-defined `func panic(...)` (which would overwrite the
        // `"panic"` key above via the ordinary user-function registration
        // path) — see `UNWRAP_PANIC_NAME` in typeck.rs.
        declare_rt(&mut module, &mut func_ids, "frog_panic",     "panic!builtin",   &[I64],           None);
        // Integer-division fault reporting — see `emit_int_div_guard`.
        declare_rt(&mut module, &mut func_ids, "frog_div_error", "frog_div_error",  &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_int_println", "frog_int_println", &[I64],           None);
        declare_rt(&mut module, &mut func_ids, "frog_float_println", "frog_float_println", &[types::F64], None);
        declare_rt(&mut module, &mut func_ids, "frog_bool_println", "frog_bool_println", &[types::I8],  None);
        declare_rt(&mut module, &mut func_ids, "frog_int_print", "frog_int_print", &[I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_float_print", "frog_float_print", &[types::F64], None);
        declare_rt(&mut module, &mut func_ids, "frog_bool_print", "frog_bool_print", &[types::I8], None);
        declare_rt(&mut module, &mut func_ids, "frog_alloc_list", "frog_alloc_list", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_len",   "frog_list_len",   &[I64],           Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_get",   "frog_list_get",   &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_set",   "frog_list_set",   &[I64, I64, I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_list_push",  "frog_list_push",  &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_list_slice", "frog_list_slice", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_range",      "frog_range",      &[I64, I64],      Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_gc_dump",    "gc_dump",         &[],               None);
        declare_rt(&mut module, &mut func_ids, "frog_alloc_variant", "frog_alloc_variant", &[I64, I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_variant_tag", "frog_variant_tag", &[I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_variant_get", "frog_variant_get", &[I64, I64], Some(I64));
        declare_rt(&mut module, &mut func_ids, "frog_variant_set", "frog_variant_set", &[I64, I64, I64], None);
        // Deep-clone-on-Copy for a GC-pointer-bearing `Var` read — see
        // `compile_expr_multi`'s `TypedExprKind::Var` arm and `Ctx::liveness`.
        declare_rt(&mut module, &mut func_ids, "frog_cow_verify", "frog_cow_verify", &[I64, I64], None);
        declare_rt(&mut module, &mut func_ids, "frog_clone",      "frog_clone",      &[I64],           Some(I64));
        // Fetches the `FrogCtx*` a host call passes as its own argument 0
        // — never an `iconst` of a host address (see `plans/EMBEDDING.md`,
        // "Getting the ctx pointer without baking an address").
        declare_rt(&mut module, &mut func_ids, "frog_ctx_current", "frog_ctx_current", &[], Some(I64));

        // Every host function shares this one import signature — see
        // `Ctx`'s `host_fns` field and `compile_call`'s host-call arm.
        for host in hosts {
            if func_ids.contains_key(host.name) {
                return Err(format!(
                    "host function '{}' collides with a runtime primitive of the same name",
                    host.name
                ));
            }
            declare_rt(&mut module, &mut func_ids, host.symbol, host.name, &[I64, I64, I64], None);
            host_fns.insert(host.name.to_string());
        }

        Ok(Codegen {
            module,
            func_ids,
            builder_ctx: FunctionBuilderContext::new(),
            host_fns,
            source_map: Vec::new(),
        })
    }

    /// A struct-typed param or return value expands to one `AbiParam` per
    /// flattened leaf field (`struct_fields`), in declared-field order —
    /// Cranelift signatures natively support multiple params/returns, so
    /// this is a direct extension of the pre-struct one-param-per-value
    /// signature shape (every non-struct type still contributes exactly one).
    fn make_sig(&self, params: &[(String, Type, bool)], return_type: &Type, structs: &StructDefs) -> cranelift_codegen::ir::Signature {
        let mut sig = self.module.make_signature();
        for (_, ty, _) in params {
            for (_, lty) in struct_fields(ty, structs) {
                sig.params.push(AbiParam::new(cl_type(&lty)));
            }
        }
        if *return_type != Type::None {
            for (_, lty) in struct_fields(return_type, structs) {
                sig.returns.push(AbiParam::new(cl_type(&lty)));
            }
        }
        // Each `mut` parameter's final value is appended as extra return
        // values, one per flattened leaf, in parameter order — the
        // copy-in/copy-out half of a `mut` parameter (`MUTABILITY.md`).
        // `compile_call` splits these off the call's results after the
        // ordinary return, matching this order exactly.
        for (_, ty, mutable) in params {
            if *mutable {
                for (_, lty) in struct_fields(ty, structs) {
                    sig.returns.push(AbiParam::new(cl_type(&lty)));
                }
            }
        }
        sig
    }

    fn build_func_body(
        name: &str,
        builder_ctx: &mut FunctionBuilderContext,
        cl_ctx: &mut Context,
        module: &mut JITModule,
        func_ids: &HashMap<String, FuncId>,
        params: &[(String, Type, bool)],
        return_type: &Type,
        body: &Spanned<TypedExpr>,
        string_arena: &mut Vec<Vec<u8>>,
        structs: &StructDefs,
        unions: &UnionDefs,
        host_fns: &std::collections::HashSet<String>,
    ) {
        let target_config = module.target_config();
        let mut bcx = FunctionBuilder::new(&mut cl_ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);

        let mut vars: HashMap<String, Variable> = HashMap::new();
        let entry_params: Vec<Value> = bcx.block_params(entry).to_vec();
        // A struct-typed param consumes as many consecutive entry params as
        // it has flattened leaf fields — `make_sig` laid these out in the
        // exact same per-param `struct_fields` order.
        let mut cursor = 0usize;
        let mut mut_params: Vec<(String, Type)> = Vec::new();
        for (name, ty, mutable) in params {
            for (path, lty) in struct_fields(ty, structs) {
                let key = var_key(name, &path);
                declare_and_def_var(&mut bcx, &mut vars, &key, &lty, entry_params[cursor]);
                cursor += 1;
            }
            if *mutable {
                mut_params.push((name.clone(), ty.clone()));
            }
        }

        // A `mut` param's own copy-out (`mut_param_copyout`, consulted at
        // every `return_`) is the only thing this body's `Ownership` marks
        // must stay sound against — see `liveness::analyze_body`. Computed
        // unconditionally now: `Ctx::liveness` is a real codegen input (the
        // clone-on-`Copy` rule at the `Var` arm), not just a debug dump.
        let exit_live: liveness::NameSet = mut_params.iter().map(|(n, _)| n.clone()).collect();
        let body_liveness = liveness::analyze_body(body, &exit_live);
        if std::env::var_os("FROG_DUMP_LIVENESS").is_some() {
            liveness::dump_body(name, body, &body_liveness);
        }

        let mut ctx = Ctx {
            func_ids, module, string_arena, structs, unions,
            printing_unions: Vec::new(), comparing_unions: Vec::new(), mut_params, liveness: body_liveness,
            host_fns,
            cow_verify: cow_verify_enabled(),
        };
        let results = compile_expr_multi(body, &mut bcx, &mut vars, &mut ctx);

        if *return_type != Type::None {
            // If the body's own type is `Never`, it already returned
            // unconditionally (see `TypedExprKind::Return`'s codegen), and
            // `results` is the empty `Vec` that arm produces — we're now
            // positioned in the dead block it switched to. Cranelift still
            // verifies that block's own terminator against the function
            // signature even though nothing ever reaches it at runtime, so
            // it needs a value list of the right shape; the values
            // themselves are never observed. Same reasoning for the
            // appended `mut`-param placeholders below.
            let mut results = if body.item.ty == Type::Never {
                struct_fields(return_type, structs).iter()
                    .map(|(_, t)| placeholder_value(&mut bcx, cl_type(t)))
                    .collect()
            } else {
                results
            };
            if body.item.ty == Type::Never {
                for (_, ty) in &ctx.mut_params {
                    for (_, lty) in struct_fields(ty, structs) {
                        results.push(placeholder_value(&mut bcx, cl_type(&lty)));
                    }
                }
            } else {
                results.extend(mut_param_copyout(&mut bcx, &vars, &ctx));
            }
            bcx.ins().return_(&results);
        } else if !ctx.mut_params.is_empty() {
            let results = mut_param_copyout(&mut bcx, &vars, &ctx);
            bcx.ins().return_(&results);
        } else {
            bcx.ins().return_(&[]);
        }

        bcx.seal_all_blocks();
        bcx.finalize(target_config);
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
        host_fns: &std::collections::HashSet<String>,
    ) -> Vec<(String, Type)> {
        let target_config = module.target_config();
        let mut bcx = FunctionBuilder::new(&mut cl_ctx.func, builder_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        bcx.seal_block(entry);
        let out_ptr = bcx.block_params(entry)[0];

        let mut vars: HashMap<String, Variable> = HashMap::new();

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
                declare_and_def_var(&mut bcx, &mut vars, &key, lty, val);
            }
        }
        let mut last_val = bcx.ins().iconst(types::I64, 0);
        let mut last_ty = &Type::Int;

        // Every prior entry's binding (`env_types`, since `FrogState::eval`
        // re-roots all of `env` after every entry regardless of whether this
        // one touches it — see `liveness::analyze_entry`'s doc comment) plus
        // this entry's own top-level bindings: both end up in
        // `env`/`bindings` by the time this entry finishes, so both must be
        // treated as live through to the end. Over-including a name that
        // turns out `Never`-typed (skipped from the real `bindings` list
        // below) only costs precision, never soundness. Computed
        // unconditionally now — see `build_func_body`'s matching comment.
        let mut exit_live: liveness::NameSet = env_types.keys().cloned().collect();
        for s in stmts {
            if let TypedExprKind::Assign { name, value } = &s.item.kind {
                if !matches!(value.item.kind, TypedExprKind::Function { .. }) {
                    exit_live.insert(name.clone());
                }
            }
        }
        let entry_liveness = liveness::analyze_entry(stmts, &exit_live);
        if std::env::var_os("FROG_DUMP_LIVENESS").is_some() {
            liveness::dump_entry("<entry>", stmts, &entry_liveness);
        }

        let mut ctx = Ctx {
            func_ids, module, string_arena, structs, unions,
            printing_unions: Vec::new(), comparing_unions: Vec::new(), mut_params: Vec::new(), liveness: entry_liveness,
            host_fns,
            cow_verify: cow_verify_enabled(),
        };

        let mut bindings: Vec<(String, Type)> = Vec::new();
        let mut slot_cursor: usize = 0;

        for stmt in stmts {
            if let TypedExprKind::Assign { name, value } = &stmt.item.kind {
                if matches!(value.item.kind, TypedExprKind::Function { .. }) {
                    continue;
                }
                let vals = compile_expr_multi(stmt, &mut bcx, &mut vars, &mut ctx);
                if vals.is_empty() {
                    // `value` is `Never`-typed (e.g. `let x = panic(...)`)
                    // — the callee trapped and control never reaches here,
                    // so there's no computed value to store and no real
                    // binding to create. `bcx` is already on the dead
                    // block the trap switched to, but later top-level
                    // statements are still compiled into it (as dead code)
                    // and may reference `name` — declare its leaf `vars`
                    // with placeholder zeros so those unreachable
                    // references don't crash the compiler with "unbound
                    // variable in codegen". Not pushed to `bindings`: it
                    // has no real value and nothing ever executes far
                    // enough to read or write its `out_ptr` slot.
                    for (path, lty) in struct_fields(&value.item.ty, structs) {
                        let key = var_key(name, &path);
                        let var = get_or_declare_var(&mut bcx, &mut vars, &key, &lty);
                        let zero = placeholder_value(&mut bcx, cl_type(&lty));
                        bcx.def_var(var, zero);
                    }
                    continue;
                }
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
                    bcx.ins().store(MachMemFlags::new(), repr, out_ptr, offset);
                    slot_cursor += 1;
                }
                bindings.push((name.clone(), last_ty.clone()));
                continue;
            }
            let vals = compile_expr_multi(stmt, &mut bcx, &mut vars, &mut ctx);
            if vals.is_empty() {
                // `stmt` is `Never`-typed (e.g. a bare top-level
                // `panic(...)`) — control never reaches here; keep
                // compiling into the dead block the trap switched to.
                continue;
            }
            last_val = vals[0];
            last_ty = &stmt.item.ty;
        }


        // __frog_main[_N] always returns a single i64 (see this function's
        // doc comment — a struct-typed final result only reports its first
        // leaf here).
        let leaf0_ty = struct_fields(last_ty, structs).into_iter().next().map(|(_, t)| t).unwrap_or(Type::Int);
        let last_val = to_i64_repr(&mut bcx, &leaf0_ty, last_val);

        bcx.ins().return_(&[last_val]);
        bcx.seal_all_blocks();
        bcx.finalize(target_config);

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
                    self.source_map.push(FnSourceInfo { name: name.clone(), span: stmt.span, entry_id });
                }
            }
        }

        // Stack maps come off each `Context` right after `define_function`,
        // but a function has no address until `finalize_definitions` below,
        // so they are held here and filed once at the end.
        let mut pending_maps: Vec<(FuncId, gc::JitFunctionMaps)> = Vec::new();
        // Names for the same functions, kept only when `FROG_JIT_SYMBOLS`
        // asks for a symbol dump — see `dump_jit_symbols`.
        let mut pending_names: Vec<(FuncId, String)> = Vec::new();

        // ── Pass 2: Define all function bodies ───────────────────────────────
        let func_defs: Vec<(String, FuncId, Vec<(String, Type, bool)>, Type, Box<Spanned<TypedExpr>>)> =
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

        for (dbgname, func_id, params, return_type, body) in &func_defs {
            let sig = self.make_sig(params, return_type, structs);
            let mut ctx = self.module.make_context();
            ctx.func.signature = sig;

            // `module` and `func_ids` are disjoint fields, so borrowing them
            // separately here (rather than cloning `func_ids` — O(n) per
            // function, O(n^2) per entry) is fine: `func_ids` is read-only
            // for the whole of Pass 2, only ever written during Pass 1 above.
            Self::build_func_body(
                dbgname,
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
                &self.host_fns,
            );

            self.module
                .define_function(*func_id, &mut ctx)
                .unwrap_or_else(|e| panic!("define_function failed: {}", e));
            pending_maps.push((*func_id, take_stack_maps(&ctx)));
            pending_names.push((*func_id, dbgname.clone()));
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
            &self.host_fns,
        );

        self.module
            .define_function(main_id, &mut ctx)
            .unwrap_or_else(|e| panic!("define {} failed: {}", entry_name, e));
        pending_maps.push((main_id, take_stack_maps(&ctx)));
        pending_names.push((main_id, entry_name.clone()));
        self.module.clear_context(&mut ctx);

        self.module.finalize_definitions().expect("finalize_definitions failed");

        // Only now do these functions have addresses, so only now can their
        // stack maps be filed by return address — see `gc::JitCode`.
        for (func_id, maps) in pending_maps {
            if maps.maps.is_empty() { continue; }
            let start = self.module.get_finalized_function(func_id) as usize;
            gc::JIT_CODE.with(|c| c.borrow_mut().register(gc::JitFunctionMaps { start, ..maps }));
        }
        self.dump_jit_symbols(&pending_names);

        (main_id, bindings)
    }
}

impl Codegen {
    /// Append `start length name` for each just-finalized function to the
    /// file named by `FROG_JIT_SYMBOLS`, and do nothing at all when that
    /// variable is unset.
    ///
    /// A sampling profiler (macOS `sample`, `perf`) sees JIT-compiled code
    /// as bare addresses in an anonymous mapping, so a profile of a
    /// froglang program attributes essentially all of its time to `???`.
    /// This is the missing half: `benches/symbolize.py` joins these ranges
    /// against a profile's addresses to say which froglang function the
    /// time was actually in.
    fn dump_jit_symbols(&self, names: &[(FuncId, String)]) {
        let Some(path) = std::env::var_os("FROG_JIT_SYMBOLS") else { return };
        use std::io::Write;
        let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
            eprintln!("frog: could not open FROG_JIT_SYMBOLS file {:?}", path);
            return;
        };
        for (func_id, name) in names {
            let start = self.module.get_finalized_function(*func_id) as usize;
            // Cranelift does not hand back a finalized function's length, so
            // the next function's start is used as this one's end by the
            // symbolizer; a length of 0 here means "until the next symbol".
            let _ = writeln!(f, "{:#x} 0 {}", start, name);
        }
    }
}

/// Pull one just-compiled function's user stack maps out of its `Context`,
/// in the shape `gc::JitCode` wants: return-address offsets and SP-relative
/// byte offsets, with no Cranelift types left in them.
///
/// `start` is left as `0` — the function has no address until
/// `Module::finalize_definitions` has run, and the caller fills it in then.
///
/// Cranelift emits these already sorted by return address (it asserts as
/// much when pushing them), which `JitCode::lookup`'s binary search relies
/// on; the assert below is what makes that reliance explicit rather than
/// assumed.
fn take_stack_maps(ctx: &Context) -> gc::JitFunctionMaps {
    let compiled = ctx.compiled_code().expect("function was just defined, so it is compiled");
    let maps: Vec<(u32, Vec<u32>)> = compiled
        .buffer
        .user_stack_maps()
        .iter()
        .map(|(return_addr, _span, map)| (*return_addr, map.entries().map(|(_ty, off)| off).collect()))
        .collect();
    debug_assert!(
        maps.windows(2).all(|w| w[0].0 < w[1].0),
        "cranelift emitted stack maps out of return-address order, which `JitCode::lookup` binary-searches",
    );
    gc::JitFunctionMaps { start: 0, len: compiled.code_info().total_size as usize, maps }
}

/// Parse, type-check, compile, and run a froglang source string.
/// Returns the i64 result of the final expression.
pub fn compile_and_run(src: &str) -> i64 {
    use crate::frontend::parser::Parser;
    use crate::frontend::typeck::TypeChecker;

    let ast = Parser::parse(src).expect("parse error");
    let mut tc = TypeChecker::new();
    let mut typed = tc.check_and_lower(ast).expect("type error");
    crate::frontend::liveness::number_nodes(&mut typed);

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
    // Same protocol `FrogState::call_jit` follows: `f` writes each top-level
    // binding into this buffer as it goes, so the collector must scan it for
    // the duration of the call — see `gc::GcHeap::push_scanned_span`.
    let out_gc_slots = gc_slots_of_bindings(&bindings, tc.struct_defs());
    let out_ptr = out_buf.as_mut_ptr();
    gc::GC_HEAP.with(|h| h.borrow_mut().push_scanned_span(out_ptr, out_gc_slots));
    let result = f(out_ptr as i64);
    gc::GC_HEAP.with(|h| h.borrow_mut().pop_scanned_span());
    result
    // string_arena and out_buf dropped here, after f() returns
}
