use std::{
    io,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use flate2::Compression;
use futures_util::{SinkExt, StreamExt};
use hashbrown::HashMap;
use rand::RngCore;
use rkyv::{Archive, Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::mpsc,
    task::yield_now,
};

#[cfg(not(feature = "disable-self-signed"))]
use rcgen::generate_simple_self_signed;
#[cfg(not(feature = "disable-self-signed"))]
use tokio_rustls::rustls::{Certificate, PrivateKey};

use tokio_rustls::{
    rustls::ServerConfig as TLSServerConfig, server::TlsStream, TlsAcceptor,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        http::{HeaderValue, StatusCode},
        Message,
    },
    WebSocketStream,
};
use tracing::{debug, info, info_span, span, trace, warn, Instrument, Level};
use version::{version, Version};

use crate::{
    deflate, inflate,
    wire::{ClientMessage, RpcHandlerError, ServerMessage},
};

/// A tokio MPSC channel that is used to send state updates to the runtime.
/// The runtime will then send these updates to the client.
pub type StateUpdateChannel = mpsc::Sender<Vec<StateUpdate>>;

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct StateUpdate {
    pub index: usize,
    pub data: Vec<u8>,
}

pub type HandlerResult<T> = Result<T, RpcHandlerError>;

/// A [ServerHandler] will be created for each connection to the server.
/// These are user-defined structs that respond to RPC calls
#[async_trait]
pub trait ServerHandler {
    /// Create a new handler.
    fn new(
        suc: StateUpdateChannel,
        // subscription manager (handler -> connection handler)
        subscriptions: HandlerSubscriptionManager,
        // event emitter (handler -> event switch)
        events: EventEmitter,
    ) -> Self
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
    T: Fn(
        StateUpdateChannel,
        HandlerSubscriptionManager,
        EventEmitter,
    ) -> Box<dyn ServerHandler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    /// The server's configuration.
    pub config: ServerConfig,
    /// A closure that creates a new handler for each connection.
    /// The closure is passed a [StateUpdateChannel] that the handler can use
    /// to send state updates to the runtime.
    pub factory: T,
    hl_version_string: HeaderValue,
    event_rx: Option<mpsc::UnboundedReceiver<Event>>,
    event_tx: mpsc::UnboundedSender<Event>,
}

impl<T> Server<T>
where
    T: Fn(
        StateUpdateChannel,
        HandlerSubscriptionManager,
        EventEmitter,
    ) -> Box<dyn ServerHandler + Send + Sync>,
    T: Send + Sync + 'static + Copy,
{
    pub fn new(config: ServerConfig, factory: T) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            hl_version_string: format!("hl/{}", config.version_major)
                .parse()
                .unwrap(),
            config,
            factory,
            event_rx: Some(event_rx),
            event_tx,
        }
    }

    pub fn get_event_emitter(&self) -> EventEmitter {
        EventEmitter(self.event_tx.clone())
    }

    pub async fn run(mut self) -> io::Result<()> {
        info!("Booting HL server v{}...", HL_VERSION);
        let acceptor = TlsAcceptor::from(Arc::new(self.config.tls.clone()));
        let listener = TcpListener::bind(&self.config.address).await?;
        info!("Listening on {} with TLS", self.config.address);

        // handlers can subscribe to event topics here
        let (subscription_notification_tx, subscription_notification_rx) =
            mpsc::unbounded_channel::<SubscriptionNotification>();

        let event_switch = EventSwitch::new(
            self.event_rx.take().unwrap(),
            subscription_notification_rx,
        );
        tokio::spawn(event_switch.run().instrument(info_span!("event_switch")));

        loop {
            if let Ok((stream, peer_addr)) = listener.accept().await {
                self.handle_connection(
                    stream,
                    acceptor.clone(),
                    peer_addr,
                    subscription_notification_tx.clone(),
                );
            }
        }
    }

    fn handle_connection(
        &self,
        stream: TcpStream,
        acceptor: TlsAcceptor,
        peer_addr: SocketAddr,
        subscription_notification_tx: mpsc::UnboundedSender<
            SubscriptionNotification,
        >,
    ) {
        let (state_change_tx, mut state_change_rx) = mpsc::channel(10);
        let (
            proxy_subscription_notification_tx,
            mut proxy_subscription_notification_rx,
        ) = mpsc::unbounded_channel::<ProxiedSubscriptionNotification>();
        let handler = (self.factory)(
            state_change_tx,
            HandlerSubscriptionManager(proxy_subscription_notification_tx),
            self.get_event_emitter(),
        );
        let version: HeaderValue = self.hl_version_string.clone();
        tokio::spawn(async move {
            let connection_span =
                span!(Level::DEBUG, "connection", peer_addr = %peer_addr);

            async move {
                let stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(_) => return,
                };

                trace!("Successfully terminated TLS handshake");

                let mut compression = Compression::default();

                let callback = |req: &Request, mut response: Response| {
                    match req.headers().get("Sec-WebSocket-Protocol") {
                        Some(req_version) if req_version == &version => {
                            response.headers_mut().append("Sec-WebSocket-Protocol", version);
                            trace!("Received valid handshake, upgrading connection to HardLight ({})", req_version.to_str().unwrap());
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
                            trace!("Accepted client specified compression level {}", c);
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

                trace!("Connection fully established");

                // keep track of active RPC calls
                let mut in_flight = [false; u8::MAX as usize + 1];

                let (rpc_tx, mut rpc_rx) = mpsc::channel(u8::MAX as usize + 1);

                let mut subscriptions: HashMap<Topic, SubscriptionID> = HashMap::new();
                let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Event>();

                let handler = Arc::new(handler);

                let compression = Arc::new(compression);

                trace!("Starting RPC handler loop");
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

                                        trace!("Received call from client. Spawning handler task...");

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

                                        trace!("Handler task spawned.");
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

                            trace!("RPC call finished. Serializing and sending response...");

                            Server::<T>::send_msg(msg, &mut ws_stream, compression.clone(), "RPC response").await;
                        }
                        // await state updates from the application
                        Some(state_changes) = state_change_rx.recv() => {
                            let span = span!(Level::DEBUG, "state_change");
                            let _enter = span.enter();

                            debug!("Received {} state update(s) from application. Serializing and sending...", state_changes.len());

                            Server::<T>::send_msg(ServerMessage::StateChange(state_changes), &mut ws_stream, compression.clone(), "state update").await;
                        }
                        // await subscription notifications from the handler
                        Some(notification) = proxy_subscription_notification_rx.recv() => {
                            let span = span!(Level::DEBUG, "subscription_notification");
                            let _enter = span.enter();

                            match notification {
                                ProxiedSubscriptionNotification::Subscribe(topic) => {
                                    if subscriptions.contains_key(&topic) {
                                        continue;
                                    }
                                    let id = {
                                        let mut id = [0; 16];
                                        rand::thread_rng().fill_bytes(&mut id);
                                        id
                                    };
                                    let tx = events_tx.clone();
                                    subscriptions.insert(topic.clone(), id);
                                    if let Err(e) = subscription_notification_tx.send(
                                        SubscriptionNotification::Subscribe { topic, id, tx }
                                    ) {
                                        warn!("Error sending subscription notification to handler: {}", e);
                                    }
                                },
                                ProxiedSubscriptionNotification::Unsubscribe(topic) => {
                                    if let Some(id) = subscriptions.remove(&topic) {
                                        if let Err(e) = subscription_notification_tx.send(
                                            SubscriptionNotification::Unsubscribe { topic, id }
                                        ) {
                                            warn!("Error sending subscription notification to handler: {}", e);
                                        }
                                    }
                                }
                            };
                        }
                        // await events from the event switch
                        Some(event) = events_rx.recv() => {
                            let span = span!(Level::DEBUG, "event");
                            let _enter = span.enter();

                            debug!("Received event from application after {:?}. Serializing and sending...", event.created_at.elapsed());

                            let Event { topic, payload, .. } = event;
                            Server::<T>::send_msg(
                                ServerMessage::NewEvent { topic, payload },
                                &mut ws_stream, compression.clone(), "Event"
                            ).await;
                        }
                    }
                }
            }
            .instrument(connection_span)
            .await
        });
    }

    async fn send_msg(
        msg: ServerMessage,
        ws_stream: &mut WebSocketStream<TlsStream<TcpStream>>,
        compression: Arc<Compression>,
        msg_type: &'static str,
    ) {
        let msg = match rkyv::to_bytes::<_, 1024>(&msg) {
            Ok(encoded) => encoded,
            Err(e) => {
                warn!("Error serializing {msg_type}: {}", e);
                return;
            }
        };

        let msg = match deflate(msg.as_ref(), *compression) {
            Some(msg) => msg,
            None => {
                warn!("Error compressing {msg_type}");
                return;
            }
        };

        match ws_stream.send(Message::Binary(msg.into())).await {
            Ok(_) => trace!("{msg_type} sent"),
            Err(e) => {
                warn!("Error sending {msg_type} to client: {}", e);
                return;
            }
        };
    }
}

pub struct EventEmitter(mpsc::UnboundedSender<Event>);

impl EventEmitter {
    pub async fn emit(&self, topic: &Topic, payload: impl Into<Vec<u8>>) {
        let event = Event::new(topic, payload);
        self.0.send(event).unwrap();
        yield_now().await;
    }
}

pub struct HandlerSubscriptionManager(
    mpsc::UnboundedSender<ProxiedSubscriptionNotification>,
);

impl HandlerSubscriptionManager {
    pub fn add(&self, topic: &Topic) {
        self.0
            .send(ProxiedSubscriptionNotification::Subscribe(topic.clone()))
            .unwrap();
    }

    pub fn remove(&self, topic: &Topic) {
        self.0
            .send(ProxiedSubscriptionNotification::Unsubscribe(topic.clone()))
            .unwrap();
    }
}

#[derive(
    Eq, PartialEq, Hash, Clone, Default, Archive, Serialize, Deserialize,
)]
#[archive(check_bytes)]
pub struct Topic {
    inner: Vec<u8>,
    is_string: bool,
}

impl Into<Vec<u8>> for Topic {
    fn into(self) -> Vec<u8> {
        self.inner
    }
}

impl Into<Topic> for Vec<u8> {
    fn into(self) -> Topic {
        Topic {
            inner: self,
            is_string: false,
        }
    }
}

impl Into<Topic> for &str {
    fn into(self) -> Topic {
        Topic {
            inner: self.as_bytes().to_vec(),
            is_string: true,
        }
    }
}

impl Into<Topic> for String {
    fn into(self) -> Topic {
        Topic {
            inner: self.into_bytes(),
            is_string: true,
        }
    }
}

impl Topic {
    pub fn as_string(&self) -> String {
        String::from_utf8(self.inner.clone()).unwrap_or("".to_string())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }
}

impl std::fmt::Debug for Topic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_string {
            write!(f, "{}", self.as_string())
        } else {
            write!(f, "{:?}", self.inner)
        }
    }
}

pub type SubscriptionID = [u8; 16];

pub enum SubscriptionNotification {
    Subscribe {
        topic: Topic,
        id: SubscriptionID,
        tx: mpsc::UnboundedSender<Event>,
    },
    Unsubscribe {
        topic: Topic,
        id: SubscriptionID,
    },
}

pub enum ProxiedSubscriptionNotification {
    Subscribe(Topic),
    Unsubscribe(Topic),
}

#[derive(Clone, Debug)]
pub struct Event {
    pub topic: Topic,
    pub payload: Vec<u8>,
    pub created_at: Instant,
}

impl Event {
    pub fn new(topic: &Topic, payload: impl Into<Vec<u8>>) -> Self {
        let payload = payload.into();
        Self {
            topic: topic.clone(),
            payload,
            created_at: Instant::now(),
        }
    }
}

/// The event switch is responsible for routing events from the application to
/// the subscribed connections.
pub struct EventSwitch {
    /// A channel to receive new events from the application
    new_event_rx: mpsc::UnboundedReceiver<Event>,
    /// A map of topic -> connections
    subscriptions:
        HashMap<Topic, HashMap<SubscriptionID, mpsc::UnboundedSender<Event>>>,
    /// Handlers notify the event switch about new subscriptions
    subscription_notification_rx:
        mpsc::UnboundedReceiver<SubscriptionNotification>,
}

impl EventSwitch {
    fn new(
        new_event_rx: mpsc::UnboundedReceiver<Event>,
        subscription_notification_rx: mpsc::UnboundedReceiver<
            SubscriptionNotification,
        >,
    ) -> Self {
        Self {
            new_event_rx,
            subscriptions: HashMap::new(),
            subscription_notification_rx,
        }
    }

    async fn run(mut self) {
        loop {
            select! {
                Some(notification) = self.subscription_notification_rx.recv() => {
                        trace!("Received subscription notification from handler");
                        match notification {
                            SubscriptionNotification::Subscribe { topic, id, tx } => {
                                trace!("Subscription notification is a subscribe");
                                if let Some(subscribers) = self.subscriptions.get_mut(&topic) {
                                    trace!("Topic already exists, adding subscriber");
                                    subscribers.insert(id, tx);
                                } else {
                                    trace!("Topic does not exist, creating topic and adding subscriber");
                                    let mut subscribers = HashMap::new();
                                    subscribers.insert(id, tx);
                                    self.subscriptions.insert(topic, subscribers);
                                }
                            }
                            SubscriptionNotification::Unsubscribe { topic, id } => {
                                trace!("Subscription notification is an unsubscribe");
                                if let Some(subscribers) = self.subscriptions.get_mut(&topic) {
                                    trace!("Topic exists, removing subscriber");
                                    subscribers.remove(&id);
                                } else {
                                    trace!("Topic does not exist, ignoring unsubscribe");
                                }
                            }
                        }
                    }
                    Some(event) = self.new_event_rx.recv() => {
                        trace!("Received event from application after {:?}", event.created_at.elapsed());
                        if let Some(subscribers) = self.subscriptions.get(&event.topic) {
                            trace!("Topic exists, sending event to {} subscribers", subscribers.len());
                            for (_, connection) in subscribers {
                                connection.send(event.clone()).unwrap();
                            }
                        } else {
                            trace!("Topic has no subscribers, ignoring event");
                        }
                        trace!("Finished routing event in {:?}", event.created_at.elapsed());
                        yield_now().await;
                    }
            }
        }
    }
}
