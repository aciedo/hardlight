# HardLight [![Crates.io](https://img.shields.io/crates/v/hardlight)](https://crates.io/crates/hardlight) [![Docs.rs](https://docs.rs/hardlight/badge.svg)](https://docs.rs/hardlight) [![Chat](https://img.shields.io/badge/discuss-z.valera.co-DCFF50)](https://z.valera.co/#narrow/stream/15-hardlight)

A secure, real-time, low-latency binary WebSocket RPC subprotocol.

**NOTE:** HardLight is currently in unstable development. The API, wire protocol and features are subject to change. See [below](#feature-tracking) for the current progress.

## What is HardLight?

HardLight is a binary WebSocket RPC subprotocol. It's designed to be faster (lower latencies, bigger capacity) than gRPC while being easier to use and secure by default. It's built on top of [rkyv](https://rkyv.org), a zero-copy deserialization library, and [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite) (for server/client implementations).

HardLight has two data models:

- RPC: a client connects to a server, and can call functions on the server
- Events: the server can push events of specified types to clients

An example: multiple clients subscribe to a "chat" event using Events. The connection state holds user info and what chat threads the user wants events for. Clients use an RPC function `send_message` to send a message, which will then persisted by the server and sent to subscribed clients.

HardLight is named after the fictional [Forerunner technology](https://www.halopedia.org/Hard_light) that "allows light to be transformed into a solid state, capable of bearing weight and performing a variety of tasks".

While there isn't an official "specification", we take a similar approach to Bitcoin Core, where the protocol is defined by the implementation. This implementation should be considered the "reference implementation" of the protocol, and ports should match the behaviour of this implementation.

### Feature tracking

All features will be completed for 1.0.0 apart from those marked with an asterisk.

- [x] RPC
  - [x] Connection state
    - [x] `connection_state` macro
    - [x] client state
    - [x] mutex'd server state
    - [x] server state autosync
  - [x] RPC macro
  - [ ] client
    - [x] self-signed TLS
    - [ ] wasm client*
    - [x] version agreement
    - [x] run in background
  - [x] server
    - [x] version agreement
    - [x] run in background
- [x] Events 
  - [x] wire support
  - [x] client
    - [x] event hooks of some variation
    - [ ] wasm client*
  - [x] server
    - [x] multi-connection topic management
      - [x] retrieve broadcast receivers from main server
      - [x] create topic handlers for server-scoped unique topics
      - [x] clean up topic handlers with no subscribers
    - [x] subscribe/unsubscribe a connection to a topic from server-side RPC handlers

## Features

- **Concurrent RPC**: up to 256 RPC calls can be occuring at the same time on a single connection
  - This doesn't include subscriptions, for which there are no hard limits
- **Events**: the server can push events to clients

## Install

```shell
cargo add hardlight
```

## Why WebSockets?

WebSockets actually have very little abstraction over a TCP stream. From [RFC6455](https://datatracker.ietf.org/doc/html/rfc6455#section-1.5):

> Conceptually, WebSocket is really just a layer on top of TCP that does the following:
>
> - adds a web origin-based security model for browsers
> - adds an addressing and protocol naming mechanism to support multiple services on one port and multiple host names on one IP address
> - layers a framing mechanism on top of TCP to get back to the IP packet mechanism that TCP is built on, but without length limits
> - includes an additional closing handshake in-band that is designed to work in the presence of proxies and other intermediaries

In effect, we gain the benefits of TLS, wide adoption & firewall support (it runs alongside HTTPS on TCP 443) while having little downsides. This means HardLight is usable in browsers, which was a requirement we had for the framework. In fact, we officially support using HardLight from browsers using the "wasm" feature.

At Valera, we use HardLight to connect clients to our servers, and for connecting some of our services to each another.

## Usage

HardLight is designed to be simple, secure and fast. We take advantage of Rust's trait system to allow you to define your own RPC methods, and then use the `#[rpc(State)]` macro to generate the necessary code to make it work.

Here's a very simple example of a counter server that stores an ephemeral counter per connection:

```rust
use hardlight::*;

#[tokio::main]
async fn main() {
    let config = ServerConfig::new_self_signed("localhost:8080");
    let mut server = CounterServer::new(config);
    server.start().await.unwrap();
    loop {} // server runs in background by default
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
        let mut state: StateGuard = self.state.lock();
        state.counter += amount;
        Ok(state.counter)
    } // state is automatically unlocked here; any changes are sent to the client
      // automagically ✨

    async fn decrement(&self, amount: u32) -> HandlerResult<u32> {
        let mut state = self.state.lock();
        state.counter -= amount;
        Ok(state.counter)
    }

    async fn get(&self) -> HandlerResult<u32> {
        let state = self.state.lock();
        Ok(state.counter)
    }
}
```

The `Counter` trait is shared between clients and servers. Any inputs or outputs have to support rkyv's `Serialize`, `Deserialize`, `Archive` and `CheckBytes` traits. This means you can use any primitive type, or any type that implements these traits.

The `#[rpc(State)]` macro will generate:

- a `Client` struct
- a `Handler` struct
- an enum, `RpcCall`, of all the RPC method identifiers and their arguments (e.g. `Increment`, `Decrement`, `Get`)

You'd be encouraged to put this trait in a separate crate or module, so you can use `Counter::Handler` and `Counter::Client` etc.

Both structs will expose methods for holding connection state in a hashmap. You can store anything serializable in connection state, and it will be available to all RPC methods. This is useful for storing authentication tokens, or other state that you want to be available to all RPC methods.

Subscriptions are a little more complex. They expand to traits for both the client and server, which are implemented by you. However, they don't have their own servers & clients - they're attached to RPC servers and clients.

The external runtime will handle networking, encryption and connection management.

## The `Handler`

The RPC handler generated by the macro expands to a struct that you implement your RPC methods on. It isn't a server itself, only exposing a function the runtime calls. Each connection has one handler.

This function has the signature `async fn handle_rpc_call(&mut self, input: &[u8]) -> Result<Vec<u8>, Error>`. It deserializes any inputs, matches the method to the function, calls the appropriate method, and serializes the output.

### Connection state

As each connection has its own handler, we provide connection state in each handler's `self.state`. Here, you can control extra metadata for the connection. A typical use of this would be storing authentication data. Cookies are exposed here (for `Handler`).

Internally, we use a `parking_lot::Mutex`, as a handler can be called from multiple threads (which allows us to make RPC calls concurrently over a single connection). When using state, you should lock it to the current thread using `let mut state = self.state.lock();`.

Clients cannot update connection state after the connection is established, but they can call RPC methods which can update the state from the server. This protects against race conditions.

Changes to connection state are sent to the client automatically without any extra code. This works through the locking mechanism, which wraps the Mutex's lock. This is (basically) how it works:

1. You call `let mut state = self.state.lock();` (which is a `StateGuard`)
2. Internally, we lock the mutex, and store the lock and starting data in the `StateGuard`.
3. You do whatever you want with the state, and then drop the `StateGuard`.
4. We compare the current values to the previous state. Any differences are sent to the client by calling to the parent connection.

As HardLight ultimately uses TCP, changes will properly happen in order, even if the client sends multiple RPC calls at once and packets are reordered.

## Events

Events are a little different from other subscription models like GraphQL. Instead of the client setting up subscriptions to topics, you define the types of events that can be sent over HardLight connections. The server decides what and when to send events to clients, normally based on the connection state. For example a client might specify what chat threads its subscribed to, or what financial accounts it wants realtime transactions for. This logic is handled by your app, not HardLight itself.

You attach handlers for each message type in the client.

Our general (conceptual) architecture at Valera looks like:

```console
UI <> Logic <> State <-------------> Logic <> State
|     frontend     |    HardLight    |  backend   |
```

We provide handlers to HardLight that modify state inside the frontend. The UI logic then updates the UI layer based on the state.

```rs
