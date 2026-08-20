// SPDX-License-Identifier: Apache-2.0
//! Thin verb-key derives for Heddle CLI args and output types.
//!
//! These derives do **not** synthesize `clap::Args` or `schemars::JsonSchema`.
//! Those stay as explicit sibling derives. Each derive only stamps
//! `HEDDLE_VERB` so clap, the command catalog, and the schema registry
//! share one construction-time key.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{DeriveInput, Error, LitStr, parse_macro_input};

/// Stamp `HEDDLE_VERB` on a clap args struct.
///
/// The annotated type must still `#[derive(clap::Args)]`. This derive only
/// records the verb key used by catalog and schema pairing tests.
#[proc_macro_derive(HeddleVerbArgs, attributes(heddle_verb))]
pub fn derive_heddle_verb_args(input: TokenStream) -> TokenStream {
    expand_verb_const(input)
}

/// Stamp `HEDDLE_VERB` on a JSON output struct.
///
/// The annotated type must still `#[derive(schemars::JsonSchema)]`. This
/// derive only records the verb key that the schema registry and catalog
/// must use for the same command.
#[proc_macro_derive(HeddleVerbOutput, attributes(heddle_verb))]
pub fn derive_heddle_verb_output(input: TokenStream) -> TokenStream {
    expand_verb_const(input)
}

fn expand_verb_const(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match verb_impl(&input) {
        Ok(tokens) => tokens,
        Err(error) => error.to_compile_error().into(),
    }
}

fn verb_impl(input: &DeriveInput) -> syn::Result<TokenStream> {
    let verb = parse_heddle_verb(&input.attrs)?;
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics #ident #ty_generics #where_clause {
            /// Catalog / schema verb key for this type.
            pub const HEDDLE_VERB: &'static str = #verb;
        }
    }
    .into())
}

fn parse_heddle_verb(attrs: &[syn::Attribute]) -> syn::Result<String> {
    let mut found = None;
    for attr in attrs {
        if !attr.path().is_ident("heddle_verb") {
            continue;
        }
        if found.is_some() {
            return Err(Error::new_spanned(
                attr,
                "duplicate #[heddle_verb] attribute",
            ));
        }
        let lit: LitStr = attr.parse_args()?;
        let value = lit.value();
        if value.trim().is_empty() {
            return Err(Error::new_spanned(
                lit,
                "#[heddle_verb] requires a non-empty verb key",
            ));
        }
        found = Some(value);
    }
    found.ok_or_else(|| {
        Error::new(
            Span::call_site(),
            "missing #[heddle_verb(\"verb\")] attribute",
        )
    })
}
