use std::{io, net::SocketAddr, str::FromStr, sync::Arc};

use async_trait::async_trait;
use flate2::Compression;
use futures_util::{SinkExt, StreamExt};
use hashbrown::HashMap;
use rand::RngCore;
use rkyv::{Archive, Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{mpsc, oneshot},
    task::{yield_now, JoinHandle},
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
use tracing::{debug, info, info_span, span, trace, warn, Instrument, Level, debug_span};
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
/// An update sent from the connection handler to the client to keep connection state synchronized.
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

// /// A RateLimit the server should enforce
// pub struct RateLimit {
//     /// The type of rate limit
//     limit_type: RateLimitType,
//     /// Requests per second to allow
//     per_sec: f64,
//     /// 
//     block_ip_after: u64
// }

// pub enum RateLimitType {
//     /// The rate limit applies per IP address
//     IpAddr,
//     /// The rate limit applies per connection
//     Connection
// }

// pub enum IpBlock {
//     /// Tem
//     Temporary {
//         after: u64,
//     },
//     InMemoryPermanent
// }

// pub enum BlockCondition {
//     /// Server will not block IPs
//     None,
// }

// impl RateLimit {
//     fn ip_per_sec(per_sec: f64) {
//         RateLimit {
//             limit_type: RateLimitType::IpAddr,
//             per_sec
//         }
//     }
    
//     fn connection_per_sec(per_sec: f64) {
//         RateLimit {
//             limit_type: RateLimitType::Connection,
//             per_sec
//         }
//     }
// }

pub const HL_VERSION: &str = version!();

/// The HardLight server, using tokio & tungstenite.
pub struct Server<Factory>
where
    Factory: Fn(
        StateUpdateChannel,
        HandlerSubscriptionManager,
        EventEmitter,
    ) -> Box<dyn ServerHandler + Send + Sync>,
    Factory: Send + Sync + 'static + Copy,
{
    /// The server's configuration.
    pub config: ServerConfig,
    /// A handler factory that is used to create a new handler for each connection.
    pub factory: Arc<Factory>,
    /// HardLight's version string
    hl_version_string: HeaderValue,
    /// Event switch's event receiver
    event_rx: Option<mpsc::UnboundedReceiver<Event>>,
    /// Event transmitter for application and handlers
    event_tx: mpsc::UnboundedSender<Event>,
    /// Application's topic notification receiver
    topic_notif_rx: Option<mpsc::UnboundedReceiver<TopicNotification>>,
    /// Event switch's topic notification transmitter
    topic_notif_tx: mpsc::UnboundedSender<TopicNotification>,
}

impl<Factory> Server<Factory>
where
    Factory: Fn(
        StateUpdateChannel,
        HandlerSubscriptionManager,
        EventEmitter,
    ) -> Box<dyn ServerHandler + Send + Sync>,
    Factory: Send + Sync + 'static + Copy,
{
    /// Creates a new [Server] with a config and a handler factory.
    pub fn new(config: ServerConfig, factory: Factory) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (topic_notif_tx, topic_notif_rx) = mpsc::unbounded_channel();
        Self {
            hl_version_string: format!("hl/{}", config.version_major)
                .parse()
                .unwrap(),
            config,
            factory: Arc::new(factory),
            event_rx: Some(event_rx),
            event_tx,
            topic_notif_rx: Some(topic_notif_rx),
            topic_notif_tx,
        }
    }

    /// Returns a cloned event emitter that allows the application to send events
    /// to connected HardLight clients
    pub fn get_event_emitter(&self) -> EventEmitter {
        EventEmitter(self.event_tx.clone())
    }

    /// Returns the topic notification receiver or nothing if it's already been taken.
    /// The server's event switch will send notifications of new or deleted topics to this receiver.
    pub fn get_topic_notifier(
        &mut self,
    ) -> Option<mpsc::UnboundedReceiver<TopicNotification>> {
        self.topic_notif_rx.take()
    }

    /// Starts the server
    pub async fn run(mut self) -> io::Result<()> {
        let worker_count = num_cpus::get();
        info!("Booting HL server v{}... with {} workers", HL_VERSION, worker_count);
        let acceptor = TlsAcceptor::from(Arc::new(self.config.tls.clone()));
        let listener = TcpListener::bind(&self.config.address).await?;
        info!("Listening on {} with TLS", self.config.address);

        // handlers can subscribe to event topics here
        let (subscription_notification_tx, subscription_notification_rx) =
            mpsc::unbounded_channel::<SubscriptionNotification>();

        let event_switch = EventSwitch::new(
            self.event_rx.take().unwrap(),
            subscription_notification_rx,
            self.topic_notif_tx.clone(),
        );
        tokio::spawn(event_switch.run().instrument(info_span!("event_switch")));

        loop {
            if let Ok((stream, peer_addr)) = listener.accept().await {
                self.spawn_connection_handler(
                    stream,
                    acceptor.clone(),
                    peer_addr,
                    subscription_notification_tx.clone(),
                );
            }
        }
    }

    /// Offloads a connection to a new connection handler task
    #[inline]
    fn spawn_connection_handler(
        &self,
        stream: TcpStream,
        acceptor: TlsAcceptor,
        peer_addr: SocketAddr,
        subscription_notification_tx: mpsc::UnboundedSender<
            SubscriptionNotification,
        >,
    ) -> JoinHandle<()> {
        let factory = self.factory.clone();
        let event_emitter = self.get_event_emitter();
        let version: HeaderValue = self.hl_version_string.clone();
        tokio::spawn(async move {
            // Request handlers send changes to their state to the handler, which sends
            // them down the wire to the client
            let (state_change_tx, mut state_change_rx) = mpsc::channel(10);
            
            // Request handlers don't directly talk to the event switch, rather they tell
            // their connection handlers to (un)subscribe from a topic using a ProxiedSubscriptionNotification
            let (
                proxy_subscription_notification_tx,
                mut proxy_subscription_notification_rx,
            ) = mpsc::unbounded_channel::<ProxiedSubscriptionNotification>();
            
            // The handler will run our RPC functions for us
            let handler = (factory)(
                state_change_tx,
                HandlerSubscriptionManager(proxy_subscription_notification_tx),
                event_emitter,
            );
            
            // Terminate the TLS connection using rustls into a raw byte stream
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(_) => return,
            };

            trace!("Successfully terminated TLS handshake");

            let mut compression = Compression::default();

            // This callback handles HL's protocol handshake in a moment...
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

            // Attempt to turn the raw byte stream into HL's WebSocket stream using tungstenite
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

            // for rpc request handlers to tell us when it has a response
            let (rpc_tx, mut rpc_rx) = mpsc::unbounded_channel();

            // track our own subscriptions locally
            let mut subscriptions: HashMap<Topic, SubscriptionID> = HashMap::new();
            // our connections to the event switch
            let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Event>();
            // rpc handlers
            let handler = Arc::new(handler);
            // compression level
            let compression = Arc::new(compression);

            trace!("Starting RPC handler loop");
            loop {
                select! {
                    biased;
                    // await responses from RPC calls
                    Some(msg) = rpc_rx.recv() => {
                        let id = match msg {
                            ServerMessage::RPCResponse { id, .. } => id,
                            _ => unreachable!(),
                        };

                        in_flight[id as usize] = false;

                        let _ = Server::<Factory>::send_msg(msg, &mut ws_stream, compression.clone(), "RPC response").await;
                    }
                    // await state updates from the application
                    Some(state_changes) = state_change_rx.recv() => {
                        let _ = Server::<Factory>::send_msg(ServerMessage::StateChange(state_changes), &mut ws_stream, compression.clone(), "state update").await;
                    }
                    // await events from the event switch
                    Some(event) = events_rx.recv() => {
                        let Event { topic, payload, .. } = event;
                        let _ = Server::<Factory>::send_msg(
                            ServerMessage::NewEvent { topic, payload },
                            &mut ws_stream, compression.clone(), "Event"
                        ).await;
                    }
                    // await subscription notifications from the handler
                    Some(notification) = proxy_subscription_notification_rx.recv() => {
                        let span = span!(Level::DEBUG, "subscription_notification");
                        let _enter = span.enter();

                        match notification {
                            ProxiedSubscriptionNotification::Subscribe(topic) => {
                                if !subscriptions.contains_key(&topic) {
                                    let mut id = [0; 16];
                                    rand::thread_rng().fill_bytes(&mut id);
                                    let tx = events_tx.clone();
                                    subscriptions.insert(topic.clone(), id);
                                    if subscription_notification_tx.send(
                                        SubscriptionNotification::Subscribe { topic: topic.clone(), id, tx }
                                    ).is_err() {
                                        warn!("Error sending subscription notification to handler");
                                        subscriptions.remove(&topic);
                                    }
                                }
                            },
                            ProxiedSubscriptionNotification::Unsubscribe(topic) => {
                                if let Some(id) = subscriptions.remove(&topic) {
                                    if subscription_notification_tx.send(
                                        SubscriptionNotification::Unsubscribe { topic: topic.clone(), id }
                                    ).is_err() {
                                        warn!("Error sending subscription notification to handler");
                                        subscriptions.insert(topic, id);
                                    }
                                }
                            }
                        };
                    }
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
                            let (send, recv) = oneshot::channel();
                            rayon::spawn_fifo(move || {
                                let msg = inflate(&msg.into_data()).map(|bytes| {
                                    rkyv::from_bytes::<ClientMessage>(&bytes).ok()
                                }).flatten();
                                let _ = send.send(msg);
                            });
                            yield_now().await;
                            let msg = match recv.await.ok().flatten() {
                                Some(msg) => msg,
                                None => {
                                    warn!("Error decoding message from client");
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
                                        tx.send(ServerMessage::RPCResponse { id, output: handler.handle_rpc_call(&internal).await }).unwrap();
                                    });

                                    trace!("Handler task spawned.");
                                }
                            }
                        }
                    }
                }
                yield_now().await;
            }
        }.instrument(debug_span!("connection", peer_addr = %peer_addr)))
    }

    #[inline]
    async fn send_msg(
        msg: ServerMessage,
        ws_stream: &mut WebSocketStream<TlsStream<TcpStream>>,
        compression: Arc<Compression>,
        msg_type: &'static str,
    ) -> Result<(), ()> {
        let (send, recv) = oneshot::channel();
        rayon::spawn_fifo(move || {
            let bytes = rkyv::to_bytes::<_, 1024>(&msg)
                .ok()
                .and_then(|bytes| deflate(&bytes, *compression));
            let _ = send.send(bytes);
        });
        let msg = recv.await
          .map_err(|_| {
              warn!("Background thread panicked while encoding {}", msg_type);
          })?
          .ok_or_else(|| {
              warn!("Error encoding {} for client", msg_type);
          })?;

        ws_stream.send(Message::Binary(msg)).await.map_err(|e| {
            warn!("Error sending {} to client: {}", msg_type, e);
        })
    }
}

/// A struct that allows the application server to emit events onto the event switch.
pub struct EventEmitter(mpsc::UnboundedSender<Event>);

impl EventEmitter {
    /// Emits an event to the HardLight server's event switch.
    pub async fn emit(&self, topic: &Topic, payload: impl Into<Vec<u8>>) {
        let event = Event::new(topic, payload);
        let _ = self.0.send(event);
        // tell tokio to shove us at the end of the task queue
        // this dramatically lowers latency
        yield_now().await;
    }
}

/// A struct that allows the RPC handler to tell the connection handler about the topics it's interested in
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
/// A topic that can be subscribed to.
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

/// Subscription notifications are sent from connection managers
/// to the event switch to subscribe/unsubscribe the connection
/// to/from topics.
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

/// Proxied subscription notifications are what the application handler
/// sends to the connection manager to tell it to subscribe/unsubscribe
/// to/from topics. The connection handler then does the hard work of
/// talking to the event switch and sending events down the connection.
pub enum ProxiedSubscriptionNotification {
    Subscribe(Topic),
    Unsubscribe(Topic),
}

#[derive(Clone, Debug)]
pub struct Event {
    pub topic: Topic,
    pub payload: Vec<u8>,
}

impl Event {
    pub fn new(topic: &Topic, payload: impl Into<Vec<u8>>) -> Self {
        let payload = payload.into();
        Self {
            topic: topic.clone(),
            payload,
        }
    }
}

/// Sent by the event switch to the application to tell it when topics
/// are created or removed.
pub enum TopicNotification {
    Created(Topic),
    Removed(Topic),
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
    /// The event switch notifies the application about topic changes
    topic_notif_tx: mpsc::UnboundedSender<TopicNotification>,
}

impl EventSwitch {
    fn new(
        new_event_rx: mpsc::UnboundedReceiver<Event>,
        subscription_notification_rx: mpsc::UnboundedReceiver<
            SubscriptionNotification,
        >,
        topic_notif_tx: mpsc::UnboundedSender<TopicNotification>,
    ) -> Self {
        Self {
            new_event_rx,
            subscriptions: HashMap::new(),
            subscription_notification_rx,
            topic_notif_tx,
        }
    }

    async fn run(mut self) {
        loop {
            select! {
                biased;
                Some(event) = self.new_event_rx.recv() => {
                    if let Some(subscribers) = self.subscriptions.get_mut(&event.topic) {
                        let mut subs_to_remove = vec![];
                        for (id, connection) in subscribers.iter() {
                            if connection.send(event.clone()).is_err() {
                                subs_to_remove.push(id.clone());
                            }
                        }
                        for id in subs_to_remove {
                            subscribers.remove(&id);
                        }
                    }
                    yield_now().await;
                }
                Some(notification) = self.subscription_notification_rx.recv() => {
                    match notification {
                        SubscriptionNotification::Subscribe { topic, id, tx } => {
                            let subscribers = self.subscriptions.entry(topic.clone()).or_insert_with(HashMap::new);
                            subscribers.insert(id, tx);

                            if subscribers.len() == 1 {
                                let _ = self.topic_notif_tx.send(TopicNotification::Created(topic));
                            }
                        }
                        SubscriptionNotification::Unsubscribe { topic, id } => {
                            if let Some(subscribers) = self.subscriptions.get_mut(&topic) {
                                subscribers.remove(&id);
                                if subscribers.is_empty() {
                                    self.subscriptions.remove(&topic);
                                    let _ = self.topic_notif_tx.send(TopicNotification::Removed(topic));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}