use hardlight::*;

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
        let mut state: StateGuard = self.state.write().await;
        state.counter += amount;
        Ok(state.counter)
    }

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::sleep;

    use super::*;

    #[tokio::test]
    async fn test_counter() {
        let config = ServerConfig::new_self_signed("localhost:8080"); // use a different port for testing
        let server = Server::new(config.clone(), factory!(Handler));
        tokio::spawn(async move {
            server.run().await.unwrap();
        });
        sleep(Duration::from_millis(10)).await;

        // Initialize the client
        let mut counter = CounterClient::new_self_signed(
            "localhost:8080",
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
