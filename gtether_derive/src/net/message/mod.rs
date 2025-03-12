use proc_macro2::{Literal, TokenStream};
use quote::quote;
use syn::{DeriveInput, Error, Ident, Result};

pub fn derive_message_body(crate_ident: &Ident, ast: DeriveInput) -> Result<TokenStream> {
    let struct_ident = &ast.ident;
    let struct_name = Literal::string(&struct_ident.to_string());

    let flags = ast.attrs.iter()
        .filter(|a| a.path().segments.len() == 1 && a.path().segments[0].ident == "message_flag")
        .map(|a| {
            let flag: Ident = a.parse_args()?;
            Ok(flag)
        })
        .collect::<Result<Vec<_>>>()?;

    let flag_tokens = if flags.is_empty() {
        TokenStream::new()
    } else {
        quote! {
            #[inline]
            fn flags() -> ::#crate_ident::util::FlagSet<::#crate_ident::net::message::MessageFlags> {
                (#( ::#crate_ident::net::message::MessageFlags::#flags )|*).into()
            }
        }
    };

    let message_body = quote! {
        impl ::#crate_ident::net::message::MessageBody for #struct_ident {
            const KEY: &'static str = #struct_name;
            #flag_tokens
        }
    };

    let reply_attr = ast.attrs.iter()
        .filter(|a| a.path().segments.len() == 1 && a.path().segments[0].ident == "message_reply")
        .nth(0);

    let message_repliable = if let Some(reply_attr) = reply_attr {
        let reply_type: Ident = reply_attr.parse_args()?;
        quote! {
            impl ::#crate_ident::net::message::MessageRepliable for #struct_ident {
                type Reply = #reply_type;
            }
        }
    } else {
        TokenStream::new()
    };

    Ok(quote! {
        #message_body
        #message_repliable
    })
}

// TODO: Merge with above?
pub fn derive_message_repliable(crate_ident: &Ident, ast: DeriveInput) -> Result<TokenStream> {
    let struct_ident = &ast.ident;

    let attribute = ast.attrs.iter()
        .filter(|a| a.path().segments.len() == 1 && a.path().segments[0].ident == "message_reply")
        .nth(0)
        .ok_or(Error::new(struct_ident.span(), "'message_reply' attribute is required"))?;

    let reply_type: Ident = attribute.parse_args()?;

    Ok(quote! {
        impl ::#crate_ident::net::message::MessageRepliable for #struct_ident {
            type Reply = #reply_type;
        }
    })
}