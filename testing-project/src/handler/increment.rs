use hardlight::*;

use super::Handler;

pub async fn increment(
    connection: &Handler,
    amount: u32,
) -> HandlerResult<u32> {
    // lock the state to the current thread
    let mut state = connection.state.write().await;
    state.counter += amount;
    Ok(state.counter)
}
