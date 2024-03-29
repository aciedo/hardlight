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
