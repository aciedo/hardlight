mod decrement;
mod increment;

use std::sync::Arc;

use crate::service::*;
use hardlight::*;
use tokio::task::yield_now;

use self::{decrement::*, increment::*};

pub struct Handler {
    // the runtime will provide the state when it creates the handler
    pub state: Arc<StateController>,
    subscription_notification_tx: UnboundedSender<ProxiedSubscriptionNotification>,
    event_tx: UnboundedSender<Event>
}

#[rpc_handler]
impl ServerHandler for Handler {
    fn new(
        suc: StateUpdateChannel, 
        sntx: UnboundedSender<ProxiedSubscriptionNotification>,
        etx: UnboundedSender<Event>
    ) -> Self {
        Self {
            state: Arc::new(StateController::new(suc)),
            subscription_notification_tx: sntx,
            event_tx: etx
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
    
    /// Subscribes the connection to the given topic
    fn subscribe(&self, topic: Topic) {
        self.subscription_notification_tx.send(ProxiedSubscriptionNotification::Subscribe(topic)).unwrap();
    }
    
    /// Unsubscribes the connection from the given topic
    fn unsubscribe(&self, topic: Topic) {
        self.subscription_notification_tx.send(ProxiedSubscriptionNotification::Unsubscribe(topic)).unwrap();
    }
    
    /// Emits an event to the event switch
    async fn emit(&self, event: Event) {
        self.event_tx.send(event).unwrap();
        yield_now().await;
    }
}
