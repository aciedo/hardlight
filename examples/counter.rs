use std::time::{SystemTime, Duration};

use hardlight::*;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = ServerConfig::new_self_signed("localhost:8080");
    let server = Server::new(config, factory!(Handler));
    tokio::spawn(async move { server.run().await.unwrap() });
    let mut client = CounterClient::new_self_signed(
        "localhost:8080",
        Compression::default(),
    );
    client.connect().await.unwrap();
    let mut subscription = client.subscribe().await.unwrap();
    tokio::spawn(async move {
        while let Ok(event) = subscription.recv().await {
            let (topic, event): (Topic, CounterEvent) = (event.topic, event.payload.into());
            info!("Received event on topic {:?}: {:?}", topic, event);
        }
    });
    client.new_counter().await.unwrap();
    for _ in 0..100 {
        client.increment(1).await.unwrap();
    }
    info!("Counter: {}", client.get().await.unwrap());
    info!("{:?}", client.state().await.unwrap());
}

#[codable]
#[derive(Debug, Clone)]
enum CounterEvent {
    Incremented {
        to: u32,
        from: u32,
    },
    Decremented {
        to: u32,
        from: u32,
    },
}

/// These RPC methods are executed on the server and can be called by clients.
#[rpc]
trait Counter {
    async fn new_counter(&self) -> HandlerResult<()>;
    async fn increment(&self, amount: u32) -> HandlerResult<()>;
    async fn decrement(&self, amount: u32) -> HandlerResult<()>;
    async fn get(&self) -> HandlerResult<u32>;
}

#[connection_state]
struct State {
    counter: u32,
    topic: Topic
}

#[rpc_handler]
impl Counter for Handler {
    async fn new_counter(&self) -> HandlerResult<()> {
        let mut state: StateGuard = self.state.write().await;
        state.topic = (0..1).map(|_| rand::random::<u8>()).collect::<Vec<_>>().into();
        state.counter = 0;
        self.subscribe(state.topic.clone());
        Ok(())
    }
    
    async fn increment(&self, amount: u32) -> HandlerResult<()> {
        let mut state: StateGuard = self.state.write().await;
        let event = CounterEvent::Incremented {
            to: state.counter + amount,
            from: state.counter,
        };
        state.counter += amount;
        self.emit(Event::new(state.topic.clone(), event.into())).await;
        Ok(())
    }

    async fn decrement(&self, amount: u32) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        let event = CounterEvent::Decremented {
            to: state.counter - amount,
            from: state.counter,
        };
        state.counter -= amount;
        self.emit(Event::new(state.topic.clone(), event.into())).await;
        Ok(())
    }

    async fn get(&self) -> HandlerResult<u32> {
        let state = self.state.read().await;
        Ok(state.counter)
    }
}
