use std::{io, net::SocketAddr, str::FromStr, sync::Arc};

use async_trait::async_trait;
use flate2::Compression;
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::mpsc,
};

#[cfg(not(feature = "disable-self-signed"))]
use rcgen::generate_simple_self_signed;
#[cfg(not(feature = "disable-self-signed"))]
use tokio_rustls::rustls::{Certificate, PrivateKey};

use tokio_rustls::{rustls::ServerConfig as TLSServerConfig, TlsAcceptor};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        http::{HeaderValue, StatusCode},
        Message,
    },
};
use tracing::{debug, info, span, warn, Instrument, Level};
use version::{version, Version};

use crate::{
    deflate, inflate,
    wire::{ClientMessage, RpcHandlerError, ServerMessage},
};

/// A tokio MPSC channel that is used to send state updates to the runtime.
/// The runtime will then send these updates to the client.
pub type StateUpdateChannel = mpsc::Sender<Vec<(usize, Vec<u8>)>>;

pub type HandlerResult<T> = Result<T, RpcHandlerError>;

/// A [ServerHandler] will be created for each connection to the server.
/// These are user-defined structs that respond to RPC calls
#[async_trait]
pub trait ServerHandler {
    /// Create a new handler using the given state update channel.
    fn new(state_update_channel: StateUpdateChannel) -> Self
    where
        Self: Sized;
    /// Handle an RPC call (method + arguments) from the client.
    async fn handle_rpc_call(&self, input: &[u8]) -> HandlerResult<Vec<u8>>;
}

#[async_trait]
pub trait ApplicationServer {
    fn new(config: ServerConfig) -> Self;
    async fn start(&mut self) -> Result<(), std::io::Error>;
    fn stop(&mut self);
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub address: String,
    pub version_major: u32,
    pub tls: TLSServerConfig,
}

impl ServerConfig {
    #[cfg(not(feature = "disable-self-signed"))]
    pub fn new_self_signed(host: &str) -> Self {
        Self::new(host, {
            let cert = generate_simple_self_signed(vec![host.into()]).unwrap();
            TLSServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(
                    vec![Certificate(cert.serialize_der().unwrap())],
                    PrivateKey(cert.serialize_private_key_der()),
                )
                .expect("failed to create TLS config")
        })
    }

    pub fn new(host: &str, tls: TLSServerConfig) -> Self {
        Self {
            address: host.into(),
            version_major: Version::from_str(HL_VERSION).unwrap().major,
            tls,
        }
    }
}

pub const HL_VERSION: &str = version!();

/// The HardLight server, using tokio & tungstenite.
pub struct Server<T>
where
    T: Fn(StateUpdateChannel) -> Box<dyn ServerHandler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    /// The server's configuration.
    pub config: ServerConfig,
    /// A closure that creates a new handler for each connection.
    /// The closure is passed a [StateUpdateChannel] that the handler can use
    /// to send state updates to the runtime.
    pub factory: T,
    pub hl_version_string: HeaderValue,
}

impl<T> Server<T>
where
    T: Fn(StateUpdateChannel) -> Box<dyn ServerHandler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    pub fn new(config: ServerConfig, factory: T) -> Self {
        Self {
            hl_version_string: format!("hl/{}", config.version_major)
                .parse()
                .unwrap(),
            config,
            factory,
        }
    }

    pub async fn run(&self) -> io::Result<()> {
        info!("Booting HL server v{}...", HL_VERSION);
        let acceptor = TlsAcceptor::from(Arc::new(self.config.tls.clone()));
        let listener = TcpListener::bind(&self.config.address).await?;
        info!("Listening on {} with TLS", self.config.address);

        loop {
            if let Ok((stream, peer_addr)) = listener.accept().await {
                self.handle_connection(stream, acceptor.clone(), peer_addr);
            }
        }
    }

    fn handle_connection(
        &self,
        stream: TcpStream,
        acceptor: TlsAcceptor,
        peer_addr: SocketAddr,
    ) {
        let (state_change_tx, mut state_change_rx) = mpsc::channel(10);
        let handler = (self.factory)(state_change_tx);
        let version: HeaderValue = self.hl_version_string.clone();
        tokio::spawn(async move {
            let connection_span =
                span!(Level::DEBUG, "connection", peer_addr = %peer_addr);

            async move {
                let stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(_) => return,
                };

                debug!("Successfully terminated TLS handshake");

                let mut compression = Compression::default();

                let callback = |req: &Request, mut response: Response| {
                    match req.headers().get("Sec-WebSocket-Protocol") {
                        Some(req_version) if req_version == &version => {
                            response.headers_mut().append("Sec-WebSocket-Protocol", version);
                            debug!("Received valid handshake, upgrading connection to HardLight ({})", req_version.to_str().unwrap());
                        }
                        Some(req_version) => {
                            *response.status_mut() = StatusCode::BAD_REQUEST;
                            warn!("Invalid request from {}, version mismatch (client gave {:?}, server wanted {:?})", peer_addr, req_version, Some(version));
                        }
                        None => {
                            *response.status_mut() = StatusCode::BAD_REQUEST;
                            warn!("Invalid request from {}, no version specified", peer_addr);
                        }
                    }

                    compression = req
                        .headers()
                        .get("X-HL-Compress")
                        .and_then(|c| c.to_str().ok())
                        .and_then(|c| c.parse::<u8>().ok())
                        .filter(|&c| c <= 9)
                        .map(|c| {
                            debug!("Accepted client specified compression level {}", c);
                            Compression::new(c as u32)
                        })
                        .unwrap_or(Compression::default());

                    response.headers_mut().append("X-HL-Compress", compression.level().into());

                    Ok(response)
                };

                let mut ws_stream = match accept_hdr_async(stream, callback).await {
                    Ok(ws_stream) => ws_stream,
                    Err(e) => {
                        warn!("Error accepting connection from {}: {}", peer_addr, e);
                        return;
                    }
                };

                debug!("Connection fully established");

                // keep track of active RPC calls
                let mut in_flight = [false; u8::MAX as usize + 1];

                let (rpc_tx, mut rpc_rx) = mpsc::channel(u8::MAX as usize + 1);

                let handler = Arc::new(handler);

                let compression = Arc::new(compression);

                debug!("Starting RPC handler loop");
                loop {
                    select! {
                        // await new messages from the client
                        Some(msg) = ws_stream.next() => {
                            let msg = match msg {
                                Ok(msg) => msg,
                                Err(e) => {
                                    warn!("Error receiving message from client: {}", e);
                                    continue;
                                }
                            };
                            if msg.is_binary() {
                                let binary = match inflate(&msg.into_data()) {
                                    Some(binary) => binary,
                                    None => {
                                        warn!("Error decompressing message from client");
                                        continue;
                                    },
                                };

                                let msg = match rkyv::from_bytes::<ClientMessage>(&binary) {
                                    Ok(msg) => msg,
                                    Err(e) => {
                                        warn!("Error deserializing message from client: {}", e);
                                        continue;
                                    }
                                };

                                match msg {
                                    ClientMessage::RPCRequest { id, internal } => {
                                        let span = span!(Level::DEBUG, "rpc", id = id);
                                        let _enter = span.enter();

                                        if in_flight[id as usize] {
                                            warn!("RPC call already in flight. Ignoring.");
                                            continue;
                                        }

                                        debug!("Received call from client. Spawning handler task...");

                                        let tx = rpc_tx.clone();
                                        let handler = handler.clone();
                                        in_flight[id as usize] = true;
                                        tokio::spawn(async move {
                                            tx.send(
                                                ServerMessage::RPCResponse {
                                                    id,
                                                    output: handler.handle_rpc_call(&internal).await,
                                                }
                                            ).await
                                        });

                                        debug!("Handler task spawned.");
                                    }
                                }
                            }
                        }
                        // await responses from RPC calls
                        Some(msg) = rpc_rx.recv() => {
                            let id = match msg {
                                ServerMessage::RPCResponse { id, .. } => id,
                                _ => unreachable!(),
                            };

                            let span = span!(Level::DEBUG, "rpc", id = id);
                            let _enter = span.enter();

                            in_flight[id as usize] = false;

                            debug!("RPC call finished. Serializing and sending response...");

                            let encoded = match rkyv::to_bytes::<ServerMessage, 1024>(&msg) {
                                Ok(encoded) => encoded,
                                Err(e) => {
                                    warn!("Error serializing response: {}", e);
                                    continue;
                                }
                            };

                            let binary = match deflate(encoded.as_ref(), *compression) {
                                Some(compressed) => compressed,
                                None => {
                                    warn!("Error compressing response");
                                    continue;
                                },
                            };

                            match ws_stream.send(Message::Binary(binary)).await {
                                Ok(_) => debug!("Response sent."),
                                Err(e) => {
                                    warn!("Error sending response to client: {}", e);
                                    continue
                                }
                            };
                        }
                        // await state updates from the application
                        Some(state_changes) = state_change_rx.recv() => {
                            let span = span!(Level::DEBUG, "state_change");
                            let _enter = span.enter();

                            debug!("Received {} state update(s) from application. Serializing and sending...", state_changes.len());

                            let encoded = match rkyv::to_bytes::<ServerMessage, 1024>(&ServerMessage::StateChange(state_changes)) {
                                Ok(encoded) => encoded,
                                Err(e) => {
                                    warn!("Error serializing state update: {}", e);
                                    continue;
                                }
                            };

                            let binary = match deflate(encoded.as_ref(), *compression) {
                                Some(compressed) => compressed,
                                None => {
                                    warn!("Error compressing state update");
                                    continue;
                                },
                            };

                            match ws_stream.send(Message::Binary(binary)).await {
                                Ok(_) => debug!("State update sent."),
                                Err(e) => {
                                    warn!("Error sending state update to client: {}", e);
                                    continue
                                }
                            };
                        }
                    }
                }
            }
            .instrument(connection_span)
            .await
        });
    }
}
