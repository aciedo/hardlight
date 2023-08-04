mod decrement;
mod increment;

use std::sync::Arc;

use crate::service::*;
use hardlight::*;

use self::{decrement::*, increment::*};

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
            RpcCall::Get {} => handle::<_, HandlerResult<_>>(async move {
                let state = self.state.read().await;
                Ok(state.counter)
            }).await,
            RpcCall::TestOverhead {} => Ok(vec![]),
        }
    }
}
