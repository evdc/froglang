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
    // here) so a future non-single-slot `FromFrog` impl — a struct or union
    // argument — needs no change to this macro, only to that impl's
    // `SLOTS`. See `host.rs`'s `FromFrog`/`ToFrog` doc comments.
    let mut offsets: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut cum = quote! { 0usize };
    for ty in &param_types {
        offsets.push(cum.clone());
        cum = quote! { (#cum + <#ty as ::froglang_core::host::FromFrog>::SLOTS) };
    }
    let total_arg_slots = cum;

    // Per-slot GC-pointer mask for the flattened argument buffer, so the
    // shim only roots slots that actually hold a heap pointer — a raw
    // `Int`/`Float` slot has no tag bits and must never be mistaken for one
    // (see `RuntimeRoots::hold_masked`'s doc comment). Built at shim-call
    // time (not macro-expansion time) since `SLOTS` is an associated const
    // this macro never evaluates itself.
    let arg_ptr_mask_pushes = param_types.iter().map(|ty| {
        quote! {
            __frog_ptr_mask.extend(::std::iter::repeat(
                ::froglang_core::codegen::is_heap_ty(&<#ty as ::froglang_core::host::FromFrog>::frog_type())
            ).take(<#ty as ::froglang_core::host::FromFrog>::SLOTS));
        }
    });

    let unmarshal_args = param_idents.iter().zip(param_types.iter()).zip(offsets.iter()).map(|((ident, ty), start)| {
        quote! {
            let #ident = <#ty as ::froglang_core::host::FromFrog>::from_frog(
                ctx, &__frog_args[(#start)..(#start + <#ty as ::froglang_core::host::FromFrog>::SLOTS)],
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
                ::std::slice::from_raw_parts_mut(__frog_out_ptr, <#ret_ty as ::froglang_core::host::ToFrog>::SLOTS)
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
            }
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