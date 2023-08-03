use hardlight::{connection_state, rpc, HandlerResult};

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

// enum Events {
//     Increment(u32),
//     Decrement(u32),
// }
