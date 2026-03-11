// lang-macros/src/lib.rs
use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{quote, ToTokens};
use syn::{parse_macro_input, punctuated::Punctuated, Attribute, DeriveInput, Meta};
use syn::{Fields, Token as SynToken};

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
            let mut prefix_fn = quote! { Grammar::prefix_error };
            let mut infix_fn = quote! { Grammar::infix_error };
            let mut precedence = quote! { Precedence::None };     
            for attr in &variant.attrs {
                if attr.path().is_ident("prefix") {
                    prefix_fn = extract_prefix_attr(attr)?;
                    precedence = quote! { Precedence::Unary };                      
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
                    _ => ParseRule { prefix: Grammar::prefix_error, infix: Grammar::infix_error, precedence: Precedence::None }
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