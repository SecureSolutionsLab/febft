//! This module is responsible for serializing wire messages in `febft`.
//!
//! All relevant types transmitted over the wire are `serde` aware, if
//! this feature is enabled with `serialize_serde`. Slightly more exotic
//! serialization routines, for better throughput, can be utilized, such
//! as [Cap'n'Proto](https://capnproto.org/capnp-tool.html), but these are
//! expected to be implemented by the user.

use std::io::{Read, Write};
use std::marker::PhantomData;
use bytes::Bytes;
use febft_common::error::*;

use crate::bft::msg_log::persistent::ProofInfo;

#[cfg(feature = "serialize_serde")]
use ::serde::{Deserialize, Serialize};
use febft_common::crypto::hash::{Context, Digest};
use febft_communication::serialize::Serializable;
use febft_execution::serialize::SharedData;
use febft_messages::serialize::OrderingProtocol;
use crate::bft::message::{ConsensusMessage, PBFTMessage};

#[cfg(feature = "serialize_capnp")]
pub mod capnp;

#[cfg(feature = "serialize_serde")]
pub mod serde;

/// The buffer type used to serialize messages into.
pub type Buf = Bytes;

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

/// The serializable type, to be used to appease the compiler and it's requirements
pub struct PBFTConsensus<D: SharedData>(PhantomData<D>);

impl<D> OrderingProtocol for PBFTConsensus<D> where D: SharedData {

    type ProtocolMessage = PBFTMessage<D::State, D::Request>;

    #[cfg(feature = "serialize_capnp")]
    fn serialize_capnp(builder: febft_capnp::consensus_messages_capnp::protocol_message::Builder, msg: &Self::ProtocolMessage) -> Result<()> {
        capnp::serialize_message::<D>(builder, msg)
    }

    #[cfg(feature = "serialize_capnp")]
    fn deserialize_capnp(reader: febft_capnp::consensus_messages_capnp::protocol_message::Reader) -> Result<Self::ProtocolMessage> {
        capnp::deserialize_message::<D>(reader)
    }
}