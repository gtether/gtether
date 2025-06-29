use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use regex::Regex;
use syn::{Ident, LitFloat, LitStr, Result, Token};
use syn::parse::{Parse, ParseStream};

pub struct TestNetDriverClientServer {
    test_name: Ident,
    stack_factory_ident: Ident,
    timeout: f32,
    test_attr_ident: Ident,
    include_pattern: Option<Regex>,
    exclude_pattern: Option<Regex>,
}

impl Parse for TestNetDriverClientServer {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut test_name: Option<Ident> = None;
        let mut stack_factory_ident: Option<Ident> = None;
        let mut timeout = 0.1;
        let mut test_attr_ident = format_ident!("test");
        let mut include_pattern: Option<Regex> = None;
        let mut exclude_pattern: Option<Regex> = None;
        
        while !input.is_empty() {
            let key_ident = input.parse::<Ident>()?;
            input.parse::<Token![:]>()?;
            match key_ident.to_string().as_str() {
                "name" => {
                    test_name = Some(input.parse::<Ident>()?);
                },
                "stack_factory" => {
                    stack_factory_ident = Some(input.parse::<Ident>()?);
                },
                "timeout" => {
                    timeout = input.parse::<LitFloat>()?.base10_parse()?;
                },
                "test_attr" => {
                    test_attr_ident = input.parse::<Ident>()?;
                },
                "include" => {
                    let pattern = input.parse::<LitStr>()?.value();
                    include_pattern = Some(Regex::new(&pattern)
                        .map_err(|e| {
                            input.error(format!("Failed to compile 'include' regex: {e}"))
                        })?);
                },
                "exclude" => {
                    let pattern = input.parse::<LitStr>()?.value();
                    exclude_pattern = Some(Regex::new(&pattern)
                        .map_err(|e| {
                            input.error(format!("Failed to compile 'exclude' regex: {e}"))
                        })?);
                },
                key => return Err(input.error(format!("Unknown key: {key}"))),
            }
            
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        
        let test_name = test_name
            .ok_or(input.error("'name' is required"))?;
        let stack_factory_ident = stack_factory_ident
            .ok_or(input.error("'stack_factory' is required"))?;

        Ok(Self {
            test_name,
            stack_factory_ident,
            timeout,
            test_attr_ident,
            include_pattern,
            exclude_pattern,
        })
    }
}

impl TestNetDriverClientServer {
    fn gen_tests_from_suite(
        &self,
        crate_ident: &Ident,
        test_names: impl IntoIterator<Item=impl AsRef<str>>,
    ) -> TokenStream {
        let stack_factory_ident = &self.stack_factory_ident;
        let timeout = self.timeout;
        let test_tag_ident = &self.test_attr_ident;

        test_names.into_iter()
            .filter(|name| {
                let name = name.as_ref();
                
                if let Some(include_pattern) = &self.include_pattern {
                    if !include_pattern.is_match(name) {
                        return false;
                    }
                }
                
                if let Some(exclude_pattern) = &self.exclude_pattern {
                    if exclude_pattern.is_match(name) {
                        return false;
                    }
                }
                
                true
            })
            .map(|name| {
                let name = name.as_ref();
    
                let test_name = format_ident!("test_{}_{}", self.test_name, name);
                let test_method_name = format_ident!("test_{}", name);
    
                quote! {
                    #[#test_tag_ident]
                    fn #test_name() {
                        let test_suite = ::#crate_ident::net::driver::tests::suites
                            ::ClientServer::<#stack_factory_ident>::default();
                        test_suite.#test_method_name(std::time::Duration::from_secs_f32(#timeout));
                    }
                }
            })
            .collect::<TokenStream>()
    }

    pub fn gen_core_tests(&self, crate_ident: &Ident) -> Result<TokenStream> {
        Ok(self.gen_tests_from_suite(
            crate_ident,
            [
                "connect",
                "listen_already_bound",
                "connect_cancelled",
                "connect_after_close",
            ],
        ))
    }

    pub fn gen_send_tests(&self, crate_ident: &Ident) -> Result<TokenStream> {
        Ok(self.gen_tests_from_suite(
            crate_ident,
            [
                "send",
                "send_many",
                "send_closed_src",
                "send_closed_dst",
                "send_closed_partial",
                "send_recv",
                "send_recv_many",
            ],
        ))
    }

    pub fn gen_send_to_tests(&self, crate_ident: &Ident) -> Result<TokenStream> {
        Ok(self.gen_tests_from_suite(
            crate_ident,
            [
                "send_to",
                "send_to_many",
                "send_to_closed_src",
                "send_to_closed_dst",
                "send_to_closed_partial",
                "send_to_invalid_connection",
                "send_recv_to",
                "send_recv_to_many",
            ],
        ))
    }

    pub fn gen_broadcast_tests(&self, crate_ident: &Ident) -> Result<TokenStream> {
        Ok(self.gen_tests_from_suite(
            crate_ident,
            [
                "broadcast",
                "broadcast_closed_src",
                "broadcast_closed_dst",
                "broadcast_closed_partial",
            ],
        ))
    }
}