use proc_macro::TokenStream;
use proc_macro_crate::{crate_name, FoundCrate};
use syn::{parse_macro_input, DeriveInput, Error};

use crate::net::driver::TestNetDriverClientServer;

mod net;

#[proc_macro_derive(MessageBody, attributes(message_flag, message_reply))]
pub fn derive_message_body(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let crate_ident = crate_ident();

    net::message::derive_message_body(&crate_ident, ast)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro]
pub fn test_net_driver_client_server_core(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as TestNetDriverClientServer);
    let crate_ident = crate_ident();

    ast.gen_core_tests(&crate_ident)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro]
pub fn test_net_driver_client_server_send(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as TestNetDriverClientServer);
    let crate_ident = crate_ident();

    ast.gen_send_tests(&crate_ident)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro]
pub fn test_net_driver_client_server_send_to(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as TestNetDriverClientServer);
    let crate_ident = crate_ident();

    ast.gen_send_to_tests(&crate_ident)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro]
pub fn test_net_driver_client_server_broadcast(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as TestNetDriverClientServer);
    let crate_ident = crate_ident();

    ast.gen_broadcast_tests(&crate_ident)
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