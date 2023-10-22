use std::{sync::Arc, time::Duration};

use crate::service::*;
use hardlight::*;
use tokio::time::sleep;

pub struct Handler {
    // the runtime will provide the state when it creates the handler
    pub state: Arc<StateController>,
    subscriptions: HandlerSubscriptionManager,
    events: EventEmitter,
}

#[rpc_handler]
impl ServerHandler for Handler {
    fn new(
        suc: StateUpdateChannel,
        subscriptions: HandlerSubscriptionManager,
        events: EventEmitter,
    ) -> Self {
        Self {
            state: Arc::new(StateController::new(suc)),
            subscriptions,
            events,
        }
    }

    async fn handle_rpc_call(&self, input: &[u8]) -> HandlerResult<Vec<u8>> {
        match de(input)? {
            RpcCall::Increment { amount } => {
                let mut state = self.state.write().await;
                state.counter += amount;
                ser(state.counter).await
            }
            RpcCall::Decrement { amount } => {
                let mut state = self.state.write().await;
                state.counter -= amount;
                ser(state.counter).await
            }
            RpcCall::Get {} => {
                let state = self.state.read().await;
                ser(state.counter).await
            }
            RpcCall::TestOverhead {} => {
                sleep(Duration::from_millis(10)).await;
                Ok(vec![])
            }
        }
    }
}
