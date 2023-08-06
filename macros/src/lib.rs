use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Ident, ItemStruct, ItemTrait};

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
            state: ::hardlight::tokio::sync::RwLock<#state_ident>,
            channel: std::sync::Arc<::hardlight::tokio::sync::mpsc::Sender<Vec<StateUpdate>>>,
        }

        impl StateController {
            #vis fn new(channel: ::hardlight::StateUpdateChannel) -> Self {
                Self {
                    state: ::hardlight::tokio::sync::RwLock::new(Default::default()),
                    channel: std::sync::Arc::new(channel),
                }
            }

            #vis async fn write(&self) -> StateGuard {
                let state = self.state.write().await;
                StateGuard {
                    starting_state: state.clone(),
                    state,
                    channel: self.channel.clone(),
                }
            }

            #vis async fn read(&self) -> ::hardlight::tokio::sync::RwLockReadGuard<'_, #state_ident> {
                self.state.read().await
            }
        }

        #vis struct StateGuard<'a> {
            state: ::hardlight::tokio::sync::RwLockWriteGuard<'a, #state_ident>,
            starting_state: #state_ident,
            channel: std::sync::Arc<::hardlight::tokio::sync::mpsc::Sender<Vec<StateUpdate>>>,
        }

        impl<'a> Drop for StateGuard<'a> {
            /// Our custom drop implementation will send any changes to the runtime
            fn drop(&mut self) {
                // "diff" the two states to see what changed
                let mut changes = Vec::new();

                #(
                    if self.state.#field_names != self.starting_state.#field_names {
                        changes.push(::hardlight::StateUpdate {
                            index: #field_indices,
                            data: ::hardlight::rkyv::to_bytes::<_, 1024>(&self.state.#field_names).unwrap().to_vec(),
                        });
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
                    ::hardlight::tokio::task::yield_now().await;
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

        impl ::hardlight::ClientState for State {
            fn apply_changes(
                &mut self,
                changes: Vec<StateUpdate>,
            ) -> ::hardlight::HandlerResult<()> {
                for ::hardlight::StateUpdate { index, data } in changes {
                    match index {
                        #(
                            #field_indices => {
                                self.#field_names = ::hardlight::rkyv::from_bytes(&data)
                                    .map_err(|_| ::hardlight::RpcHandlerError::BadInputBytes)?;
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
    let no_server_handler = args.to_string().contains("no_server_handler");
    let trait_input = parse_macro_input!(input as ItemTrait);
    let vis = &trait_input.vis;

    let trait_ident = &trait_input.ident;

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
        #[derive(::hardlight::rkyv_derive::Archive, ::hardlight::rkyv_derive::Serialize, ::hardlight::rkyv_derive::Deserialize)]
        #[archive(crate = "::hardlight::rkyv", check_bytes)]
        #[repr(u8)]
        #vis enum RpcCall {
            #(#rpc_variants),*
        }
    };

    let server_handler = if !no_server_handler {
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
                            let result = ::hardlight::rkyv::to_bytes::<_, 1024>(&result).unwrap();
                            Ok(result.to_vec())
                        }
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        quote! {
            use ::hardlight::ServerHandler;

            #vis struct Handler {
                // the runtime will provide the state when it creates the handler
                #vis state: std::sync::Arc<StateController>,
                subscription_notification_tx: ::hardlight::tokio::sync::mpsc::UnboundedSender<::hardlight::ProxiedSubscriptionNotification>,
                event_tx: ::hardlight::tokio::sync::mpsc::UnboundedSender<::hardlight::Event>
            }

            #[::hardlight::async_trait::async_trait]
            impl ::hardlight::ServerHandler for Handler {
                fn new(
                    suc: ::hardlight::StateUpdateChannel, 
                    sntx: ::hardlight::tokio::sync::mpsc::UnboundedSender<::hardlight::ProxiedSubscriptionNotification>,
                    etx: ::hardlight::tokio::sync::mpsc::UnboundedSender<::hardlight::Event>
                ) -> Self {
                    Self {
                        state: std::sync::Arc::new(StateController::new(suc)),
                        subscription_notification_tx: sntx,
                        event_tx: etx
                    }
                }

                async fn handle_rpc_call(
                    &self,
                    input: &[u8],
                ) -> Result<Vec<u8>, ::hardlight::RpcHandlerError> {
                    let call: RpcCall = ::hardlight::rkyv::from_bytes(input)
                        .map_err(|_| ::hardlight::RpcHandlerError::BadInputBytes)?;

                    match call {
                        #(#server_methods),*
                    }
                }
                
                /// Subscribes the connection to the given topic
                fn subscribe(&self, topic: ::hardlight::Topic) {
                    self.subscription_notification_tx.send(::hardlight::ProxiedSubscriptionNotification::Subscribe(topic)).unwrap();
                }
                
                /// Unsubscribes the connection from the given topic
                fn unsubscribe(&self, topic: ::hardlight::Topic) {
                    self.subscription_notification_tx.send(::hardlight::ProxiedSubscriptionNotification::Unsubscribe(topic)).unwrap();
                }
                
                /// Emits an event to the event switch
                fn emit(&self, event: ::hardlight::Event) {
                    self.event_tx.send(event).unwrap();
                }
            }
        }
    } else {
        quote! {}
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
                            Ok(c) => ::hardlight::rkyv::from_bytes(&c)
                                .map_err(|_| ::hardlight::RpcHandlerError::BadOutputBytes),
                            Err(e) => Err(e),
                        }
                    }
                };

                client_methods.push(client_method);
            }
        }

        #[cfg(not(feature = "disable-self-signed"))]
        let client_new = quote! {
            fn new_self_signed(host: &str, compression: ::hardlight::Compression) -> Self {
                Self {
                    host: host.to_string(),
                    self_signed: true,
                    shutdown: None,
                    rpc_tx: None,
                    events_tx: None,
                    state: None,
                    compression,
                }
            }

            fn new(host: &str, compression: ::hardlight::Compression) -> Self {
                Self {
                    host: host.to_string(),
                    self_signed: false,
                    shutdown: None,
                    rpc_tx: None,
                    events_tx: None,
                    state: None,
                    compression,
                }
            }
        };

        #[cfg(feature = "disable-self-signed")]
        let client_new = quote! {
            fn new(host: &str, compression: ::hardlight::Compression) -> Self {
                Self {
                    host: host.to_string(),
                    self_signed: false,
                    shutdown: None,
                    rpc_tx: None,
                    events_tx: None,
                    state: None,
                    compression,
                }
            }
        };

        quote! {
            use ::hardlight::ApplicationClient;
            // CLIENT CODE
            #vis struct #client_name {
                host: String,
                self_signed: bool,
                shutdown: Option<::hardlight::tokio::sync::oneshot::Sender<()>>,
                rpc_tx: Option<::hardlight::tokio::sync::mpsc::Sender<(Vec<u8>, ::hardlight::RpcResponseSender)>>,
                events_tx: Option<std::sync::Arc<::hardlight::tokio::sync::broadcast::Sender<::hardlight::Event>>>,
                state: Option<std::sync::Arc<::hardlight::tokio::sync::RwLock<State>>>,
                compression: ::hardlight::Compression,
            }

            #[::hardlight::async_trait::async_trait]
            impl ::hardlight::ApplicationClient for #client_name {
                type State = State;
                #client_new

                /// Spawns a runtime client in the background to maintain the active
                /// connection
                async fn connect(&mut self) -> Result<(), ::hardlight::tungstenite::Error> {
                    let (shutdown, shutdown_rx) = ::hardlight::tokio::sync::oneshot::channel();
                    let (control_channels_tx, control_channels_rx) = ::hardlight::tokio::sync::oneshot::channel();
                    let (error_tx, error_rx) = ::hardlight::tokio::sync::oneshot::channel();

                    let self_signed = self.self_signed;
                    let host = self.host.clone();
                    let compression = self.compression;

                    tokio::spawn(async move {
                        let mut client: ::hardlight::Client<State> = if self_signed {
                            ::hardlight::Client::new_self_signed(&host, compression)
                        } else {
                            ::hardlight::Client::new(&host, compression)
                        };

                        if let Err(e) =
                            client.connect(shutdown_rx, control_channels_tx).await
                        {
                            error_tx.send(e).unwrap()
                        };
                    });

                    tokio::select! {
                        Ok((rpc_tx, state, events_tx)) = control_channels_rx => {
                            // at this point, the client will NOT return any errors, so we
                            // can safely ignore the error_rx channel
                            ::hardlight::tracing::debug!("Received control channels from client");
                            self.shutdown = Some(shutdown);
                            self.rpc_tx = Some(rpc_tx);
                            self.events_tx = Some(events_tx);
                            self.state = Some(state);
                            Ok(())
                        }
                        e = error_rx => {
                            Err(e.unwrap())
                        }
                    }
                }

                /// Returns a RwLockReadGuard to the state of the client.
                /// Do NOT hold this lock for long periods of time, as it will block processing.
                /// It is designed to be used for short periods of time, such as logging or a brief check/clone.
                async fn state(&self) -> ::hardlight::HandlerResult<::hardlight::tokio::sync::RwLockReadGuard<'_, Self::State>> {
                    match &self.state {
                        Some(state) => Ok(state.read().await),
                        None => Err(::hardlight::RpcHandlerError::ClientNotConnected),
                    }
                }
                
                async fn subscribe(&self) -> ::hardlight::HandlerResult<::hardlight::tokio::sync::broadcast::Receiver<::hardlight::Event>> {
                    match &self.events_tx {
                        Some(events_tx) => Ok(events_tx.subscribe()),
                        None => Err(::hardlight::RpcHandlerError::ClientNotConnected),
                    }
                }

                fn disconnect(&mut self) {
                    ::hardlight::tracing::debug!("Telling client to shutdown");
                    match self.shutdown.take() {
                        Some(shutdown) => {
                            let _ = shutdown.send(());
                        }
                        None => {}
                    }
                }
            }

            impl #client_name {
                async fn make_rpc_call(&self, call: RpcCall) -> ::hardlight::HandlerResult<Vec<u8>> {
                    if let Some(rpc_chan) = self.rpc_tx.clone() {
                        let (tx, rx) = ::hardlight::tokio::sync::oneshot::channel();
                        rpc_chan
                            .send((
                                ::hardlight::rkyv::to_bytes::<RpcCall, 1024>(&call)
                                    .map_err(|_| ::hardlight::RpcHandlerError::BadInputBytes)?
                                    .to_vec(),
                                tx,
                            ))
                            .await
                            .unwrap();
                        rx.await.unwrap()
                    } else {
                        Err(::hardlight::RpcHandlerError::ClientNotConnected)
                    }
                }
            }

            impl Drop for #client_name {
                fn drop(&mut self) {
                    ::hardlight::tracing::debug!("Application client got dropped. Disconnecting.");
                    self.disconnect();
                }
            }

            #[::hardlight::async_trait::async_trait]
            impl #trait_ident for #client_name {
                #(#client_methods)*
            }
        }
    };

    let expanded = quote! {
        #[::hardlight::async_trait::async_trait]
        #trait_input
        #shared_code
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
        #[::hardlight::async_trait::async_trait]
        #input
    };

    proc_macro::TokenStream::from(expanded)
}

#[proc_macro_attribute]
/// Takes any ast as an input and annotates it with useful attributes for data
/// serialization and deserialization. This consists of `Archive + Serialize +
/// Deserialize`, for the root type and `CheckBytes` for the archived version.
pub fn codable(
    _attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = parse_macro_input!(item as syn::Item);

    let expanded = quote! {
        #[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
        #[archive(crate = "::hardlight::rkyv", check_bytes)]
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
