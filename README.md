# HardLight [![Crates.io](https://img.shields.io/crates/v/hardlight)](https://crates.io/crates/hardlight) [![Docs.rs](https://docs.rs/hardlight/badge.svg)](https://docs.rs/hardlight) [![Chat](https://img.shields.io/badge/discuss-z.valera.co-DCFF50)](https://z.valera.co/#narrow/stream/15-hardlight)

A secure, real-time, low-latency binary WebSocket RPC subprotocol.

```rs
use std::time::Instant;

use hardlight::*;
use tokio::time::{sleep, Duration};
use tracing::{info, info_span, Instrument};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // --- server side code
    tokio::spawn(
        async move {
            let config = ServerConfig::new_self_signed("localhost:8080");
            let mut server = Server::new(config, factory!(Handler));

            // multiple emitters can be created (just keep calling this)
            // this lets you send events to topics (which will be sent to all
            // clients subscribed to that topic)
            let event_emitter = server.get_event_emitter();
            // only one topic notifier can be created
            // this will receive notifications about topics being created and
            // removed
            let mut topic_notifier = server.get_topic_notifier().unwrap();

            tokio::spawn(
                async move {
                    while let Some(notif) = topic_notifier.recv().await {
                        match notif {
                            TopicNotification::Created(topic) => {
                                info!("Topic created: {:?}", topic);
                                event_emitter
                                    .emit(&topic, CounterEvent::ServerSentTest)
                                    .await;
                            }
                            TopicNotification::Removed(topic) => {
                                info!("Topic removed: {:?}", topic);
                            }
                        }
                    }
                }
                .instrument(info_span!("topic_notifier")),
            );

            server.run().await.unwrap()
        }
        .instrument(info_span!("server")),
    );

    // --- client side code
    tokio::spawn(
        async move {
            // the macros generate a client struct for us
            let mut client = CounterClient::new_self_signed(
                "localhost:8080",
                Compression::default(),
            );

            client.connect().await.unwrap();

            // get a client event receiver
            // this will only receive the events from topics the server handler
            // has subscribed this client to
            let mut subscription = client.subscribe().await.unwrap();

            // spawn a task to receive events in the background and print them
            tokio::spawn(
                async move {
                    while let Ok(event) = subscription.recv().await {
                        let (topic, event): (Topic, CounterEvent) =
                            (event.topic, event.payload.into());
                        info!(
                            "Received event on topic {:?}: {:?}",
                            topic, event
                        );
                    }
                }
                .instrument(info_span!("subscription")),
            );
            // create a new counter and subscribes the client to it

            client.new_counter().await.unwrap();

            // find out what topic we are subscribed to
            let topic = {
                let start = Instant::now();
                let mut topic = client.state().await.unwrap().topic.clone();
                while topic.as_bytes().is_empty() {
                    sleep(Duration::from_nanos(100)).await;
                    topic = client.state().await.unwrap().topic.clone();
                }
                info!(
                    "Received state update for topic {:?} after RPC response",
                    start.elapsed()
                );
                topic
            };
            info!("Subscribed to topic {:?}", topic);

            // this will emit an "incremented" event to the counter's topic
            client.increment(1).await.unwrap();
            // this will emit a "decremented" event to the counter's topic
            client.decrement(1).await.unwrap();

            info!("Counter: {}", client.get().await.unwrap());
            info!("{:?}", client.state().await.unwrap());
        }
        .instrument(info_span!("client")),
    )
    .await
    .unwrap();
}

#[codable]
#[derive(Debug, Clone)]
enum CounterEvent {
    /// Emitted when the counter is incremented.
    Incremented { to: u32, from: u32 },
    /// Emitted when the counter is decremented.
    Decremented { to: u32, from: u32 },
    /// Emitted when the server sends a test event.
    ServerSentTest,
}

/// These RPC methods are executed on the server and can be called by clients.
#[rpc]
trait Counter {
    /// Creates a new counter.
    async fn new_counter(&self) -> HandlerResult<()>;
    /// Increments the counter by the given amount.
    async fn increment(&self, amount: u32) -> HandlerResult<()>;
    /// Decrements the counter by the given amount.
    async fn decrement(&self, amount: u32) -> HandlerResult<()>;
    /// Gets the current value of the counter.
    async fn get(&self) -> HandlerResult<u32>;
}

#[connection_state]
struct State {
    counter: u32,
    topic: Topic,
}

#[rpc_handler]
impl Counter for Handler {
    async fn new_counter(&self) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        self.subscriptions.remove(&state.topic);
        state.topic = (0..1)
            .map(|_| rand::random::<u8>())
            .collect::<Vec<_>>()
            .into();
        state.counter = 0;
        self.subscriptions.add(&state.topic);
        Ok(())
    }

    async fn increment(&self, amount: u32) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        let new = state.counter + amount;
        let event = CounterEvent::Incremented {
            to: new,
            from: state.counter,
        };
        state.counter = new;
        self.events.emit(&state.topic, event).await;
        Ok(())
    }

    async fn decrement(&self, amount: u32) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        let new = state.counter - amount;
        let event = CounterEvent::Decremented {
            to: new,
            from: state.counter,
        };
        state.counter = new;
        self.events.emit(&state.topic, event).await;
        Ok(())
    }

    async fn get(&self) -> HandlerResult<u32> {
        Ok(self.state.read().await.counter)
    }
}
```

## What is HardLight?

HardLight is a binary WebSocket RPC subprotocol. It's designed to be faster (lower latencies, bigger capacity) than gRPC while being easier to use and secure by default. It's built on top of [rkyv](https://rkyv.org), a zero-copy deserialization library, and [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite) (for server/client implementations).

HardLight has two data models:

- RPC: a client connects to a server, and can call functions on the server
- Events: the server can push events of specified types to clients

An example: multiple clients subscribe to a "chat-topic-abc" topic using Events. The connection state holds user info, synced between client and server automatically. Clients use an RPC function `send_message` to send a message, which will then persisted by the server and sent to subscribed clients. You can tie your own event infrastructure using the topic notifier when creating a server. We use NATS at Valera.

HardLight is named after the fictional [Forerunner technology](https://www.halopedia.org/Hard_light) that "allows light to be transformed into a solid state, capable of bearing weight and performing a variety of tasks".

While there isn't an official "specification", we take a similar approach to Bitcoin Core, where the protocol is defined by the implementation. This implementation should be considered the "reference implementation" of the protocol, and ports should match the behaviour of this implementation.

### Feature tracking (in priority order)

- [x] RPC
- [x] Connection state
- [x] Macros
- [x] Events
- [ ] WASM support
- [ ] Better docs

## Features

- **Concurrent RPC**: up to 256 RPC calls can be occuring at the same time on a single connection
- **Events**: very powerful server-sent events
- **No IDL**: no need to write a `.proto` file, just use Rust types and stick `#[codable]` on them

## Install

```shell
cargo add hardlight
```

## Why WebSockets?

WebSockets actually have very little abstraction over a TCP stream. From [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455#section-1.5):

> Conceptually, WebSocket is really just a layer on top of TCP that does the following:
>
> - adds a web origin-based security model for browsers
> - adds an addressing and protocol naming mechanism to support multiple services on one port and multiple host names on one IP address
> - layers a framing mechanism on top of TCP to get back to the IP packet mechanism that TCP is built on, but without length limits
> - includes an additional closing handshake in-band that is designed to work in the presence of proxies and other intermediaries

In effect, we gain the benefits of TLS, wide adoption & firewall support (it runs alongside HTTPS on TCP 443) while having little downsides. This means HardLight is usable in browsers, which was a requirement we had for the framework. In fact, we officially support using HardLight from browsers using the "wasm" feature.

At Valera, we use HardLight to connect Atlas SDK to Atlas servers.

**Note:** We're investigating adding QUIC as well. This won't be a breaking change - we'll support both WebSockets and QUIC on the same port number. WS on TCP 443, QUIC on UDP 443.