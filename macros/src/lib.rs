use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, AttributeArgs, Ident, ItemStruct, ItemTrait};

#[proc_macro_attribute]
pub fn connection_state(_attr: TokenStream, input: TokenStream) -> TokenStream {
    let input_ast = parse_macro_input!(input as ItemStruct);
    let state_ident = &input_ast.ident;
    let vis = &input_ast.vis;

    let field_names: Vec<&Ident> = input_ast
        .fields
        .iter()
        .map(|field| field.ident.as_ref().unwrap())
        .collect();

    let field_indices: Vec<usize> = (0..field_names.len()).collect();

    let expanded = quote! {
        #[derive(Clone, Default, Debug)]
        #input_ast

        #vis struct StateController {
            state: parking_lot::Mutex<#state_ident>,
            channel: std::sync::Arc<tokio::sync::mpsc::Sender<Vec<(usize, Vec<u8>)>>>,
        }

        impl StateController {
            fn new(channel: StateUpdateChannel) -> Self {
                Self {
                    state: parking_lot::Mutex::new(Default::default()),
                    channel: std::sync::Arc::new(channel),
                }
            }

            #vis fn lock(&self) -> StateGuard {
                let state = self.state.lock();
                StateGuard {
                    starting_state: state.clone(),
                    state,
                    channel: self.channel.clone(),
                }
            }
        }

        #vis struct StateGuard<'a> {
            state: parking_lot::MutexGuard<'a, #state_ident>,
            starting_state: #state_ident,
            channel: std::sync::Arc<tokio::sync::mpsc::Sender<Vec<(usize, Vec<u8>)>>>,
        }

        impl<'a> Drop for StateGuard<'a> {
            /// Our custom drop implementation will send any changes to the runtime
            fn drop(&mut self) {
                // "diff" the two states to see what changed
                let mut changes = Vec::new();

                #(
                    if self.state.#field_names != self.starting_state.#field_names {
                        changes.push((
                            #field_indices,
                            rkyv::to_bytes::<_, 1024>(&self.state.#field_names).unwrap().to_vec(),
                        ));
                    }
                )*

                // if there are no changes, don't bother sending anything
                if changes.is_empty() {
                    return;
                }

                // send the changes to the runtime
                // we have to spawn a new task because we can't await inside a drop
                let channel = self.channel.clone();
                tokio::spawn(async move {
                    // this could fail if the server shuts down before these
                    // changes are sent... but we're not too worried about that
                    let _ = channel.send(changes).await;
                });
            }
        }

        impl std::ops::Deref for StateGuard<'_> {
            type Target = #state_ident;

            fn deref(&self) -> &Self::Target {
                &self.state
            }
        }

        impl std::ops::DerefMut for StateGuard<'_> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.state
            }
        }

        impl ClientState for State {
            fn apply_changes(
                &mut self,
                changes: Vec<(usize, Vec<u8>)>,
            ) -> HandlerResult<()> {
                for (field_index, new_value) in changes {
                    match field_index {
                        #(
                            #field_indices => {
                                self.#field_names = rkyv::from_bytes(&new_value)
                                    .map_err(|_| RpcHandlerError::BadInputBytes)?;
                            }
                        ),*
                        _ => {}
                    }
                }
                Ok(())
            }
        }
    };

    TokenStream::from(expanded)
}

fn snake_to_pascal_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;

    s.chars().for_each(|c| match c {
        '_' => capitalize_next = true,
        _ if capitalize_next => {
            result.extend(c.to_uppercase());
            capitalize_next = false;
        }
        _ => result.push(c),
    });

    result
}

#[proc_macro_attribute]
pub fn rpc(args: TokenStream, input: TokenStream) -> TokenStream {
    let _args = parse_macro_input!(args as AttributeArgs);
    let trait_input = parse_macro_input!(input as ItemTrait);
    let vis = &trait_input.vis;

    let trait_ident = &trait_input.ident;

    // // rewrite trait input so all outputs are wrapped in HandlerResult
    // let trait_items = trait_input.clone()
    //     .items
    //     .iter()
    //     .map(|item| {
    //         if let syn::TraitItem::Method(method) = item {
    //             let output = &method.sig.output;
    //             let output = quote! {
    //                 -> HandlerResult<#output>
    //             };
    //             let mut method = method.clone();
    //             method.sig.output = syn::parse2(output).unwrap();
    //             syn::TraitItem::Method(method)
    //         } else {
    //             item.clone()
    //         }
    //     })
    //     .collect::<Vec<_>>();

    // let trait_input = syn::ItemTrait {
    //     attrs: trait_input.attrs,
    //     vis: vis.clone(),
    //     unsafety: trait_input.unsafety,
    //     auto_token: trait_input.auto_token,
    //     trait_token: trait_input.trait_token,
    //     ident: trait_ident.clone(),
    //     generics: trait_input.generics,
    //     colon_token: trait_input.colon_token,
    //     supertraits: trait_input.supertraits,
    //     brace_token: trait_input.brace_token,
    //     items: trait_items,
    // };

    // Generate RpcCall enum variants
    let rpc_variants = trait_input
        .items
        .iter()
        .filter_map(|item| {
            if let syn::TraitItem::Method(method) = item {
                let variant_ident = {
                    let s = method.sig.ident.to_string();
                    format_ident!("{}", snake_to_pascal_case(&s))
                };

                // inputs need to be like ident: type so we can use them in the
                // enum
                let inputs = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|input| {
                        if let syn::FnArg::Typed(typed) = input {
                            if let syn::Pat::Ident(ident) = &*typed.pat {
                                let ty = &typed.ty;
                                Some(quote! {
                                    #ident: #ty
                                })
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                Some(quote! {
                    #variant_ident { #(#inputs),* }
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let shared_code = quote! {
        #[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
        #[archive_attr(derive(::hardlight::bytecheck::CheckBytes))]
        #[archive(crate = "::hardlight::rkyv")]
        #[repr(u8)]
        #vis enum RpcCall {
            #(#rpc_variants),*
        }
    };

    let application_server = {
        let server_struct_ident = format_ident!("{}Server", trait_ident);

        quote! {
            #vis struct #server_struct_ident {
                config: ServerConfig,
                shutdown: Option<tokio::sync::oneshot::Sender<()>>,
                control_channels: Option<()>,
            }

            #[async_trait::async_trait]
            impl ApplicationServer for #server_struct_ident {
                fn new(config: ServerConfig) -> Self {
                    Self {
                        config,
                        shutdown: None,
                        control_channels: None,
                    }
                }

                async fn start(&mut self) -> Result<(), std::io::Error> {
                    let server = Server::new(self.config.clone(), Handler::init());
                    let (error_tx, error_rx) = tokio::sync::oneshot::channel();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (control_channels_tx, control_channels_rx) = tokio::sync::oneshot::channel();

                    tokio::spawn(async move {
                        if let Err(e) = server.run(shutdown_rx, control_channels_tx).await {
                            error_tx.send(e).unwrap()
                        };
                    });

                    tokio::select! {
                        e = error_rx => {
                            tracing::error!("Server error: {:?}", e);
                            return Err(e.unwrap());
                        }
                        control_channels = control_channels_rx => {
                            self.control_channels = Some(control_channels.unwrap());
                            self.shutdown = Some(shutdown_tx);
                            Ok(())
                        }
                    }
                }
                fn stop(&mut self) {
                    tracing::debug!("Telling server to shutdown");
                    match self.shutdown.take() {
                        Some(shutdown) => {
                            let _ = shutdown.send(());
                        }
                        None => {}
                    }
                }
            }
        }
    };

    let server_handler = {
        let server_methods = trait_input
            .items
            .iter()
            .filter_map(|item| {
                if let syn::TraitItem::Method(method) = item {
                    let method_ident = &method.sig.ident;
                    let variant_ident = {
                        let s = method.sig.ident.to_string();
                        format_ident!("{}", snake_to_pascal_case(&s))
                    };
                    let inputs = method
                        .sig
                        .inputs
                        .iter()
                        .filter_map(|arg| match arg {
                            syn::FnArg::Typed(pat) => Some(&pat.pat),
                            _ => None,
                        })
                        .collect::<Vec<_>>();

                    Some(quote! {
                        RpcCall::#variant_ident { #(#inputs),* } => {
                            let result = self.#method_ident(#(#inputs),*).await?;
                            let result = rkyv::to_bytes::<_, 1024>(&result).unwrap();
                            Ok(result.to_vec())
                        }
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        quote! {
            /// RPC server that implements the [Counter] trait. A wrapper around
            /// [Server]
            #vis struct Handler {
                // the runtime will provide the state when it creates the handler
                #vis state: std::sync::Arc<StateController>,
            }

            impl Handler {
                /// An easier way to get the channel factory
                fn init(
                ) -> impl Fn(StateUpdateChannel) -> Box<dyn ServerHandler + Send + Sync>
                       + Send
                       + Sync
                       + 'static
                       + Copy {
                    |state_update_channel| Box::new(Self::new(state_update_channel))
                }
            }

            #[async_trait::async_trait]
            impl ServerHandler for Handler {
                fn new(state_update_channel: StateUpdateChannel) -> Self {
                    Self {
                        state: std::sync::Arc::new(StateController::new(state_update_channel)),
                    }
                }

                async fn handle_rpc_call(
                    &self,
                    input: &[u8],
                ) -> Result<Vec<u8>, RpcHandlerError> {
                    let call: RpcCall = rkyv::from_bytes(input)
                        .map_err(|_| RpcHandlerError::BadInputBytes)?;

                    match call {
                        #(#server_methods),*
                    }
                }
            }
        }
    };

    let application_client = {
        // Generate the client code
        let client_name = format_ident!("{}Client", trait_ident);

        let mut client_methods = Vec::new();
        for item in &trait_input.items {
            if let syn::TraitItem::Method(method) = item {
                let method_ident = &method.sig.ident;
                let method_inputs = &method.sig.inputs;
                let method_output = &method.sig.output;
                let attr = &method.attrs;

                let rpc_call_variant = {
                    let s = method_ident.to_string();
                    format_ident!("{}", snake_to_pascal_case(&s))
                };

                let rpc_call_params = method_inputs
                    .iter()
                    .filter_map(|arg| match arg {
                        syn::FnArg::Typed(pat_type) => Some(&pat_type.pat),
                        _ => None,
                    })
                    .collect::<Vec<_>>();

                let client_method = quote! {
                    #(#attr)*
                    async fn #method_ident(#method_inputs) #method_output {
                        match self.make_rpc_call(RpcCall::#rpc_call_variant { #(#rpc_call_params),* }).await {
                            Ok(c) => rkyv::from_bytes(&c)
                                .map_err(|_| RpcHandlerError::BadOutputBytes),
                            Err(e) => Err(e),
                        }
                    }
                };

                client_methods.push(client_method);
            }
        }
        
        #[cfg(not(feature = "disable-self-signed"))]
        let client_new = quote! {
            fn new_self_signed(host: &str, compression: Compression) -> Self {
                Self {
                    host: host.to_string(),
                    self_signed: true,
                    shutdown: None,
                    rpc_tx: None,
                    compression,
                }
            }

            fn new(host: &str, compression: Compression) -> Self {
                Self {
                    host: host.to_string(),
                    self_signed: false,
                    shutdown: None,
                    rpc_tx: None,
                    compression,
                }
            }
        };
        
        #[cfg(feature = "disable-self-signed")]
        let client_new = quote! {
            fn new(host: &str, compression: Compression) -> Self {
                Self {
                    host: host.to_string(),
                    self_signed: false,
                    shutdown: None,
                    rpc_tx: None,
                    compression,
                }
            }
        };

        quote! {
            // CLIENT CODE
            #vis struct #client_name {
                host: String,
                self_signed: bool,
                shutdown: Option<tokio::sync::oneshot::Sender<()>>,
                rpc_tx: Option<tokio::sync::mpsc::Sender<(Vec<u8>, RpcResponseSender)>>,
                compression: Compression,
            }

            #[async_trait::async_trait]
            impl ApplicationClient for #client_name {
                #client_new

                /// Spawns a runtime client in the background to maintain the active
                /// connection
                async fn connect(&mut self) -> Result<(), tungstenite::Error> {
                    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (control_channels_tx, control_channels_rx) = tokio::sync::oneshot::channel();
                    let (error_tx, error_rx) = tokio::sync::oneshot::channel();

                    let self_signed = self.self_signed;
                    let host = self.host.clone();
                    let compression = self.compression;

                    tokio::spawn(async move {
                        let mut client: Client<State> = if self_signed {
                            Client::new_self_signed(&host, compression)
                        } else {
                            Client::new(&host, compression)
                        };

                        if let Err(e) =
                            client.connect(shutdown_rx, control_channels_tx).await
                        {
                            error_tx.send(e).unwrap()
                        };
                    });

                    tokio::select! {
                        Ok((rpc_tx,)) = control_channels_rx => {
                            // at this point, the client will NOT return any errors, so we
                            // can safely ignore the error_rx channel
                            tracing::debug!("Received control channels from client");
                            self.shutdown = Some(shutdown);
                            self.rpc_tx = Some(rpc_tx);
                            Ok(())
                        }
                        e = error_rx => {
                            tracing::error!("Error received from client: {:?}", e);
                            Err(e.unwrap())
                        }
                    }
                }

                fn disconnect(&mut self) {
                    tracing::debug!("Telling client to shutdown");
                    match self.shutdown.take() {
                        Some(shutdown) => {
                            let _ = shutdown.send(());
                        }
                        None => {}
                    }
                }
            }

            impl #client_name {
                async fn make_rpc_call(&self, call: RpcCall) -> HandlerResult<Vec<u8>> {
                    if let Some(rpc_chan) = self.rpc_tx.clone() {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        rpc_chan
                            .send((
                                rkyv::to_bytes::<RpcCall, 1024>(&call)
                                    .map_err(|_| RpcHandlerError::BadInputBytes)?
                                    .to_vec(),
                                tx,
                            ))
                            .await
                            .unwrap();
                        rx.await.unwrap()
                    } else {
                        Err(RpcHandlerError::ClientNotConnected)
                    }
                }
            }

            impl Drop for #client_name {
                fn drop(&mut self) {
                    tracing::debug!("Application client got dropped. Disconnecting.");
                    self.disconnect();
                }
            }

            #[async_trait::async_trait]
            impl #trait_ident for #client_name {
                #(#client_methods)*
            }
        }
    };

    let expanded = quote! {
        #[async_trait::async_trait]
        #trait_input
        #shared_code
        #application_server
        #server_handler
        #application_client
    };

    TokenStream::from(expanded)
}

#[proc_macro_attribute]
/// This attribute is used to mark a struct as an RPC handler. Currently,
/// this just adds the `#[async_trait::async_trait]` attribute to the struct.
pub fn rpc_handler(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as syn::ItemImpl);

    let expanded = quote! {
        #[async_trait::async_trait]
        #input
    };

    proc_macro::TokenStream::from(expanded)
}

#[proc_macro_attribute]
/// Takes any ast as an input and annotates it with useful attributes for data
/// serialization and deserialization. This consists of `Archive + Serialize + Deserialize`,
/// for the root type and `CheckBytes` for the archived version.
pub fn codable(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as syn::Item);

    let expanded = quote! {
        #[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
        #[archive(crate = "::hardlight::rkyv", check_bytes)]
        // #[archive_attr(derive(::hardlight::bytecheck::CheckBytes))]
        // #[archive_attr(check_bytes(bytecheck_crate = "::hardlight::bytecheck"))]
        #input
    };

    proc_macro::TokenStream::from(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snake_to_pascal_case_empty() {
        assert_eq!(snake_to_pascal_case(""), "");
    }

    #[test]
    fn test_snake_to_pascal_case_single_word() {
        assert_eq!(snake_to_pascal_case("hello"), "Hello");
        assert_eq!(snake_to_pascal_case("world"), "World");
    }

    #[test]
    fn test_snake_to_pascal_case_multiple_words() {
        assert_eq!(snake_to_pascal_case("hello_world"), "HelloWorld");
        assert_eq!(snake_to_pascal_case("foo_bar_baz"), "FooBarBaz");
    }

    #[test]
    fn test_snake_to_pascal_case_mixed_case() {
        assert_eq!(snake_to_pascal_case("hello_world_42"), "HelloWorld42");
        assert_eq!(snake_to_pascal_case("foo_BAR_baz"), "FooBarBaz");
    }
}
