use proc_macro::TokenStream;
use proc_macro_crate::{crate_name, FoundCrate};
use syn::{parse_macro_input, DeriveInput, Error};

mod net;

#[proc_macro_derive(MessageBody, attributes(message_flag, message_reply))]
pub fn derive_message_body(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let crate_ident = crate_ident();

    net::message::derive_message_body(&crate_ident, ast)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn crate_ident() -> syn::Ident {
    let found_crate = crate_name("gtether").unwrap();
    let name = match &found_crate {
        FoundCrate::Itself => "gtether",
        FoundCrate::Name(name) => name,
    };

    syn::Ident::new(name, proc_macro2::Span::call_site())
}