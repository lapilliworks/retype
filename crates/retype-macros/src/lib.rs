//! Derive macros for retype.
//!
//! One so far: [`CodeName`], which turns variant names into the error codes the
//! product documents, so that a variant name is the only place a code is
//! written.

use proc_macro::TokenStream;

use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, parse_macro_input};

/// The prefix every code carries, `docs/errors.md` §3.
const PREFIX: &str = "E_";

/// Derives `as_str`, `parse`, `KNOWN` and `Display` from the variant names.
///
/// `IoPermission` becomes `E_IO_PERMISSION`. One variant may be marked
/// `#[code(passthrough)]` to hold a code from another version: it must be a
/// newtype, it prints and reads as its inner string undecorated, and it is
/// excluded from `KNOWN`.
#[proc_macro_derive(CodeName, attributes(code))]
pub fn code_name(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);

	match expand(&input) {
		Ok(tokens) => tokens.into(),
		Err(error) => error.to_compile_error().into(),
	}
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
	let name = &input.ident;
	let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

	let Data::Enum(data) = &input.data else {
		return Err(syn::Error::new_spanned(input, "CodeName only derives for enums"));
	};

	let mut known = Vec::new();
	let mut passthrough: Option<&Ident> = None;

	for variant in &data.variants {
		let is_passthrough = variant.attrs.iter().any(|attribute| {
			attribute.path().is_ident("code") && attribute.parse_args::<Ident>().is_ok_and(|arg| arg == "passthrough")
		});

		if is_passthrough {
			if passthrough.is_some() {
				return Err(syn::Error::new_spanned(
					variant,
					"only one passthrough variant is allowed",
				));
			}
			if !matches!(&variant.fields, Fields::Unnamed(fields) if fields.unnamed.len() == 1) {
				return Err(syn::Error::new_spanned(
					variant,
					"a passthrough variant must hold exactly one field",
				));
			}
			passthrough = Some(&variant.ident);
			continue;
		}

		if !matches!(variant.fields, Fields::Unit) {
			return Err(syn::Error::new_spanned(
				variant,
				"only a passthrough variant may hold data",
			));
		}

		known.push(&variant.ident);
	}

	if known.is_empty() {
		return Err(syn::Error::new_spanned(
			input,
			"an enum of codes needs at least one code",
		));
	}

	let codes: Vec<String> = known.iter().map(|ident| code_for(&ident.to_string())).collect();

	let unknown_arm = passthrough.map(|ident| quote! { Self::#ident(raw) => raw, });
	let unknown_parse = passthrough.map(|_| quote! { _ => None });

	Ok(quote! {
		impl #impl_generics #name #ty_generics #where_clause {
			/// The code as text, borrowed, which is how it is compared and logged.
			pub fn as_str(&self) -> &str {
				match self {
					#(Self::#known => #codes,)*
					#unknown_arm
				}
			}

			/// A known code, or none if this build has never heard of it.
			pub fn parse(raw: &str) -> Option<Self> {
				match raw {
					#(#codes => Some(Self::#known),)*
					#unknown_parse
				}
			}

			/// Every code this build knows, in declaration order.
			pub const KNOWN: &'static [Self] = &[#(Self::#known,)*];
		}

		impl #impl_generics ::core::fmt::Display for #name #ty_generics #where_clause {
			fn fmt(&self, formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
				formatter.write_str(self.as_str())
			}
		}
	})
}

/// The code for a variant name: the name in screaming snake case, with the
/// product prefix. A name that carries no boundary needs no separator, so
/// `Noop` becomes `NOOP`.
fn code_for(variant: &str) -> String {
	let characters: Vec<char> = variant.chars().collect();
	let mut out = String::with_capacity(PREFIX.len() + variant.len() + 4);
	out.push_str(PREFIX);

	for (index, character) in characters.iter().enumerate() {
		if character.is_uppercase() && index > 0 {
			let previous = characters[index - 1];
			let next_is_lower = characters.get(index + 1).is_some_and(|next| next.is_lowercase());
			if previous.is_lowercase() || previous.is_ascii_digit() || (previous.is_uppercase() && next_is_lower) {
				out.push('_');
			}
		}
		out.extend(character.to_uppercase());
	}

	out
}

#[cfg(test)]
mod tests {
	use super::code_for;

	#[test]
	fn a_single_word_keeps_its_shape() {
		assert_eq!(code_for("Anchor"), "E_ANCHOR");
		assert_eq!(code_for("Noop"), "E_NOOP");
	}

	#[test]
	fn words_are_separated() {
		assert_eq!(code_for("IoPermission"), "E_IO_PERMISSION");
		assert_eq!(code_for("ValueUnexpected"), "E_VALUE_UNEXPECTED");
		assert_eq!(code_for("JobResumeUnavailable"), "E_JOB_RESUME_UNAVAILABLE");
	}

	#[test]
	fn acronym_runs_stay_together() {
		assert_eq!(code_for("IpcInternal"), "E_IPC_INTERNAL");
		assert_eq!(code_for("CfgNameClash"), "E_CFG_NAME_CLASH");
	}

	#[test]
	fn digits_and_acronym_ends_split_where_they_should() {
		assert_eq!(code_for("IpcIpV6"), "E_IPC_IP_V6");
		assert_eq!(code_for("IoEof"), "E_IO_EOF");
	}
}
