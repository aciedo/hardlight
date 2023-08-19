mod client;
mod server;
mod wire;

use std::io::Write;

pub use async_trait;
pub use bytecheck;
pub use client::*;
use flate2::write::{
    DeflateDecoder as Decompressor, DeflateEncoder as Compressor,
};
pub use flate2::Compression;
use futures_util::Future;
pub use hardlight_macros::*;
pub use parking_lot;
pub use rkyv;
use rkyv::{
    de::deserializers::SharedDeserializeMap,
    ser::serializers::{
        AlignedSerializer, AllocScratch, CompositeSerializer, FallbackScratch,
        HeapScratch, SharedSerializeMap,
    },
    validation::validators::DefaultValidator,
    AlignedVec, Archive, CheckBytes, Deserialize, Serialize,
};
pub use rkyv_derive;
pub use server::*;
pub use tokio;
pub use tokio_macros;
pub use tokio_tungstenite::tungstenite;
pub use tracing;
use tracing::trace;
pub use wire::*;

pub(crate) fn inflate(msg: &[u8]) -> Option<Vec<u8>> {
    let mut decompressor = Decompressor::new(vec![]);
    decompressor.write_all(msg).ok()?;
    let out = decompressor.finish().ok();
    trace!(
        "Inflate: {} bytes -> {} bytes",
        msg.len(),
        out.as_ref().map(|v| v.len()).unwrap_or(0)
    );
    out
}

pub(crate) fn deflate(msg: &[u8], level: Compression) -> Option<Vec<u8>> {
    let mut compressor = Compressor::new(vec![], level);
    compressor.write_all(msg).ok()?;
    let out = compressor.finish().ok();
    trace!(
        "Deflate: {} bytes -> {} bytes",
        msg.len(),
        out.as_ref().map(|v| v.len()).unwrap_or(0)
    );
    out
}

#[macro_export]
/// A macro to create a factory function for a handler.
/// Example:
/// ```
/// factory!(Handler)
/// ```
/// expands to
/// ```
/// |suc, nstx| Box::new(Handler::new(suc, nstx))
/// ```
macro_rules! factory {
    ($handler:ty) => {
        |suc, nstx, etx| Box::new(<$handler>::new(suc, nstx, etx))
    };
}

/// Awaits a future and serializes the result. This is a helper function for
/// implementing [ServerHandler::handle_rpc_call].
pub async fn handle<F, O>(f: F) -> HandlerResult<Vec<u8>>
where
    F: Future<Output = O>,
    O: Serialize<
        CompositeSerializer<
            AlignedSerializer<AlignedVec>,
            FallbackScratch<HeapScratch<1024>, AllocScratch>,
            SharedSerializeMap,
        >,
    >,
{
    let result = f.await;
    let result = rkyv::to_bytes::<_, 1024>(&result).unwrap();
    Ok(result.to_vec())
}

/// Deserializes a slice of bytes into a type that implements [Archive].
pub fn deserialize<'a, T>(bytes: &'a [u8]) -> HandlerResult<T>
where
    T: rkyv::Archive,
    <T as Archive>::Archived: for<'b> CheckBytes<DefaultValidator<'b>>
        + Deserialize<T, SharedDeserializeMap>,
{
    rkyv::from_bytes::<T>(bytes).map_err(|_| RpcHandlerError::ServerDecodeError)
}
