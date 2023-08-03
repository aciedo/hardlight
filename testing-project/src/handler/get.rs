use hardlight::*;

use super::Handler;

pub async fn get(connection: &Handler) -> HandlerResult<u32> {
    let state = connection.state.read().await;
    Ok(state.counter)
}
