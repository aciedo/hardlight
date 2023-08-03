use std::sync::Arc;

use hardlight::*;

#[rpc(no_server_handler)]
pub trait Counter {
    async fn increment(&self, amount: u32) -> HandlerResult<u32>;
    async fn decrement(&self, amount: u32) -> HandlerResult<u32>;
    /// We'll deprecate this at some point as we can just send it using Events
    async fn get(&self) -> HandlerResult<u32>;
    /// A simple function that does nothing and returns nothing
    async fn test_overhead(&self) -> HandlerResult<()>;
}

#[connection_state]
pub struct State {
    pub counter: u32,
}

pub struct Handler {
    // the runtime will provide the state when it creates the handler
    pub state: Arc<StateController>,
}

#[rpc_handler]
impl ServerHandler for Handler {
    fn new(state_update_channel: StateUpdateChannel) -> Self {
        Self {
            state: Arc::new(StateController::new(state_update_channel)),
        }
    }

    async fn handle_rpc_call(&self, input: &[u8]) -> HandlerResult<Vec<u8>> {
        match deserialize(input)? {
            RpcCall::Increment { amount } => {
                handle(increment(self, amount)).await
            }
            RpcCall::Decrement { amount } => {
                handle(decrement(self, amount)).await
            }
            RpcCall::Get {} => handle(get(self)).await,
            RpcCall::TestOverhead {} => Ok(vec![]),
        }
    }
}

async fn increment(connection: &Handler, amount: u32) -> HandlerResult<u32> {
    let mut state = connection.state.write().await;
    state.counter += amount;
    Ok(state.counter)
}

async fn decrement(connection: &Handler, amount: u32) -> HandlerResult<u32> {
    let mut state = connection.state.write().await;
    state.counter -= amount;
    Ok(state.counter)
}

async fn get(connection: &Handler) -> HandlerResult<u32> {
    let state = connection.state.read().await;
    Ok(state.counter)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::sleep;

    use super::*;

    #[tokio::test]
    async fn test_counter() {
        // Initialize the server
        let config = ServerConfig::new_self_signed("localhost:8070"); // use a different port for testing
        let server = Server::new(config.clone(), factory!(Handler));
        tokio::spawn(async move {
            // run the server in a separate task
            server.run().await.unwrap();
        });

        // wait for the server to start
        sleep(Duration::from_millis(10)).await;

        // Initialize the client
        let mut counter = CounterClient::new_self_signed(
            "localhost:8070",
            Compression::default(),
        );

        counter.connect().await.unwrap();

        // Test increment
        let res = counter.increment(10).await.unwrap();
        assert_eq!(res, 10);

        // Test decrement
        let res = counter.decrement(5).await.unwrap();
        assert_eq!(res, 5);

        // Test get
        let res = counter.get().await.unwrap();
        assert_eq!(res, 5);
    }
}
