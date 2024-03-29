use rkyv::{Archive, CheckBytes, Deserialize, Serialize};

use crate::{StateUpdate, Topic};

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
pub enum ClientMessage {
    /// A message from the client when it calls a method on the server.
    RPCRequest {
        /// A unique counter for each RPC call.
        /// This is used to match responses to requests. It restricts the
        /// number of concurrent operations to 256. Active operations
        /// cannot reuse the same ID, therefore IDs of completed
        /// requests can be reused.
        id: u8,
        /// The internal message serialized with rkyv. This will include the
        /// method name and arguments. The format of this message will slightly
        /// differ depending on the number of methods, and types of arguments.
        /// The macros handle generating the code for this.
        internal: Vec<u8>,
    },
}

#[derive(Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
pub enum ServerMessage {
    /// A message from the server with the output of a client's method call.
    RPCResponse {
        /// A unique counter for each RPC call.
        /// This is used to match responses to requests. It restricts the
        /// number of concurrent operations to 256. Active operations
        /// cannot reuse the same ID, therefore IDs of completed
        /// requests can be reused.
        id: u8,
        /// The function's output serialized with rkyv. The format of this
        /// message will differ with each application.
        /// The macros handle generating the code for this.
        output: Result<Vec<u8>, RpcHandlerError>,
    },
    /// A message from the server with a new event.
    NewEvent {
        /// The event serialized with rkyv. The format of this will differ
        /// between applications. The macros handle generating the code for
        /// this.
        topic: Topic,
        payload: Vec<u8>,
    },
    /// The server updates the connection state.
    StateChange(Vec<StateUpdate>),
}

#[derive(Archive, Serialize, Deserialize, Debug)]
#[archive(check_bytes)]
pub enum RpcHandlerError {
    /// You tried to make an RPC call before your client was connected
    ClientNotConnected,
    /// You've tried to make too many RPC calls at once.
    /// This will come from your client instead of the server, which
    /// will simply ignore your RPC request to prevent overloads.
    TooManyCallsInFlight,
    /// Client failed to encode something
    ClientEncodeError,
    /// Client failed to decode something
    ClientDecodeError,
    /// Client failed to encode something
    ServerEncodeError,
    /// Client failed to decode something
    ServerDecodeError,
    ClientCompressError,
    ClientDecompressError,
    ServerCompressError,
    ServerDecompressError,
    /// The server handler task fatally crashed during execution
    HandlerPanicked
}
