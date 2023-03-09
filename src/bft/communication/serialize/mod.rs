//! This module is responsible for serializing wire messages in `febft`.
//!
//! All relevant types transmitted over the wire are `serde` aware, if
//! this feature is enabled with `serialize_serde`. Slightly more exotic
//! serialization routines, for better throughput, can be utilized, such
//! as [Cap'n'Proto](https://capnproto.org/capnp-tool.html), but these are
//! expected to be implemented by the user.

use std::io::{Read, Write};
use bytes::Bytes;

use crate::bft::communication::message::SystemMessage;
use crate::bft::crypto::hash::{Context, Digest};
use crate::bft::error::*;
use crate::bft::msg_log::persistent::ProofInfo;

#[cfg(feature = "serialize_serde")]
use ::serde::{Deserialize, Serialize};

use super::message::ConsensusMessage;

#[cfg(feature = "serialize_capnp")]
pub mod capnp;

#[cfg(feature = "serialize_serde")]
pub mod serde;

/// The buffer type used to serialize messages into.
pub type Buf = Bytes;

pub fn serialize_message<W, D>(w: &mut W, msg: &SystemMessage<D::State, D::Request, D::Reply>) -> Result<()>
    where W: Write + AsRef<[u8]> + AsMut<[u8]>, D: SharedData {

    #[cfg(feature="serialize_capnp")]
    capnp::serialize_message::<W, D>(w, msg)?;

    #[cfg(feature = "serialize_serde")]
    serde::serialize_message::<W, D>(msg, w)?;

    Ok(())
}

pub fn deserialize_message<R, D>(r: R) -> Result<SystemMessage<D::State, D::Request, D::Reply>> where R: Read + AsRef<[u8]>, D: SharedData {

    #[cfg(feature = "serialize_capnp")]
    let result = capnp::deserialize_message::<R, D>(r)?;

    #[cfg(feature= "serialize_serde")]
    let result = serde::deserialize_message::<R, D>(r)?;

    Ok(result)
}

pub fn serialize_digest<W: Write + AsRef<[u8]> + AsMut<[u8]>, D: SharedData>(
    message: &SystemMessage<D::State, D::Request, D::Reply>,
    w: &mut W,
) -> Result<Digest> {
    serialize_message::<W, D>(w, message)?;

    let mut ctx = Context::new();
    ctx.update(w.as_ref());
    Ok(ctx.finish())
}

pub fn serialize_consensus<W, D>(w: &mut W, message: &ConsensusMessage<D::Request>) -> Result<()>
    where
        W: Write + AsRef<[u8]> + AsMut<[u8]>,
        D: SharedData,
{

    #[cfg(feature = "serialize_capnp")]
    capnp::serialize_consensus::<W, D>(w, message)?;

    #[cfg(feature = "serialize_serde")]
    serde::serialize_consensus::<W, D>(message, w)?;

    Ok(())
}

pub fn deserialize_consensus<R, D>(r: R) -> Result<ConsensusMessage<D::Request>>
    where
        R: Read + AsRef<[u8]>,
        D: SharedData,
{

    #[cfg(feature = "serialize_capnp")]
    let result = capnp::deserialize_consensus::<R, D>(r)?;

    #[cfg(feature = "serialize_serde")]
    let result = serde::deserialize_consensus::<R, D>(r)?;

    Ok(result)
}



/// Marker trait containing the types used by the application,
/// as well as routines to serialize the application data.
///
/// Both clients and replicas should implement this trait,
/// to communicate with each other.
/// This data type must be Send since it will be sent across
/// threads for processing and follow up reception
pub trait SharedData: Send {
    /// The application state, which is mutated by client
    /// requests.
    #[cfg(feature = "serialize_serde")]
    type State: for<'a> Deserialize<'a> + Serialize + Send + Clone;

    #[cfg(feature = "serialize_capnp")]
    type State: Send + Clone;

    /// Represents the requests forwarded to replicas by the
    /// clients of the BFT system.
    #[cfg(feature = "serialize_serde")]
    type Request: for<'a> Deserialize<'a> + Serialize + Send + Clone;

    #[cfg(feature = "serialize_capnp")]
    type Request: Send + Clone;

    /// Represents the replies forwarded to clients by replicas
    /// in the BFT system.
    #[cfg(feature = "serialize_serde")]
    type Reply: for<'a> Deserialize<'a> + Serialize + Send + Clone;

    #[cfg(feature = "serialize_capnp")]
    type Reply: Send + Clone;

    ///Serialize a state so it can be utilized by the SMR middleware
    ///  (either for network sending or persistent storing)
    fn serialize_state<W>(w: W, state: &Self::State) -> Result<()> where W: Write;

    ///Deserialize a state generated by the serialize_state function.
    fn deserialize_state<R>(r: R) -> Result<Self::State> where R: Read;

    ///Serialize a request from your service, given the writer to serialize into
    fn serialize_request<W>(w: W, request: &Self::Request) -> Result<()> where W: Write;

    ///Deserialize a request that was generated by the serialize request function above
    fn deserialize_request<R>(r: R) -> Result<Self::Request> where R: Read;

    ///Serialize a reply into a given writer
    fn serialize_reply<W>(w: W, reply: &Self::Reply) -> Result<()> where W: Write;

    ///Deserialize a reply that was generated using the serialize reply function above
    fn deserialize_reply<R>(r: R) -> Result<Self::Reply> where R: Read;
}


