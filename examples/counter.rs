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
    info!("Counter: {}", client.get().await.unwrap());
    client.increment(5).await.unwrap();
    info!("Counter: {}", client.get().await.unwrap());
    client.decrement(2).await.unwrap();
    info!("Counter: {}", client.get().await.unwrap());
    info!("{:?}", client.state().await.unwrap());
}

/// These RPC methods are executed on the server and can be called by clients.
#[rpc]
trait Counter {
    async fn increment(&self, amount: u32) -> HandlerResult<u32>;
    async fn decrement(&self, amount: u32) -> HandlerResult<u32>;
    async fn get(&self) -> HandlerResult<u32>;
}

#[connection_state]
struct State {
    counter: u32,
}

#[rpc_handler]
impl Counter for Handler {
    async fn increment(&self, amount: u32) -> HandlerResult<u32> {
        // lock the state to the current thread
        let mut state: StateGuard = self.state.write().await;
        state.counter += amount;
        Ok(state.counter)
    } // state is automatically unlocked here; any changes are sent to the client
      // automagically ✨

    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        let mut state = self.state.write().await;
        state.counter -= amount;
        Ok(state.counter)
    }

    async fn get(&self) -> HandlerResult<u32> {
        let state = self.state.read().await;
        Ok(state.counter)
    }
}
