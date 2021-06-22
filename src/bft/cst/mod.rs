//! The collaborative state transfer algorithm.
//!
//! The implementation is based on the paper «On the Efﬁciency of
//! Durable State Machine Replication», by A. Bessani et al.

use crate::bft::prng::State as RngState;

/// Represents the state of an on-going colloborative
/// state transfer protocol execution.
pub struct CollabStTransfer {
    phase: ProtoPhase,
    rng: RngState,
}

enum ProtoPhase {
    Init,
    ReceivingCid(usize),
    ReceivingAppState(usize),
}
