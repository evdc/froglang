// lang-macros/src/lib.rs
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote, ToTokens};
use syn::{parse_macro_input, punctuated::Punctuated, Attribute, DeriveInput, Meta};
use syn::{Fields, Token as SynToken};
use syn::{FnArg, ItemFn, Pat, ReturnType};

/// Registers a Rust function as a froglang host function — see
/// `plans/EMBEDDING.md`.
///
/// `#[frog_fn] fn read_file(path: String) -> String { ... }` leaves
/// `read_file` itself untouched and callable normally, and additionally
/// generates:
///
/// - `__frog_shim_read_file`, a `#[no_mangle] extern "C" fn(ctx, args, out)`
///   matching the uniform shim ABI every host function shares regardless of
///   its frog type (`compile_host_call` in `codegen/mod.rs`). It expands
///   `jit_frame_guard!()` directly in its own body — that macro's doc
///   comment (`gc.rs`) states this must happen in the function the mutator
///   calls, not a helper it calls, so this can't be factored out.
/// - `read_file_host() -> froglang_core::host::HostFn`, the descriptor
///   `FrogStateBuilder::func` wants. A function rather than a `const`:
///   `HostFn::params` is a `Vec<Type>`, and `Type` (`Str`, `List`, ...)
///   isn't const-constructible.
///
/// Every parameter type must implement `FromFrog` and the return type
/// `ToFrog` (`froglang_core::host`) — implemented today for `i64`, `f64`,
/// `bool`, `()`, `String`/`&str`, and `Vec<T>` of any of those. A parameter
/// must be a plain `ident: Type` — no patterns, no `self`.
#[proc_macro_attribute]
pub fn frog_fn(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    match _frog_fn(input) {
        Ok(res) => res,
        Err(err) => err.to_compile_error().into(),
    }
}

fn _frog_fn(f: ItemFn) -> syn::Result<TokenStream> {
    let fn_name = f.sig.ident.clone();
    let shim_ident = format_ident!("__frog_shim_{}", fn_name);
    let descriptor_ident = format_ident!("{}_host", fn_name);
    let symbol_lit = format!("__frog_host_{}", fn_name);
    let frog_name_lit = fn_name.to_string();

    let mut param_idents: Vec<syn::Ident> = Vec::new();
    let mut param_types: Vec<syn::Type> = Vec::new();
    for arg in &f.sig.inputs {
        match arg {
            FnArg::Receiver(r) => {
                return Err(syn::Error::new_spanned(r, "#[frog_fn] cannot take self"));
            }
            FnArg::Typed(pt) => {
                let Pat::Ident(pi) = pt.pat.as_ref() else {
                    return Err(syn::Error::new_spanned(&pt.pat, "#[frog_fn] parameters must be plain identifiers"));
                };
                param_idents.push(pi.ident.clone());
                param_types.push((*pt.ty).clone());
            }
        }
    }
    let ret_ty: syn::Type = match &f.sig.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, t) => (**t).clone(),
    };

    // Cumulative slot offsets, computed as token streams (not evaluated
    // here) so a multi-slot `FromFrog` impl — a struct or union argument —
    // needs no change to this macro, only to that impl's `leaves()`. See
    // `host.rs`'s `FromFrog`/`ToFrog` doc comments.
    let mut offsets: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut cum = quote! { 0usize };
    for ty in &param_types {
        offsets.push(cum.clone());
        cum = quote! { (#cum + <#ty as ::froglang_core::host::FromFrog>::slots()) };
    }
    let total_arg_slots = cum;

    // Per-*leaf* GC-pointer mask for the flattened argument buffer, so the
    // shim only roots slots that actually hold a heap pointer — a raw
    // `Int`/`Float` slot has no tag bits and must never be mistaken for one
    // (see `RuntimeRoots::hold_masked`'s doc comment). Built from each
    // param's own `leaves()` — a multi-leaf (struct-shaped) argument's
    // scalar and pointer columns are not uniform, so this can't be a single
    // repeated bit the way it could when every `FromFrog` impl was
    // single-slot.
    let arg_ptr_mask_pushes = param_types.iter().map(|ty| {
        quote! {
            __frog_ptr_mask.extend(
                <#ty as ::froglang_core::host::FromFrog>::leaves().iter()
                    .map(::froglang_core::codegen::is_heap_ty)
            );
        }
    });

    let unmarshal_args = param_idents.iter().zip(param_types.iter()).zip(offsets.iter()).map(|((ident, ty), start)| {
        quote! {
            let #ident = <#ty as ::froglang_core::host::FromFrog>::from_frog(
                ctx, &__frog_args[(#start)..(#start + <#ty as ::froglang_core::host::FromFrog>::slots())],
            );
        }
    });

    let expanded = quote! {
        #f

        // The shim is a JIT call target, not a Rust API: its three raw
        // pointers are supplied by generated code that upholds their
        // validity (see `HostFn`), and marking it `unsafe` would only move
        // the obligation somewhere no human writes.
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        #[no_mangle]
        pub extern "C" fn #shim_ident(
            __frog_ctx: *mut ::froglang_core::runtime::host::FrogCtx,
            __frog_args_ptr: *const i64,
            __frog_out_ptr: *mut i64,
        ) {
            let _jit_frame = ::froglang_core::jit_frame_guard!();
            let ctx: &mut ::froglang_core::runtime::host::FrogCtx = unsafe { &mut *__frog_ctx };
            let __frog_args: &[i64] = unsafe {
                ::std::slice::from_raw_parts(__frog_args_ptr, #total_arg_slots)
            };
            // Every argument slot died at this call from the JIT caller's
            // point of view (no stack map covers a raw stack-slot buffer
            // either) — held for this shim's whole body per the runtime's
            // GC-safety rule (`gc.rs`, `RuntimeRoots`). Only the slots that
            // actually hold a heap pointer are rooted as such — a raw
            // `Int`/`Float` slot has no tag bits, and rooting it as a
            // pointer would make the collector dereference an arbitrary
            // address.
            let mut __frog_ptr_mask: ::std::vec::Vec<bool> = ::std::vec::Vec::with_capacity(__frog_args.len());
            #(#arg_ptr_mask_pushes)*
            let _roots = ::froglang_core::runtime::gc::RuntimeRoots::hold_masked(__frog_args, &__frog_ptr_mask);
            // Roots everything this shim allocates via `ctx` until it
            // drops at the end of this function — i.e. after the result
            // has been written into `__frog_out_ptr` below.
            let mut _scope = ctx.scope();

            #(#unmarshal_args)*

            let __frog_result: #ret_ty = #fn_name(#(#param_idents),*);

            let __frog_out: &mut [i64] = unsafe {
                ::std::slice::from_raw_parts_mut(__frog_out_ptr, <#ret_ty as ::froglang_core::host::ToFrog>::slots())
            };
            ::froglang_core::host::ToFrog::to_frog(__frog_result, ctx, __frog_out);
        }

        pub fn #descriptor_ident() -> ::froglang_core::host::HostFn {
            ::froglang_core::host::HostFn {
                name: #frog_name_lit,
                symbol: #symbol_lit,
                shim: #shim_ident as *const u8,
                params: ::std::vec![ #( <#param_types as ::froglang_core::host::FromFrog>::frog_type() ),* ],
                ret: <#ret_ty as ::froglang_core::host::ToFrog>::frog_type(),
                param_leaves: ::std::vec![ #( <#param_types as ::froglang_core::host::FromFrog>::leaves() ),* ],
                ret_leaves: <#ret_ty as ::froglang_core::host::ToFrog>::leaves(),
            }
        }
    };

    Ok(expanded.into())
}

/// Parsed `#[frog(...)]` attributes shared by `FrogData`/`FrogUnion`.
struct FrogTypeAttrs {
    /// `#[frog(name = "...")]` — defaults to the Rust type's own name.
    name: Option<String>,
    /// `#[frog(error)]` — emit `error Name(...)` (grants `Trait::Error`)
    /// instead of plain `data Name(...)`.
    error: bool,
    /// `#[frog(declared)]` — this type maps onto a `data`/`error`
    /// declaration frog source already provides; `frog_decl()` returns
    /// `None` instead of generating one.
    declared: bool,
}

fn parse_frog_type_attrs(attrs: &[Attribute]) -> syn::Result<FrogTypeAttrs> {
    let mut out = FrogTypeAttrs { name: None, error: false, declared: false };
    for attr in attrs {
        if !attr.path().is_ident("frog") { continue; }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                out.name = Some(lit.value());
            } else if meta.path.is_ident("error") {
                out.error = true;
            } else if meta.path.is_ident("declared") {
                out.declared = true;
            } else {
                return Err(meta.error("unknown #[frog(...)] attribute; expected name, error, or declared"));
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// One field of a struct/variant being derived over, abstracting away
/// named vs. positional/tuple vs. unit fields so the rest of the derive
/// doesn't need three code paths.
struct DerivedField {
    /// How to read this field off an owned `self`/variant value — `self.x`
    /// or `self.0`.
    accessor: proc_macro2::TokenStream,
    /// The frog-side field name: the declared name, or the stringified
    /// index for a positional field — matching `field_name_or_positional`
    /// (`frontend/typeck.rs`)'s convention, though for `frog_decl`
    /// rendering purposes only positional's *absence* of a name matters.
    decl_name: Option<String>,
    ty: syn::Type,
}

fn derived_fields(fields: &Fields) -> Vec<DerivedField> {
    match fields {
        Fields::Named(f) => f.named.iter().map(|fld| {
            let ident = fld.ident.clone().unwrap();
            DerivedField {
                accessor: quote! { self.#ident },
                decl_name: Some(ident.to_string()),
                ty: fld.ty.clone(),
            }
        }).collect(),
        Fields::Unnamed(f) => f.unnamed.iter().enumerate().map(|(i, fld)| {
            let idx = syn::Index::from(i);
            DerivedField {
                accessor: quote! { self.#idx },
                decl_name: None,
                ty: fld.ty.clone(),
            }
        }).collect(),
        Fields::Unit => Vec::new(),
    }
}

/// `data`/`error Name(field: Type, ...)` source text for one struct/variant
/// field list, rendered at run time (`Type`'s `Display` needs a runtime
/// `Type` value, so this can't be a compile-time string) via each field's
/// own `ToFrog::frog_type()`.
fn frog_decl_fields_expr<'a>(fields: impl Iterator<Item = (Option<&'a str>, &'a syn::Type)>) -> proc_macro2::TokenStream {
    let pieces: Vec<proc_macro2::TokenStream> = fields.map(|(decl_name, ty)| {
        match decl_name {
            Some(name) => quote! { format!("{}: {}", #name, <#ty as ::froglang_core::host::ToFrog>::frog_type()) },
            None => quote! { format!("{}", <#ty as ::froglang_core::host::ToFrog>::frog_type()) },
        }
    }).collect();
    quote! { ::std::vec![ #(#pieces),* ].join(", ") }
}

/// `#[derive(FrogData)]` — see `plans/EMBEDDING.md`. Implements `ToFrog`,
/// `FromFrog` and `FrogDecl` for a plain Rust struct (named, tuple, or
/// unit), mapping it onto a frog `data`/`error Name(...)` declaration by
/// field order:
///
/// - `leaves()` is the concatenation of each field's own `leaves()`, in
///   declaration order — exactly `codegen::struct_fields`'s recursion, so a
///   struct-typed field flattens inline. Composed purely from the impl
///   tree; no `StructDefs` lookup, which is what keeps this AOT-clean (see
///   `host.rs`'s `FromFrog`/`ToFrog` doc comments). `FrogStateBuilder::
///   data::<T>()`'s one-time audit is what catches a mismatch instead.
/// - `frog_decl()` renders `"data Name(x: Int, y: Int)"` (or
///   `"error Name(...)"` under `#[frog(error)]`), or is suppressed
///   entirely under `#[frog(declared)]`.
/// - A tuple struct maps to frog's positional fields (`data Lit(Int)`) —
///   frog forbids mixing named and positional fields in one declaration,
///   so a derive only ever emits one or the other.
#[proc_macro_derive(FrogData, attributes(frog))]
pub fn derive_frog_data(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match _derive_frog_data(input) {
        Ok(res) => res,
        Err(err) => err.to_compile_error().into(),
    }
}

fn _derive_frog_data(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let attrs = parse_frog_type_attrs(&input.attrs)?;
    let frog_name = attrs.name.clone().unwrap_or_else(|| name.to_string());

    let syn::Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(&input, "#[derive(FrogData)] only supports structs — see FrogUnion for enums"));
    };

    let fields = derived_fields(&data.fields);
    let field_types: Vec<syn::Type> = fields.iter().map(|f| f.ty.clone()).collect();
    let accessors: Vec<proc_macro2::TokenStream> = fields.iter().map(|f| f.accessor.clone()).collect();
    let bind_idents: Vec<syn::Ident> = (0..fields.len()).map(|i| format_ident!("__frog_f{}", i)).collect();

    let ctor = match &data.fields {
        Fields::Named(f) => {
            let idents: Vec<_> = f.named.iter().map(|fld| fld.ident.clone().unwrap()).collect();
            quote! { #name { #( #idents: #bind_idents ),* } }
        }
        Fields::Unnamed(_) => quote! { #name( #( #bind_idents ),* ) },
        Fields::Unit => quote! { #name },
    };

    let decl_keyword = if attrs.error { "error" } else { "data" };
    let decl_fields_expr = frog_decl_fields_expr(fields.iter().map(|f| (f.decl_name.as_deref(), &f.ty)));
    let frog_decl_body = if attrs.declared {
        quote! { fn frog_decl() -> ::std::option::Option<::std::string::String> { ::std::option::Option::None } }
    } else {
        quote! {
            fn frog_decl() -> ::std::option::Option<::std::string::String> {
                ::std::option::Option::Some(format!("{} {}({})", #decl_keyword, #frog_name, #decl_fields_expr))
            }
        }
    };

    let expanded = quote! {
        impl ::froglang_core::host::ToFrog for #name {
            fn frog_type() -> ::froglang_core::frontend::typeck::Type {
                ::froglang_core::frontend::typeck::Type::strukt(#frog_name)
            }
            fn leaves() -> ::std::vec::Vec<::froglang_core::frontend::typeck::Type> {
                let mut __v = ::std::vec::Vec::new();
                #( __v.extend(<#field_types as ::froglang_core::host::ToFrog>::leaves()); )*
                __v
            }
            fn to_frog(self, ctx: &mut ::froglang_core::runtime::host::FrogCtx, out: &mut [i64]) {
                #[allow(unused_mut, unused_variables)]
                let mut __cursor = 0usize;
                #(
                    {
                        let __n = <#field_types as ::froglang_core::host::ToFrog>::slots();
                        ::froglang_core::host::ToFrog::to_frog(#accessors, ctx, &mut out[__cursor..__cursor + __n]);
                        __cursor += __n;
                    }
                )*
            }
        }

        impl ::froglang_core::host::FromFrog for #name {
            fn frog_type() -> ::froglang_core::frontend::typeck::Type {
                ::froglang_core::frontend::typeck::Type::strukt(#frog_name)
            }
            fn leaves() -> ::std::vec::Vec<::froglang_core::frontend::typeck::Type> {
                let mut __v = ::std::vec::Vec::new();
                #( __v.extend(<#field_types as ::froglang_core::host::FromFrog>::leaves()); )*
                __v
            }
            fn from_frog(ctx: &::froglang_core::runtime::host::FrogCtx, slots: &[i64]) -> Self {
                #[allow(unused_mut, unused_variables)]
                let mut __cursor = 0usize;
                #(
                    let #bind_idents = {
                        let __n = <#field_types as ::froglang_core::host::FromFrog>::slots();
                        let __v = <#field_types as ::froglang_core::host::FromFrog>::from_frog(ctx, &slots[__cursor..__cursor + __n]);
                        __cursor += __n;
                        __v
                    };
                )*
                #ctor
            }
        }

        impl ::froglang_core::host::FrogDecl for #name {
            #frog_decl_body
        }
    };

    Ok(expanded.into())
}

/// One enum variant's fields, in the shape a `match` arm needs — pattern
/// plus fresh bind idents, distinct from `DerivedField`'s `self.x`-style
/// accessor since a variant is read by destructuring, not field access.
struct EnumVariantFields {
    /// `Self::Circle { r }` / `Self::Foo(__frog_v0, __frog_v1)` / `Self::Unit`.
    pattern: proc_macro2::TokenStream,
    /// Also used, unchanged, as the constructor's field values in
    /// `from_frog` (`Self::Circle { r }` reads the same both ways; a tuple
    /// variant's fresh idents are equally fine on either side).
    bind_idents: Vec<syn::Ident>,
    field_types: Vec<syn::Type>,
    /// `(declared field name, type)` pairs for `frog_decl` rendering —
    /// `None` name for a positional/tuple variant.
    decl_fields: Vec<(Option<String>, syn::Type)>,
}

fn enum_variant_fields(variant_ident: &syn::Ident, fields: &Fields) -> EnumVariantFields {
    match fields {
        Fields::Named(f) => {
            let idents: Vec<syn::Ident> = f.named.iter().map(|fld| fld.ident.clone().unwrap()).collect();
            let tys: Vec<syn::Type> = f.named.iter().map(|fld| fld.ty.clone()).collect();
            let decl_fields = idents.iter().cloned().zip(tys.iter().cloned())
                .map(|(i, t)| (Some(i.to_string()), t)).collect();
            EnumVariantFields {
                pattern: quote! { Self::#variant_ident { #( #idents ),* } },
                bind_idents: idents,
                field_types: tys,
                decl_fields,
            }
        }
        Fields::Unnamed(f) => {
            let binds: Vec<syn::Ident> = (0..f.unnamed.len()).map(|i| format_ident!("__frog_v{}", i)).collect();
            let tys: Vec<syn::Type> = f.unnamed.iter().map(|fld| fld.ty.clone()).collect();
            let decl_fields = tys.iter().cloned().map(|t| (None, t)).collect();
            EnumVariantFields {
                pattern: quote! { Self::#variant_ident( #( #binds ),* ) },
                bind_idents: binds,
                field_types: tys,
                decl_fields,
            }
        }
        Fields::Unit => EnumVariantFields {
            pattern: quote! { Self::#variant_ident },
            bind_idents: Vec::new(),
            field_types: Vec::new(),
            decl_fields: Vec::new(),
        },
    }
}

/// `#[derive(FrogUnion)]` — see `plans/EMBEDDING.md`/`plans/RUNTIME.md`.
/// Implements `ToFrog`, `FromFrog` and `FrogDecl` for a plain Rust `enum`,
/// mapping it onto a frog nominal union (`data Name is A(...) | B(...)`, or
/// `error Name is ...` under `#[frog(error)]`) — the inline-layout case
/// only (`codegen::MAX_INLINE_UNION_MEMBERS`, currently 6); a self-
/// referential or wider union is Stage 4 (`plans/EMBEDDING.md`), not yet
/// supported here.
///
/// Each variant maps to a nominal marker `Type::strukt("Name.Variant")`,
/// exactly as `hoist_data_decls` (`frontend/typeck.rs`) registers a frog-
/// declared union's members — `frog_type()` normalizes `Union(markers)` at
/// run time (the real `Type::normalize`, not a re-implementation), which is
/// what assigns each member its runtime tag (`Ord for Type`). Declaration
/// order is therefore *not* tag order; `codegen::reorder_member_leaves_by_
/// marker` bridges the two.
#[proc_macro_derive(FrogUnion, attributes(frog))]
pub fn derive_frog_union(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match _derive_frog_union(input) {
        Ok(res) => res,
        Err(err) => err.to_compile_error().into(),
    }
}

fn _derive_frog_union(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let attrs = parse_frog_type_attrs(&input.attrs)?;
    let frog_name = attrs.name.clone().unwrap_or_else(|| name.to_string());

    let syn::Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(&input, "#[derive(FrogUnion)] only supports enums — see FrogData for structs"));
    };
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&input.generics, "#[derive(FrogUnion)] doesn't support generic enums — frog itself rejects generic unions"));
    }
    if data.variants.len() < 2 {
        return Err(syn::Error::new_spanned(
            &input,
            "#[derive(FrogUnion)] needs at least two variants — a one-variant union isn't a \
             union (Type::normalize collapses `X` alone to `X`, not `Union([X])`)",
        ));
    }

    let variants: Vec<EnumVariantFields> = data.variants.iter()
        .map(|v| enum_variant_fields(&v.ident, &v.fields))
        .collect();
    let variant_idents: Vec<&syn::Ident> = data.variants.iter().map(|v| &v.ident).collect();
    let markers: Vec<String> = variant_idents.iter().map(|v| format!("{}.{}", frog_name, v)).collect();
    let marker_exprs: Vec<proc_macro2::TokenStream> = markers.iter()
        .map(|m| quote! { ::froglang_core::frontend::typeck::Type::strukt(#m) })
        .collect();
    let nvariants = variants.len();

    // Runtime-value leaf lists, one expression per variant, declaration
    // order — reordered into normalized (tag) order at call time via
    // `reorder_member_leaves_by_marker`, since only `Type::normalize`
    // itself (not this macro) knows that order.
    let variant_leaves_to: Vec<proc_macro2::TokenStream> = variants.iter().map(|v| {
        let tys = &v.field_types;
        quote! { { let mut __v = ::std::vec::Vec::new(); #( __v.extend(<#tys as ::froglang_core::host::ToFrog>::leaves()); )* __v } }
    }).collect();
    let variant_leaves_from: Vec<proc_macro2::TokenStream> = variants.iter().map(|v| {
        let tys = &v.field_types;
        quote! { { let mut __v = ::std::vec::Vec::new(); #( __v.extend(<#tys as ::froglang_core::host::FromFrog>::leaves()); )* __v } }
    }).collect();

    let to_frog_arms: Vec<proc_macro2::TokenStream> = variants.iter().zip(marker_exprs.iter()).map(|(v, marker)| {
        let pattern = &v.pattern;
        let field_packs: Vec<proc_macro2::TokenStream> = v.bind_idents.iter().zip(v.field_types.iter()).map(|(bind, ty)| {
            quote! {
                {
                    let __n = <#ty as ::froglang_core::host::ToFrog>::slots();
                    let mut __sub = ::std::vec![0i64; __n];
                    ::froglang_core::host::ToFrog::to_frog(#bind, ctx, &mut __sub);
                    __buf.extend(__sub);
                }
            }
        }).collect();
        quote! {
            #pattern => {
                let __idx = members.iter().position(|m| *m == #marker)
                    .expect("FrogUnion::to_frog: variant marker not found in normalized union");
                let mut __buf: ::std::vec::Vec<i64> = ::std::vec::Vec::new();
                #(#field_packs)*
                (__idx, __buf)
            }
        }
    }).collect();

    let from_frog_arms: Vec<proc_macro2::TokenStream> = variants.iter().zip(marker_exprs.iter()).map(|(v, marker)| {
        let bind_idents = &v.bind_idents;
        let field_types = &v.field_types;
        let ctor = &v.pattern; // `Self::Circle { r }` / `Self::Foo(__frog_v0, ..)` — same shape reconstructs
        quote! {
            if *__member_ty == #marker {
                #[allow(unused_mut, unused_variables)]
                let mut __cursor = 0usize;
                #(
                    let #bind_idents = {
                        let __n = <#field_types as ::froglang_core::host::FromFrog>::slots();
                        let __v = <#field_types as ::froglang_core::host::FromFrog>::from_frog(ctx, &__leaf_vals[__cursor..__cursor + __n]);
                        __cursor += __n;
                        __v
                    };
                )*
                return #ctor;
            }
        }
    }).collect();

    let decl_keyword = if attrs.error { "error" } else { "data" };
    let variant_decl_exprs: Vec<proc_macro2::TokenStream> = variant_idents.iter().zip(variants.iter()).map(|(ident, v)| {
        let name_str = ident.to_string();
        if v.decl_fields.is_empty() {
            quote! { #name_str.to_string() }
        } else {
            let fields_expr = frog_decl_fields_expr(v.decl_fields.iter().map(|(n, t)| (n.as_deref(), t)));
            quote! { format!("{}({})", #name_str, #fields_expr) }
        }
    }).collect();
    let frog_decl_body = if attrs.declared {
        quote! { fn frog_decl() -> ::std::option::Option<::std::string::String> { ::std::option::Option::None } }
    } else {
        quote! {
            fn frog_decl() -> ::std::option::Option<::std::string::String> {
                let __variants: ::std::vec::Vec<::std::string::String> = ::std::vec![ #(#variant_decl_exprs),* ];
                ::std::option::Option::Some(format!("{} {} is {}", #decl_keyword, #frog_name, __variants.join(" | ")))
            }
        }
    };

    let expanded = quote! {
        // A compile-time-checked guard against the real, exported constant
        // (not a hardcoded `6` here) — `MAX_INLINE_UNION_MEMBERS` is the
        // number of member tags the low 3 bits of a tagged pointer can
        // hold (`plans/RUNTIME.md`'s "Word encoding"); a wider union needs
        // the boxed representation this derive doesn't build.
        const _: () = assert!(
            #nvariants <= ::froglang_core::codegen::MAX_INLINE_UNION_MEMBERS,
            "#[derive(FrogUnion)] only supports inline (<= MAX_INLINE_UNION_MEMBERS) unions",
        );

        impl ::froglang_core::host::ToFrog for #name {
            fn frog_type() -> ::froglang_core::frontend::typeck::Type {
                ::froglang_core::frontend::typeck::Type::Union(
                    ::std::vec![ #(#marker_exprs),* ]
                ).normalize()
            }
            fn leaves() -> ::std::vec::Vec<::froglang_core::frontend::typeck::Type> {
                let __union_ty = <Self as ::froglang_core::host::ToFrog>::frog_type();
                let ::froglang_core::frontend::typeck::Type::Union(members) = &__union_ty else {
                    unreachable!("FrogUnion always has >= 2 distinctly-named variants");
                };
                let __declared: ::std::vec::Vec<(::froglang_core::frontend::typeck::Type, ::std::vec::Vec<::froglang_core::frontend::typeck::Type>)> =
                    ::std::vec![ #( (#marker_exprs, #variant_leaves_to) ),* ];
                let member_leaves = ::froglang_core::codegen::reorder_member_leaves_by_marker(members, &__declared);
                let layout = ::froglang_core::codegen::union_layout_of_leaves(&member_leaves);
                ::froglang_core::codegen::union_columns(&layout, &__union_ty)
            }
            fn to_frog(self, ctx: &mut ::froglang_core::runtime::host::FrogCtx, out: &mut [i64]) {
                let __union_ty = <Self as ::froglang_core::host::ToFrog>::frog_type();
                let ::froglang_core::frontend::typeck::Type::Union(members) = &__union_ty else {
                    unreachable!("FrogUnion always has >= 2 distinctly-named variants");
                };
                let __declared: ::std::vec::Vec<(::froglang_core::frontend::typeck::Type, ::std::vec::Vec<::froglang_core::frontend::typeck::Type>)> =
                    ::std::vec![ #( (#marker_exprs, #variant_leaves_to) ),* ];
                let member_leaves = ::froglang_core::codegen::reorder_member_leaves_by_marker(members, &__declared);
                let (__idx, __leaf_vals): (usize, ::std::vec::Vec<i64>) = match self {
                    #(#to_frog_arms)*
                };
                let __tag = ::froglang_core::codegen::member_tag(__idx);
                let __packed = ::froglang_core::codegen::pack_union_member_runtime_of_leaves(&member_leaves, __idx, __tag, &__leaf_vals);
                out[..__packed.len()].copy_from_slice(&__packed);
            }
        }

        impl ::froglang_core::host::FromFrog for #name {
            fn frog_type() -> ::froglang_core::frontend::typeck::Type {
                <Self as ::froglang_core::host::ToFrog>::frog_type()
            }
            fn leaves() -> ::std::vec::Vec<::froglang_core::frontend::typeck::Type> {
                let __union_ty = <Self as ::froglang_core::host::FromFrog>::frog_type();
                let ::froglang_core::frontend::typeck::Type::Union(members) = &__union_ty else {
                    unreachable!("FrogUnion always has >= 2 distinctly-named variants");
                };
                let __declared: ::std::vec::Vec<(::froglang_core::frontend::typeck::Type, ::std::vec::Vec<::froglang_core::frontend::typeck::Type>)> =
                    ::std::vec![ #( (#marker_exprs, #variant_leaves_from) ),* ];
                let member_leaves = ::froglang_core::codegen::reorder_member_leaves_by_marker(members, &__declared);
                let layout = ::froglang_core::codegen::union_layout_of_leaves(&member_leaves);
                ::froglang_core::codegen::union_columns(&layout, &__union_ty)
            }
            fn from_frog(ctx: &::froglang_core::runtime::host::FrogCtx, slots: &[i64]) -> Self {
                let __union_ty = <Self as ::froglang_core::host::FromFrog>::frog_type();
                let ::froglang_core::frontend::typeck::Type::Union(members) = &__union_ty else {
                    unreachable!("FrogUnion always has >= 2 distinctly-named variants");
                };
                let __declared: ::std::vec::Vec<(::froglang_core::frontend::typeck::Type, ::std::vec::Vec<::froglang_core::frontend::typeck::Type>)> =
                    ::std::vec![ #( (#marker_exprs, #variant_leaves_from) ),* ];
                let member_leaves = ::froglang_core::codegen::reorder_member_leaves_by_marker(members, &__declared);
                let __idx = (slots[0] & ::froglang_core::runtime::gc::TAG_MASK) as usize - 1;
                let __leaf_vals = ::froglang_core::codegen::unpack_union_member_runtime_of_leaves(&member_leaves, __idx, slots);
                let __member_ty = &members[__idx];
                #(#from_frog_arms)*
                unreachable!("FrogUnion::from_frog: tag {} doesn't match any declared variant", __idx);
            }
        }

        impl ::froglang_core::host::FrogDecl for #name {
            #frog_decl_body
        }
    };

    Ok(expanded.into())
}

#[proc_macro_derive(ParseRules, attributes(prefix, infix))]
pub fn derive_parse_rules(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match _derive_parse_rules2(input) {
        Ok(res) => res,
        Err(err) => err.to_compile_error().into()
    }
}

#[proc_macro_derive(Lex, attributes(lex))]
pub fn derive_lex(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match _derive_lex(input) {
        Ok(res) => res,
        Err(err) => err.to_compile_error().into()
    }
}


fn _derive_parse_rules2(input: DeriveInput) -> syn::Result<TokenStream> {
    // Process the enum variants and their attributes
    let mut rules = vec![];
    if let syn::Data::Enum(data) = &input.data {
        for variant in &data.variants {   
            let variant_name = &variant.ident;
            // `None` = this token has no prefix rule, i.e. it cannot start an
            // expression. `Grammar::return_expr` reads this to decide whether a
            // value follows a `return`.
            let mut prefix_fn = quote! { None };
            let mut infix_fn = quote! { Grammar::infix_error };
            let mut precedence = quote! { Precedence::None };     
            for attr in &variant.attrs {
                if attr.path().is_ident("prefix") {
                    let f = extract_prefix_attr(attr)?;
                    prefix_fn = quote! { Some(#f) };
                    // Only value-atom tokens (literals) and dual-role
                    // unary/binary operators (e.g. `-`, which is also
                    // infix `Minus` — that `#[infix(...)]` attr overrides
                    // this default below) need a nonzero default
                    // precedence here. Without an explicit `#[infix(...)]`,
                    // this default is only ever consulted when the token
                    // shows up where a continuation was expected; for a
                    // pure syntax-starting keyword (`if`, `{`, `let`,
                    // `for`, ...) that should mean "the previous
                    // expression just ended here", not "try to parse me
                    // as an operator" — see `Grammar::for_expr`'s doc
                    // comment for the confusing `ExpectedOperator` error
                    // this caused before this check existed.
                    if matches!(prefix_fn_name(attr)?.as_str(), "unary" | "literal") {
                        precedence = quote! { Precedence::Unary };
                    }
                }
                if attr.path().is_ident("infix") {
                    (infix_fn, precedence) = extract_infix_attr(attr)?;
                }
            }

            match &variant.fields {
                // For variants with no data like Token::Plus
                Fields::Unit => {
                    rules.push(quote! {
                        Token::#variant_name => ParseRule { prefix: #prefix_fn, infix: #infix_fn, precedence: #precedence },
                    });
                },
                // For variants with unnamed fields like Token::Number(f64)
                Fields::Unnamed(_) => {
                    rules.push(quote! {
                        Token::#variant_name(_) => ParseRule { prefix: #prefix_fn, infix: #infix_fn, precedence: #precedence },
                    });
                },
                // For variants with named fields
                Fields::Named(_) => {
                    rules.push(quote! {
                        Token::#variant_name { .. } => ParseRule { prefix: #prefix_fn, infix: #infix_fn, precedence: #precedence },
                    });
                }
            }
        }
    }

    let output = quote! {
        impl Grammar {
            pub fn get_parse_rule(token: &Spanned<Token>) -> ParseRule {
                match &token.item {
                    #(#rules)*
                    _ => ParseRule { prefix: None, infix: Grammar::infix_error, precedence: Precedence::None }
                }
            }
        }
    };
    
    // Return ONLY the generated code, not the original
    Ok(output.into())
}


fn extract_prefix_attr(attr: &Attribute) -> syn::Result<proc_macro2::TokenStream> {
    let nested = attr.parse_args_with(Punctuated::<Meta, SynToken![,]>::parse_terminated)?;
    let first = nested.get(0).ok_or_else(|| syn::Error::new(Span::call_site(), "Expected 1 argument to prefix()"))?;
    let rule_name = first.path().to_token_stream();
    Ok(rule_name.clone())
}

/// The last path segment of a `#[prefix(...)]` attribute's function, e.g.
/// `"unary"` for `#[prefix(Grammar::unary)]` — used to decide whether this
/// token's default precedence (absent an explicit `#[infix(...)]`) should
/// be `Unary` or `None`.
fn prefix_fn_name(attr: &Attribute) -> syn::Result<String> {
    let nested = attr.parse_args_with(Punctuated::<Meta, SynToken![,]>::parse_terminated)?;
    let first = nested.get(0).ok_or_else(|| syn::Error::new(Span::call_site(), "Expected 1 argument to prefix()"))?;
    let path = first.path();
    let last = path.segments.last()
        .ok_or_else(|| syn::Error::new(Span::call_site(), "Expected a path argument to prefix()"))?;
    Ok(last.ident.to_string())
}

fn extract_lex_attr(attr: &Attribute) -> syn::Result<String> {
    let literal: syn::LitStr = attr.parse_args()?;
    Ok(literal.value())
}

fn extract_infix_attr(attr: &Attribute) -> syn::Result<(proc_macro2::TokenStream, proc_macro2::TokenStream)> {
    // Expect a form like #[infix(binary, Precedence::Term)]
    let nested = attr.parse_args_with(Punctuated::<Meta, SynToken![,]>::parse_terminated)?;
    let first = nested.get(0).ok_or_else(|| syn::Error::new(Span::call_site(), "Expected 2 arguments to infix()"))?;
    let second = nested.get(1).ok_or_else(|| syn::Error::new(Span::call_site(), "Expected 2 arguments to infix()"))?;
    let rule_name = first.path().to_token_stream();
    let precedence = second.path().to_token_stream();

    Ok((rule_name, precedence))
}

fn _derive_lex(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let variants = match &input.data {
        syn::Data::Enum(data) => &data.variants,
        _ => panic!("LexerSupport can only be derived for enums"),
    };

    let mut keyword_arms = Vec::new();
    let mut operator_arms = Vec::new();
    let mut display_arms = Vec::new();

    for variant in variants {
        for attr in &variant.attrs {
            if attr.path().is_ident("lex") {
                let variant_ident = &variant.ident;
                let literal_str = extract_lex_attr(attr)?;
                let first_char = literal_str.chars().next().expect("Empty lex attribute");
                if first_char.is_alphabetic() {
                    keyword_arms.push(quote! { #literal_str => Some(Self::#variant_ident) });
                }
                else if !first_char.is_alphanumeric() {
                    operator_arms.push(quote! { #literal_str => Some(Self::#variant_ident) });
                }

                let display_arm = quote! { Self::#variant_ident => write!(f, "{}", #literal_str) };
                display_arms.push(display_arm);
            }
        }
    }

    let output = quote! {
        impl #name {
            /// Matches a string slice to a multi-character keyword token.
            pub fn from_keyword(s: &str) -> Option<Self> {
                match s {
                    #(#keyword_arms,)*
                    _ => None,
                }
            }

            /// Matches a single character to a simple punctuation/operator token.
            pub fn from_operator(s: &str) -> Option<Self> {
                match s {
                    #(#operator_arms,)*
                    _ => None,
                }
            }

            pub fn display(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    #(#display_arms,)*
                    _ => write!(f, "<??>")
                }
            }
        }
    };

    Ok(output.into())
} 