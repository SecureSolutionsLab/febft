//! This module is responsible for serializing wire messages in `febft`.
//!
//! If using the [Cap'n'Proto](https://capnproto.org/capnp-tool.html) backend,
//! the API for the module will be slightly different. Users can opt to enable
//! the [serde](https://serde.rs/) backend instead, which has a much more flexible
//! API, but performs worse, in general, because it doesn't have a
//! zero-copy architecture, like Cap'n'Proto.
//!
//! Configuring one over the other is done with the following feature flags:
//!
//! - `serialize_capnp`
//! - `serialize_serde_BACKEND`, where `BACKEND` may be `bincode`, for instance.
//!   Consult the `Cargo.toml` file for more alternatives.

#[cfg(feature = "serialize_capnp")]
mod capnp;

#[cfg(feature = "serialize_serde")]
mod serde;

#[cfg(feature = "serialize_serde")]
use ::serde::{Serialize, Deserialize};

use bytes::{Buf, BufMut};

use crate::bft::error::*;
use crate::bft::communication::message::SystemMessage;

#[cfg(feature = "serialize_capnp")]
pub use self::capnp::{ToCapnp, FromCapnp};

/// Serialize a wire message into the buffer `B`.
///
/// Once the operation is finished, the buffer is returned.
#[cfg(feature = "serialize_capnp")]
pub fn serialize_message<O: ToCapnp, B: BufMut>(buf: B, m: SystemMessage<O>) -> Result<B> {
    capnp::serialize_message(buf, m)
}

/// Serialize a wire message into the buffer `B`.
///
/// Once the operation is finished, the buffer is returned.
#[cfg(feature = "serialize_serde")]
pub fn serialize_message<O, B>(buf: B, m: SystemMessage<O>) -> Result<B>
where
    O: Serialize,
    B: BufMut,
{
    #[cfg(feature = "serialize_serde_bincode")]
    { serde::bincode::serialize_message(buf, m) }

    #[cfg(feature = "serialize_serde_messagepack")]
    { serde::messagepack::serialize_message(buf, m) }

    #[cfg(feature = "serialize_serde_cbor")]
    { serde::cbor::serialize_message(buf, m) }
}

/// Deserialize a wire message from a buffer `B`.
#[cfg(feature = "serialize_capnp")]
pub fn deserialize_message<O: FromCapnp, B: Buf>(buf: B) -> Result<SystemMessage<O>> {
    capnp::deserialize_message(buf)
}

/// Deserialize a wire message from a buffer `B`.
#[cfg(feature = "serialize_serde")]
pub fn deserialize_message<O, B>(buf: B) -> Result<SystemMessage<O>>
where
    O: for<'de> Deserialize<'de>,
    B: Buf,
{
    #[cfg(feature = "serialize_serde_bincode")]
    { serde::bincode::deserialize_message(buf) }

    #[cfg(feature = "serialize_serde_messagepack")]
    { serde::messagepack::deserialize_message(buf) }

    #[cfg(feature = "serialize_serde_cbor")]
    { serde::cbor::deserialize_message(buf) }
}
