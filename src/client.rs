use std::{fmt::Debug, str::FromStr, sync::Arc, time::SystemTime};

use async_trait::async_trait;
use flate2::Compression;
use futures_util::{SinkExt, StreamExt};
use rustls_native_certs::load_native_certs;
use tokio::{
    select,
    sync::{mpsc, oneshot, RwLock, RwLockReadGuard},
};
use tokio_rustls::rustls::{
    client::{ServerCertVerified, ServerCertVerifier},
    Certificate, ClientConfig as TLSClientConfig, RootCertStore, ServerName,
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        self,
        error::ProtocolError,
        handshake::client::generate_key,
        http::{HeaderValue, Request},
        Error, Message,
    },
    Connector,
};
use tracing::{debug, error, span, warn, Instrument, Level, trace};
use version::Version;

use crate::{
    deflate, inflate,
    server::{HandlerResult, HL_VERSION},
    wire::{ClientMessage, RpcHandlerError, ServerMessage},
};

use array_init::array_init;

pub struct ClientConfig {
    tls: TLSClientConfig,
    host: String,
    compression: Compression,
}

pub trait ClientState {
    fn apply_changes(
        &mut self,
        changes: Vec<(usize, Vec<u8>)>,
    ) -> HandlerResult<()>;
}

#[async_trait]
pub trait ApplicationClient {
    type State;
    #[cfg(not(feature = "disable-self-signed"))]
    fn new_self_signed(host: &str, compression: Compression) -> Self;
    fn new(host: &str, compression: Compression) -> Self;
    async fn state(&self) -> HandlerResult<RwLockReadGuard<'_, Self::State>>;
    async fn connect(&mut self) -> Result<(), tungstenite::Error>;
    fn disconnect(&mut self);
}

/// The [RpcResponseSender] is used to send the response of an RPC call back to
/// the application. When an application sends an RPC call to the server, it
/// will provide a serialized RPC call (method + args) and one of these senders.
/// The application will await the response on the receiver side of the channel.
pub type RpcResponseSender = oneshot::Sender<Result<Vec<u8>, RpcHandlerError>>;

pub type ControlChannels<T> = (
    // rpc sender - app can send multiple RPC requests consisting of:
    // (serialized RPC call (includes method + args), RPC's response sender)
    mpsc::Sender<(Vec<u8>, RpcResponseSender)>,
    Arc<RwLock<T>>,
);

pub struct Client<T>
where
    T: ClientState + Default + Debug,
{
    config: ClientConfig,
    state: Arc<RwLock<T>>,
    hl_version_string: HeaderValue,
}

impl<T> Client<T>
where
    T: ClientState + Default + Debug,
{
    /// Creates a new client that doesn't verify the server's certificate.
    pub fn new_self_signed(host: &str, compression: Compression) -> Self {
        let tls = TLSClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(
                NoCertificateVerification {},
            ))
            .with_no_client_auth();
        let config = ClientConfig {
            tls,
            host: host.to_string(),
            compression,
        };
        Self::new_with_config(config)
    }

    /// Create a new client using the system's root certificates.
    pub fn new(host: &str, compression: Compression) -> Self {
        let mut root_store = RootCertStore::empty();
        for cert in load_native_certs().unwrap() {
            root_store.add(&Certificate(cert.0)).unwrap();
        }
        let tls = TLSClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let config = ClientConfig {
            tls,
            host: host.to_string(),
            compression,
        };
        Self::new_with_config(config)
    }

    /// Create a new client using the given configuration.
    fn new_with_config(config: ClientConfig) -> Self {
        let version = Version::from_str(HL_VERSION).unwrap();
        Self {
            config,
            state: Arc::new(T::default().into()),
            hl_version_string: format!("hl/{}", version.major).parse().unwrap(),
        }
    }

    pub async fn connect<'a>(
        &'a mut self,
        // Allows the application's wrapping client to shut down the connection
        mut shutdown: oneshot::Receiver<()>,
        // Sends control channels to the application so it can send RPC calls,
        // events, and other things to the server.
        control_channels_tx: oneshot::Sender<ControlChannels<T>>,
    ) -> Result<(), Error> {
        let connection_span =
            span!(Level::DEBUG, "connection", host = self.config.host);

        async move {
            let connector = Connector::Rustls(Arc::new(self.config.tls.clone()));

            let req = Request::builder().method("GET").header("Host", self.config.host.clone()).header("Connection", "Upgrade").header("Upgrade", "websocket").header("Sec-WebSocket-Version", "13").header("Sec-WebSocket-Key", generate_key()).header("Sec-WebSocket-Protocol", self.hl_version_string.clone()).header("X-HL-Compress", self.config.compression.level()).uri(format!("wss://{}/", self.config.host)).body(()).unwrap();

            debug!("Connecting to server...");
            let (mut stream, res) = connect_async_tls_with_config(req, None, Some(connector)).await?;

            let headers = res.headers();

            match headers.get("Sec-WebSocket-Protocol") {
                Some(protocol) if protocol == &self.hl_version_string => {
                    let compression = headers.get("X-HL-Compress")
                        .and_then(|h| h.to_str().ok())
                        .and_then(|n| n.parse::<u32>().ok())
                        .filter(|&c| c <= 9)
                        .map(|c| Compression::new(c));

                    trace!(
                        "HardLight connection established [{}, compression {}]",
                        protocol.to_str().unwrap(),
                        match compression {
                            Some(c) => format!("explicit {}", c.level()),
                            None => format!("implicit {}", self.config.compression.level()),
                        }
                    );
                }
                Some(protocol) => {
                    error!("Received bad version from server. Wanted {:?}, got {:?}", self.hl_version_string, protocol);
                    return Err(Error::Protocol(ProtocolError::HandshakeIncomplete));
                }
                None => {
                    error!("Received bad version from server. Wanted {:?}, got None", self.hl_version_string);
                    return Err(Error::Protocol(ProtocolError::HandshakeIncomplete));
                }
            }

            trace!("Sending control channels to application...");
            let (rpc_request_tx, mut rpc_request_rx) = mpsc::channel(10);
            if let Err(_) = control_channels_tx.send((rpc_request_tx, self.state.clone())) {
                panic!("Failed to send control channels to application");
            }
            trace!("Control channels sent.");

            // keep track of active RPC calls
            let mut active_rpc_calls: [Option<RpcResponseSender>; 256] = array_init(|_| None);

            debug!("Starting RPC handler loop");
            loop {
                select! {
                    // await RPC requests from the application
                    Some((internal, completion_tx)) = rpc_request_rx.recv() => {
                        trace!("Received RPC request from application");
                        // find a free rpc id
                        if let Some(id) = active_rpc_calls.iter().position(|x| x.is_none()) {
                            let span = span!(Level::DEBUG, "rpc", id = id);
                            let _enter = span.enter();
                            trace!("Found free RPC id");

                            let msg = ClientMessage::RPCRequest {
                                id: id as u8,
                                internal
                            };

                            let mut binary = match rkyv::to_bytes::<ClientMessage, 1024>(&msg) {
                                Ok(bytes) => bytes,
                                Err(e) => {
                                    warn!("Failed to serialize RPC call. Ignoring. Error: {e}");
                                    // we don't care if the receiver has dropped
                                    let _ = completion_tx.send(Err(RpcHandlerError::BadInputBytes));
                                    continue
                                }
                            }.to_vec();

                            binary = match deflate(&binary, self.config.compression) {
                                Some(bytes) => bytes,
                                None => {
                                    warn!("Failed to compress RPC call. Ignoring.");
                                    // we don't care if the receiver has dropped
                                    let _ = completion_tx.send(Err(RpcHandlerError::BadInputBytes));
                                    continue
                                }
                            };

                            trace!("Sending RPC call to server");

                            match stream.send(Message::Binary(binary)).await {
                                Ok(_) => (),
                                Err(e) => {
                                    warn!("Failed to send RPC call. Ignoring. Error: {e}");
                                    // we don't care if the receiver has dropped
                                    let _ = completion_tx.send(Err(RpcHandlerError::ClientNotConnected));
                                    continue
                                }
                            }

                            trace!("RPC call sent to server");

                            active_rpc_calls[id] = Some(completion_tx);
                        } else {
                            warn!("No free RPC id available. Responding with an error.");
                            let _ = completion_tx.send(Err(RpcHandlerError::TooManyCallsInFlight));
                        }
                    }
                    // await RPC responses from the server
                    Some(msg) = stream.next() => {
                        if let Ok(msg) = msg {
                            if let Message::Binary(mut bytes) = msg {
                                bytes = match inflate(&bytes) {
                                    Some(bytes) => bytes,
                                    None => {
                                        warn!("Failed to decompress RPC response. Ignoring.");
                                        continue
                                    }
                                };
                                let msg: ServerMessage = match rkyv::from_bytes(&bytes) {
                                    Ok(msg) => msg,
                                    Err(e) => {
                                        warn!("Received invalid RPC response. Ignoring. Error: {e}");
                                        continue;
                                    }
                                };
                                match msg {
                                    ServerMessage::RPCResponse { id, output } => {
                                        let span = span!(Level::DEBUG, "rpc", id = %id);
                                        let _enter = span.enter();
                                        trace!("Received RPC response from server");
                                        if let Some(completion_tx) = active_rpc_calls[id as usize].take() {
                                            trace!("Attempting send to application");
                                            let _ = completion_tx.send(output);
                                        } else {
                                            warn!("Received RPC response for unknown RPC call. Ignoring.");
                                        }
                                    }
                                    ServerMessage::StateChange(changes) => {
                                        let span = span!(Level::DEBUG, "state_change");
                                        let _enter = span.enter();
                                        trace!("Received {} state change(s) from server", changes.len());
                                        if let Err(e) = self.state.write().await.apply_changes(changes) {
                                            warn!("Failed to apply state changes. Error: {:?}", e);
                                        };
                                    }
                                    ServerMessage::NewEvent { .. } => {
                                        warn!("NewEvent has not been implemented yet. Ignoring.")
                                    }
                                }
                            }
                        }
                    }
                    // await shutdown signal
                    _ = &mut shutdown => {
                        debug!("Application sent shutdown, breaking handler loop.");
                        break;
                    }
                }
            }

            if let Err(e) = stream.close(None).await {
                warn!("Failed to nicely close stream: {e}");
            } else {
                debug!("Closed stream");
            }

            debug!("RPC handler loop exited.");
            Ok(())
        }
        .instrument(connection_span)
        .await
    }
}

struct NoCertificateVerification {}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}
