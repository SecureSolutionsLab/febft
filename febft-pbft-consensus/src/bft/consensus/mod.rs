use std::cmp::Reverse;
use std::collections::{BinaryHeap, BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use either::Either;
use event_listener::Event;
use log::{debug, error, info, trace, warn};

use atlas_common::error::*;
use atlas_common::node_id::NodeId;
use atlas_common::ordering::{Orderable, SeqNo, tbo_advance_message_queue, tbo_advance_message_queue_return, tbo_queue_message};
use atlas_communication::message::{Header, StoredMessage};
use atlas_communication::protocol_node::ProtocolNetworkNode;
use atlas_core::messages::{ClientRqInfo, RequestMessage, StoredRequestMessage, SystemMessage};
use atlas_core::ordering_protocol::ProtocolConsensusDecision;
use atlas_core::persistent_log::{OrderingProtocolLog, StatefulOrderingProtocolLog};
use atlas_core::serialize::{LogTransferMessage, StateTransferMessage};
use atlas_core::timeouts::Timeouts;
use atlas_execution::ExecutorHandle;
use atlas_execution::serialize::ApplicationData;
use atlas_metrics::metrics::metric_increment;

use crate::bft::{PBFT, SysMsg};
use crate::bft::consensus::decision::{ConsensusDecision, DecisionPollStatus, DecisionStatus, MessageQueue};
use crate::bft::message::{ConsensusMessage, ConsensusMessageKind, PBFTMessage};
use crate::bft::message::serialize::PBFTConsensus;
use crate::bft::metric::OPERATIONS_PROCESSED_ID;
use crate::bft::msg_log::decided_log::Log;
use crate::bft::msg_log::deciding_log::CompletedBatch;
use crate::bft::msg_log::decisions::{DecisionLog, IncompleteProof, Proof};
use crate::bft::sync::{AbstractSynchronizer, Synchronizer};
use crate::bft::sync::view::ViewInfo;

pub mod decision;
pub mod accessory;

#[derive(Debug, Clone)]
/// Status returned from processing a consensus message.
pub enum ConsensusStatus {
    /// A particular node tried voting twice.
    VotedTwice(NodeId),
    /// A `febft` quorum still hasn't made a decision
    /// on a client request to be executed.
    Deciding,
    /// A `febft` quorum decided on the execution of
    /// the batch of requests with the given digests.
    /// The first digest is the digest of the Prepare message
    /// And therefore the entire batch digest
    /// THe second Vec<Digest> is a vec with digests of the requests contained in the batch
    /// The third is the messages that should be persisted for this batch to be considered persisted
    Decided,
}

#[derive(Debug, Clone)]
/// Represents the status of calling `poll()` on a `Consensus`.
pub enum ConsensusPollStatus<O> {
    /// The `Replica` associated with this `Consensus` should
    /// poll its main channel for more messages.
    Recv,
    /// A new consensus message is available to be processed.
    NextMessage(Header, ConsensusMessage<O>),
    /// The first consensus instance of the consensus queue is ready to be finalized
    /// as it has already been decided
    Decided,
}

/// Represents a queue of messages to be ordered in a consensus instance.
///
/// Because of the asynchronicity of the Internet, messages may arrive out of
/// context, e.g. for the same consensus instance, a `PRE-PREPARE` reaches
/// a node after a `PREPARE`. A `TboQueue` arranges these messages to be
/// processed in the correct order.
pub struct TboQueue<O> {
    curr_seq: SeqNo,
    watermark: u32,
    get_queue: bool,
    pre_prepares: VecDeque<VecDeque<StoredMessage<ConsensusMessage<O>>>>,
    prepares: VecDeque<VecDeque<StoredMessage<ConsensusMessage<O>>>>,
    commits: VecDeque<VecDeque<StoredMessage<ConsensusMessage<O>>>>,
}

impl<O> Orderable for TboQueue<O> {
    /// Reports the id of the consensus this `TboQueue` is tracking.
    fn sequence_number(&self) -> SeqNo {
        self.curr_seq
    }
}

impl<O> TboQueue<O> {
    fn new(curr_seq: SeqNo, watermark: u32) -> Self {
        Self {
            curr_seq,
            watermark,
            get_queue: false,
            pre_prepares: VecDeque::new(),
            prepares: VecDeque::new(),
            commits: VecDeque::new(),
        }
    }

    fn base_seq(&self) -> SeqNo {
        self.curr_seq + SeqNo::from(self.watermark)
    }

    /// Signal this `TboQueue` that it may be able to extract new
    /// consensus messages from its internal storage.
    pub fn signal(&mut self) {
        self.get_queue = true;
    }

    fn advance_queue(&mut self) -> MessageQueue<O> {
        self.curr_seq = self.curr_seq.next();

        let pre_prepares = tbo_advance_message_queue_return(&mut self.pre_prepares)
            .unwrap_or_else(|| VecDeque::new());
        let prepares = tbo_advance_message_queue_return(&mut self.prepares)
            .unwrap_or_else(|| VecDeque::new());
        let commits = tbo_advance_message_queue_return(&mut self.commits)
            .unwrap_or_else(|| VecDeque::new());

        MessageQueue::from_messages(pre_prepares, prepares, commits)
    }


    /// Advances the message queue, and updates the consensus instance id.
    fn next_instance_queue(&mut self) {
        self.curr_seq = self.curr_seq.next();
        tbo_advance_message_queue(&mut self.pre_prepares);
        tbo_advance_message_queue(&mut self.prepares);
        tbo_advance_message_queue(&mut self.commits);
    }

    /// Queues a consensus message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    pub fn queue(&mut self, h: Header, m: ConsensusMessage<O>) {
        match m.kind() {
            ConsensusMessageKind::PrePrepare(_) => self.queue_pre_prepare(h, m),
            ConsensusMessageKind::Prepare(_) => self.queue_prepare(h, m),
            ConsensusMessageKind::Commit(_) => self.queue_commit(h, m),
        }
    }

    /// Queues a `PRE-PREPARE` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_pre_prepare(&mut self, h: Header, m: ConsensusMessage<O>) {
        tbo_queue_message(
            self.base_seq(),
            &mut self.pre_prepares,
            StoredMessage::new(h, m),
        )
    }

    /// Queues a `PREPARE` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_prepare(&mut self, h: Header, m: ConsensusMessage<O>) {
        tbo_queue_message(self.base_seq(), &mut self.prepares, StoredMessage::new(h, m))
    }

    /// Queues a `COMMIT` message for later processing, or drops it
    /// immediately if it pertains to an older consensus instance.
    fn queue_commit(&mut self, h: Header, m: ConsensusMessage<O>) {
        tbo_queue_message(self.base_seq(), &mut self.commits, StoredMessage::new(h, m))
    }

    /// Clear this queue
    fn clear(&mut self) {
        self.get_queue = false;
        self.pre_prepares.clear();
        self.prepares.clear();
        self.commits.clear();
    }
}

/// A data structure to keep track of any consensus instances that have been signalled
///
/// A consensus instance being signalled means it should be polled.
#[derive(Debug)]
pub struct Signals {
    // Prevent duplicates efficiently
    signaled_nos: BTreeSet<SeqNo>,
    signaled_seq_no: BinaryHeap<Reverse<SeqNo>>,
}

/// The consensus handler. Responsible for multiplexing consensus instances and keeping track
/// of missing messages
pub struct Consensus<D, ST, LP, PL>
    where D: ApplicationData + 'static,
          ST: StateTransferMessage + 'static,
          LP: LogTransferMessage + 'static,
          PL: Clone {
    node_id: NodeId,
    /// The handle to the executor of the function
    executor_handle: ExecutorHandle<D>,
    /// How many consensus instances can we overlap at the same time.
    watermark: u32,
    /// The current seq no that we are currently in
    seq_no: SeqNo,
    /// The current sequence numbers that are awaiting polling
    signalled: Signals,
    /// The current view that we are in
    curr_view: ViewInfo,
    /// The consensus instances that are currently being processed
    /// A given consensus instance n will only be finished when all consensus instances
    /// j, where j < n have already been processed, in order to maintain total ordering
    decisions: VecDeque<ConsensusDecision<D, ST, LP, PL>>,
    /// The queue for messages that sit outside the range seq_no + watermark
    /// These messages cannot currently be processed since they sit outside the allowed
    /// zone but they will be processed once the seq no moves forward enough to include them
    tbo_queue: TboQueue<D::Request>,
    /// This queue serves for us to keep track of messages we receive of coming up views.
    /// This is important for us to be able to continue the process of moving views after a view change
    view_queue: VecDeque<Vec<StoredMessage<ConsensusMessage<D::Request>>>>,
    /// The consensus guard that will be used to ensure that the proposer only proposes one batch
    /// for each consensus instance
    consensus_guard: Arc<ProposerConsensusGuard>,
    /// A reference to the timeouts
    timeouts: Timeouts,
    /// Check if we are currently recovering from a fault, meaning we should ignore timeouts
    is_recovering: bool,

    persistent_log: PL,
}

impl<D, ST, LP, PL> Consensus<D, ST, LP, PL> where D: ApplicationData + 'static,
                                                   ST: StateTransferMessage + 'static,
                                                   LP: LogTransferMessage + 'static,
                                                   PL: Clone {
    pub fn new_replica(node_id: NodeId, view: &ViewInfo, executor_handle: ExecutorHandle<D>, seq_no: SeqNo,
                       watermark: u32, consensus_guard: Arc<ProposerConsensusGuard>, timeouts: Timeouts,
                       persistent_log: PL) -> Self {
        let mut curr_seq = seq_no;

        let mut consensus = Self {
            node_id,
            executor_handle,
            watermark,
            seq_no,
            signalled: Signals::new(watermark),
            curr_view: view.clone(),
            decisions: VecDeque::with_capacity(watermark as usize),
            tbo_queue: TboQueue::new(seq_no, watermark),
            view_queue: VecDeque::with_capacity(watermark as usize),
            consensus_guard,
            timeouts,
            is_recovering: false,
            persistent_log,
        };

        // Initialize the consensus instances
        for _ in 0..watermark {
            let decision = ConsensusDecision::init_decision(
                node_id,
                curr_seq,
                view,
                consensus.persistent_log.clone(),
            );

            consensus.enqueue_decision(decision);

            curr_seq += SeqNo::ONE;
        }

        consensus
    }

    /// Queue a given message into our message queues.
    pub fn queue(&mut self, header: Header, message: ConsensusMessage<D::Request>) {
        let message_seq = message.sequence_number();

        let view_seq = message.view();

        match view_seq.index(self.curr_view.sequence_number()) {
            Either::Right(i) if i > 0 => {
                self.enqueue_other_view_message(i, header, message);

                return;
            }
            Either::Right(_) => {}
            Either::Left(_) => {
                // The message pertains to older views
                warn!("{:?} // Ignoring consensus message {:?} received from {:?} as we are already in view {:?}",
                    self.node_id, message, header.from(), self.curr_view.sequence_number());

                return;
            }
        };

        let i = match message_seq.index(self.seq_no) {
            Either::Right(i) => i,
            Either::Left(_) => {
                // The message pertains to older consensus instances

                warn!("{:?} // Ignoring consensus message {:?} received from {:?} as we are already in seq no {:?}",
                    self.node_id, message, header.from(), self.seq_no);

                return;
            }
        };

        if i >= self.decisions.len() {
            debug!("{:?} // Queueing message out of context msg {:?} received from {:?} into tbo queue",
                self.node_id, message, header.from());

            // We are not currently processing this consensus instance
            // so we need to queue the message
            self.tbo_queue.queue(header, message);
        } else {
            debug!("{:?} // Queueing message out of context msg {:?} received from {:?} into the corresponding decision {}",
                self.node_id, message, header.from(), i);
            // Queue the message in the corresponding pending decision
            self.decisions.get_mut(i).unwrap().queue(header, message);

            // Signal that we are ready to receive messages
            self.signalled.push_signalled(message_seq);
        }
    }

    /// Poll the given consensus
    pub fn poll(&mut self) -> ConsensusPollStatus<D::Request> {
        trace!("Current signal queue: {:?}", self.signalled);

        while let Some(seq_no) = self.signalled.pop_signalled() {
            let index = seq_no.index(self.seq_no);

            if let Either::Right(index) = index {
                let poll_result = self.decisions[index].poll();

                match poll_result {
                    DecisionPollStatus::NextMessage(header, message) => {
                        // We had a message pending, so it's possible that there are more messages
                        // Pending
                        self.signalled.push_signalled(seq_no);

                        return ConsensusPollStatus::NextMessage(header, message);
                    }
                    DecisionPollStatus::TryPropose => {
                        // This won't generate a loop since only the first poll will actually return
                        // A TryProposeAndRecv poll status. All other subsequent polls will
                        // Behave as a regular poll and either return recv or next message
                        self.signalled.push_signalled(seq_no);

                        //TODO: Prevent non leaders from forming an always increasing
                        // List of available sequence numbers
                        if self.curr_view.leader_set().contains(&self.node_id) {
                            self.consensus_guard.make_seq_available(seq_no);
                        }
                    }
                    _ => {}
                }
            } else { error!("Cannot possibly poll sequence number that is in the past {:?} vs current {:?}", seq_no, self.seq_no) }
        }

        // If the first decision in the queue is decided, then we must finalize it
        // Before doing anything else
        // This shouldn't happen since this decided is always returned first from process message
        // and it will be handled until completion from there, but having a backup is never
        // A bad idea
        if self.can_finalize() {
            return ConsensusPollStatus::Decided;
        }

        ConsensusPollStatus::Recv
    }

    pub fn process_message<NT>(&mut self,
                                   header: Header,
                                   message: ConsensusMessage<D::Request>,
                                   synchronizer: &Synchronizer<D>,
                                   timeouts: &Timeouts,
                                   log: &mut Log<D, PL>,
                                   node: &Arc<NT>) -> Result<ConsensusStatus>
        where NT: ProtocolNetworkNode<PBFT<D, ST, LP>> + 'static,
              PL: OrderingProtocolLog<PBFTConsensus<D>> {
        let message_seq = message.sequence_number();

        let view_seq = message.view();

        match view_seq.index(self.curr_view.sequence_number()) {
            Either::Right(i) if i > 0 => {
                self.enqueue_other_view_message(i, header, message);

                return Ok(ConsensusStatus::Deciding);
            }
            Either::Right(_) => {}
            Either::Left(_) => {
                // The message pertains to older views
                warn!("{:?} // Ignoring consensus message {:?} received from {:?} as we are already in view {:?}",
                    self.node_id, message, header.from(), self.curr_view.sequence_number());

                return Ok(ConsensusStatus::Deciding);
            }
        };

        let i = match message_seq.index(self.seq_no) {
            Either::Right(i) => i,
            Either::Left(_) => {
                warn!("Message {:?} from {:?} is behind our current sequence no {:?}. Ignoring", message, header.from(), self.seq_no, );

                return Ok(ConsensusStatus::Deciding);
            }
        };

        if i >= self.decisions.len() {
            // We are not currently processing this consensus instance
            // so we need to queue the message
            debug!("{:?} // Queueing message {:?} for seq no {:?}", self.node_id, message, message_seq);

            self.tbo_queue.queue(header, message);

            return Ok(ConsensusStatus::Deciding);
        }

        // Get the correct consensus instance for this message
        let decision = self.decisions.get_mut(i).unwrap();

        let status = decision.process_message(header, message, synchronizer, timeouts, log, node)?;

        Ok(match status {
            DecisionStatus::VotedTwice(node) => {
                ConsensusStatus::VotedTwice(node)
            }
            DecisionStatus::Deciding => {
                ConsensusStatus::Deciding
            }
            DecisionStatus::Queued | DecisionStatus::Transitioned => {
                //When we transition phases, we may discover new messages
                // That were in the queue, so we must be signalled again
                self.signalled.push_signalled(message_seq);

                ConsensusStatus::Deciding
            }
            DecisionStatus::Decided => {
                ConsensusStatus::Decided
            }
        })
    }

    /// Are we able to finalize the next consensus instance on the queue?
    pub fn can_finalize(&self) -> bool {
        self.decisions.front().map(|d| d.is_finalizeable()).unwrap_or(false)
    }

    pub(super) fn finalizeable_count(&self) -> usize {
        let mut count = 0;

        for decision in &self.decisions {
            if decision.is_finalizeable() {
                count += 1;
            } else {
                break;
            }
        }

        count
    }

    /// Finalize the next consensus instance if possible
    pub fn finalize(&mut self, view: &ViewInfo) -> Result<Option<CompletedBatch<D::Request>>> {

        // If the decision can't be finalized, then we can't finalize the batch
        if let Some(decision) = self.decisions.front() {
            if !decision.is_finalizeable() {
                return Ok(None);
            }
        } else {
            // This should never happen
            panic!("Front of the decision queue is empty?");
        }

        // Move to the next instance of the consensus since the current one is going to be finalized
        let decision = self.next_instance(view);

        let batch = decision.finalize()?;

        info!("{:?} // Finalizing consensus instance {:?} with {:?} rqs", self.node_id, batch.sequence_number(), batch.request_count());

        metric_increment(OPERATIONS_PROCESSED_ID, Some(batch.request_count() as u64));

        Ok(Some(batch))
    }

    /// Advance to the next instance of the consensus
    /// This will also create the necessary new decision to keep the pending decisions
    /// equal to the water mark
    pub fn next_instance(&mut self, view: &ViewInfo) -> ConsensusDecision<D, ST, LP, PL> {
        let decision = self.decisions.pop_front().unwrap();

        self.seq_no = self.seq_no.next();

        //Get the next message queue from the tbo queue. If there are no messages present
        // (expected during normal operations, then we will create a new message queue)
        let queue = self.tbo_queue.advance_queue();

        if !queue.is_signalled() && self.is_recovering {
            self.is_recovering = false;

            // This means the queue is empty.
            self.timeouts.cancel_client_rq_timeouts(None);
        }

        let new_seq_no = self.decisions.back()
            .map(|d| d.sequence_number().next())
            // If the watermark is 1, then the seq no of the
            .unwrap_or(self.seq_no);

        // Create the decision to keep the queue populated
        let novel_decision = ConsensusDecision::init_with_msg_log(self.node_id,
                                                                  new_seq_no,
                                                                  view,
                                                                  self.persistent_log.clone(), queue, );

        self.enqueue_decision(novel_decision);

        decision
    }

    /// Install the received state into the consensus
    pub fn install_state(&mut self,
                         view_info: ViewInfo,
                         dec_log: &DecisionLog<D::Request>) -> Result<(Vec<D::Request>)> {

        // get the latest seq no
        let seq_no = {
            let last_exec = dec_log.last_execution();
            if last_exec.is_none() {
                self.sequence_number()
            } else {
                last_exec.unwrap()
            }
        };

        if seq_no > SeqNo::ZERO {
            // If we have installed a new state, then we must be recovering and therefore should
            // Stop timeouts
            self.is_recovering = true;
        }

        // skip old messages
        self.install_sequence_number(seq_no.next(), &view_info);

        // Update the decisions with the new view information
        self.install_view(&view_info);

        let mut reqs = Vec::with_capacity(dec_log.proofs().len());

        for proof in dec_log.proofs() {
            if !proof.are_pre_prepares_ordered()? {
                unreachable!()
            }

            for pre_prepare in proof.pre_prepares() {
                let x: &ConsensusMessage<D::Request> = pre_prepare.message().consensus();

                match x.kind() {
                    ConsensusMessageKind::PrePrepare(pre_prepare_reqs) => {
                        for req in pre_prepare_reqs {
                            let rq_msg: &RequestMessage<D::Request> = req.message();

                            reqs.push(rq_msg.operation().clone());
                        }
                    }
                    _ => { unreachable!() }
                }
            }
        }

        Ok(reqs)
    }

    pub fn install_sequence_number(&mut self, novel_seq_no: SeqNo, view: &ViewInfo) {
        info!("{:?} // Installing sequence number {:?} vs current {:?}", self.node_id, novel_seq_no, self.seq_no);

        match novel_seq_no.index(self.seq_no) {
            Either::Left(_) => {
                debug!("{:?} // Installed sequence number is left of the current on. Clearing all queues", self.node_id);

                self.clear_all_queues();

                let mut sequence_no = novel_seq_no;

                while self.decisions.len() < self.watermark as usize {
                    let novel_decision = ConsensusDecision::init_decision(self.node_id,
                                                                          sequence_no, view, self.persistent_log.clone());

                    self.enqueue_decision(novel_decision);

                    sequence_no = sequence_no + SeqNo::ONE;
                }

                self.tbo_queue.curr_seq = novel_seq_no;
                self.seq_no = novel_seq_no;
            }
            Either::Right(0) => {
                // We are in the correct sequence number
                debug!("{:?} // Installed sequence number is the same as the current one. No action required", self.node_id);
            }
            Either::Right(limit) if limit >= self.decisions.len() => {
                debug!("{:?} // Installed sequence number is right of the current one and is larger than the decisions we have stored. Clearing stored decisions.", self.node_id);

                // We have more skips to do than currently watermarked decisions,
                // so we must clear all our decisions and then consume from the tbo queue
                // Until all decisions that have already been saved to the log are discarded of
                self.decisions.clear();
                self.signalled.clear();
                self.consensus_guard.clear();

                let mut sequence_no = novel_seq_no;

                let mut overflow = limit - self.watermark as usize;

                if overflow >= self.tbo_queue.pre_prepares.len() {
                    debug!("{:?} // Decision log overflow is larger than the tbo queue. Clearing tbo queue {} vs {}", self.node_id, overflow, self.tbo_queue.pre_prepares.len());

                    // If we have more overflow than stored in the tbo queue, then
                    // We must clear the entire tbo queue and start fresh
                    self.tbo_queue.clear();

                    self.tbo_queue.curr_seq = novel_seq_no;
                } else {
                    debug!("{:?} // Decision log overflow eats into the tbo queue. Removing {} out of {} seqs", self.node_id, overflow, self.tbo_queue.pre_prepares.len());

                    for _ in 0..overflow {
                        // Read the next overflow consensus instances and dispose of them
                        // As they have already been registered to the log
                        self.tbo_queue.next_instance_queue();
                    }
                }

                /// Get the next few already populated message queues from the tbo queue.
                /// This will also adjust the tbo queue sequence number to the correct one
                while self.tbo_queue.sequence_number() < novel_seq_no && self.decisions.len() < self.watermark as usize {
                    let messages = self.tbo_queue.advance_queue();

                    let decision = ConsensusDecision::init_with_msg_log(self.node_id, sequence_no, view,
                                                                        self.persistent_log.clone(), messages);

                    debug!("{:?} // Initialized new decision from TBO queue messages {:?}", self.node_id, decision.sequence_number());

                    self.enqueue_decision(decision);

                    sequence_no += SeqNo::ONE;
                }

                while self.decisions.len() < self.watermark as usize {
                    let decision = ConsensusDecision::init_decision(self.node_id, sequence_no,
                                                                    view, self.persistent_log.clone());

                    self.enqueue_decision(decision);

                    sequence_no += SeqNo::ONE;
                }

                self.seq_no = novel_seq_no;
            }
            Either::Right(limit) => {
                debug!("{:?} // Installed sequence number is right of the current one and is smaller than the decisions we have stored. Removing decided decisions.", self.node_id);

                for _ in 0..limit {
                    // Pop the decisions that have already been made and dispose of them
                    self.decisions.pop_front();
                }

                // The decision at the head of the list is now novel_seq_no

                // Get the last decision in the decision queue.
                // The following new consensus decisions will have the sequence number of the last decision
                let mut sequence_no: SeqNo = self.decisions.back().unwrap().sequence_number().next();

                while self.decisions.len() < self.watermark as usize {
                    // We advanced [`limit`] sequence numbers on the decisions,
                    // so by advancing the tbo queue the missing decisions, we will
                    // Also advance [`limit`] sequence numbers on the tbo queue, which is the intended
                    // Behaviour
                    let messages = self.tbo_queue.advance_queue();

                    let decision = ConsensusDecision::init_with_msg_log(self.node_id, sequence_no,
                                                                        view, self.persistent_log.clone(), messages);

                    self.enqueue_decision(decision);

                    sequence_no += SeqNo::ONE;
                }

                self.seq_no = novel_seq_no;
            }
        }

        self.consensus_guard.install_seq_no(novel_seq_no);
        self.tbo_queue.signal();

        // A couple of assertions to make sure we are good
        assert_eq!(self.tbo_queue.sequence_number(), self.seq_no);
        assert_eq!(self.decisions.front().unwrap().sequence_number(), self.seq_no);
    }

    /// Catch up to the quorums latest decided consensus
    pub fn catch_up_to_quorum(&mut self,
                              seq: SeqNo,
                              view: &ViewInfo,
                              proof: Proof<D::Request>,
                              log: &mut Log<D, PL>) -> Result<ProtocolConsensusDecision<D::Request>>
        where PL: OrderingProtocolLog<PBFTConsensus<D>> {

        // If this is successful, it means that we are all caught up and can now start executing the
        // batch
        let to_execute = log.install_proof(seq, proof)?;

        // Move to the next instance as this one has been finalized
        self.next_instance(view);

        Ok(to_execute)
    }

    /// Create a fake `PRE-PREPARE`. This is useful during the view
    /// change protocol.
    pub fn forge_propose<K>(
        &self,
        requests: Vec<StoredRequestMessage<D::Request>>,
        synchronizer: &K,
    ) -> SysMsg<D, ST, LP>
        where
            K: AbstractSynchronizer<D>,
    {
        SystemMessage::from_protocol_message(PBFTMessage::Consensus(ConsensusMessage::new(
            self.sequence_number(),
            synchronizer.view().sequence_number(),
            ConsensusMessageKind::PrePrepare(requests),
        )))
    }

    /// Install a given view into the current consensus decisions.
    pub fn install_view(&mut self, view: &ViewInfo) {
        let view_index = match view.sequence_number().index(self.curr_view.sequence_number()) {
            Either::Right(i) => { i }
            Either::Left(_) => {
                error!("{:?} // Attempted to install a view that is not ahead of the current view. Ignoring.", self.node_id);

                return;
            }
        };

        debug!("{:?} // Installing view {:?}, view index: {}", self.node_id, view.sequence_number(), view_index);

        if view_index == 0 {
            // We are in the same view
            return;
        }

        self.curr_view = view.clone();
        self.consensus_guard.install_view(view.clone());

        // Since we are changing view, all messages from the previous view are now invalid
        self.clear_all_queues();

        let mut sequence_no = self.sequence_number();

        while self.decisions.len() < self.watermark as usize {
            let novel_decision = ConsensusDecision::init_decision(self.node_id, sequence_no, view, self.persistent_log.clone());

            self.enqueue_decision(novel_decision);

            sequence_no = sequence_no + SeqNo::ONE;
        }

        if view_index > 1 {
            for _ in 0..view_index - 1 {
                self.view_queue.pop_front();
            }
        }

        let option = self.view_queue.pop_front();

        debug!("{:?} // Installing view {:?}, view index: {}. View queue: {:?}", self.node_id, view.sequence_number(), view_index, option);

        if let Some(messages) = option {
            for message in messages {
                let (header, message) = message.into_inner();

                self.queue(header, message);
            }
        }
    }

    /// Enqueue a message from another view into it's correct queue
    fn enqueue_other_view_message(&mut self, index: usize, header: Header, message: ConsensusMessage<D::Request>) {
        debug!("{:?} // Enqueuing a message from another view into the view queue. Index {}  {:?}", self.node_id, index, message);

        // Adjust the index to be 0 based
        let index = index - 1;

        while self.view_queue.len() <= index {
            self.view_queue.push_back(Vec::new());
        }

        self.view_queue[index].push(StoredMessage::new(header, message));
    }


    /// Finalize the view change protocol
    pub fn finalize_view_change<NT>(
        &mut self,
        (header, message): (Header, ConsensusMessage<D::Request>),
        synchronizer: &Synchronizer<D>,
        timeouts: &Timeouts,
        log: &mut Log<D, PL>,
        node: &Arc<NT>,
    ) where NT: ProtocolNetworkNode<PBFT<D, ST, LP>> + 'static,
            PL: OrderingProtocolLog<PBFTConsensus<D>> {
        let view = synchronizer.view();
        //Prepare the algorithm as we are already entering this phase

        //TODO: when we finalize a view change, we want to treat the pre prepare request
        // As the only pre prepare, since it already has info provided by everyone in the network.
        // Therefore, this should go straight to the Preparing phase instead of waiting for
        // All the view's leaders.

        self.install_view(&view);

        if let ConsensusMessageKind::PrePrepare(reqs) = &message.kind() {
            let mut final_rqs = Vec::with_capacity(reqs.len());

            for x in reqs {
                final_rqs.push(ClientRqInfo::from(x));
            }

            // Register the messages that we have received in this pre prepare from the view change
            // So the proposer doesn't repeat them
            self.consensus_guard.install_sync_message_requests(final_rqs);
        }

        // Advance the initialization phase of the first decision, which is the current decision
        // So the proposer won't try to propose anything to this decision
        self.decisions[0].skip_init_phase();

        self.process_message(header, message, synchronizer, timeouts, log, node).unwrap();

        self.consensus_guard.unlock_consensus();
    }

    /// Collect the incomplete proof that is currently being decided
    pub fn collect_incomplete_proof(&self, f: usize) -> IncompleteProof {
        if let Some(decision) = self.decisions.front() {
            decision.deciding(f)
        } else {
            unreachable!()
        }
    }

    /// Enqueue a decision onto our overlapping decision log
    fn enqueue_decision(&mut self, decision: ConsensusDecision<D, ST, LP, PL>) {
        self.signalled.push_signalled(decision.sequence_number());

        self.decisions.push_back(decision);
    }

    /// Clear all of the queues that are associated with this
    /// consensus
    fn clear_all_queues(&mut self) {
        self.decisions.clear();
        self.tbo_queue.clear();
        self.signalled.clear();
        self.consensus_guard.clear();
    }

    pub(super) fn is_catching_up(&self) -> bool {
        // If we have a bunch of messages still to process,
        // Don't listen to timeouts
        self.is_recovering
    }
}

impl<D, ST, LP, PL> Orderable for Consensus<D, ST, LP, PL>
    where D: ApplicationData + 'static,
          ST: StateTransferMessage + 'static,
          LP: LogTransferMessage + 'static,
          PL: Clone {
    fn sequence_number(&self) -> SeqNo {
        self.seq_no
    }
}

/// The consensus guard for handling when the proposer should propose and to which consensus instance
pub struct ProposerConsensusGuard {
    /// Can I propose batches at this time
    can_propose: AtomicBool,
    /// The proposer should sleep until we are ready to start proposing again
    event_waker: Event,
    /// The revolving door of available sequence numbers to propose to
    /// We want to have a Min Heap so we reverse the SeqNo's ordering
    seq_no_queue: Mutex<(BinaryHeap<Reverse<SeqNo>>, ViewInfo)>,
    /// Cached check so we don't always have to lock the last_view_change mutex
    has_pending_view_change_reqs: AtomicBool,
    /// A list of all requests sent by the leader in the SYNC message.
    /// These requests should not be repeated.
    /// We must store them due to the way the request pre processor
    /// sends requests to the proposer
    last_view_change: Mutex<Option<BTreeMap<NodeId, BTreeMap<SeqNo, SeqNo>>>>,
}

impl ProposerConsensusGuard {
    /// Initialize a new consensus guard object
    pub(super) fn new(view: ViewInfo, watermark: u32) -> Arc<Self> {
        Arc::new(Self {
            // We start at false since we have to wait for the state transfer protocol
            can_propose: AtomicBool::new(false),
            event_waker: Event::new(),
            seq_no_queue: Mutex::new((BinaryHeap::with_capacity(watermark as usize), view)),
            has_pending_view_change_reqs: AtomicBool::new(false),
            last_view_change: Mutex::new(None),
        })
    }

    /// Are we able to propose to the current consensus instance
    pub fn can_propose(&self) -> bool {
        self.can_propose.load(Ordering::Relaxed)
    }

    /// Block until we are ready to start proposing again
    pub fn block_until_ready(&self) {
        self.event_waker.listen().wait();
    }

    /// Lock the consensus, making it impossible for the proposer to propose any requests
    pub fn lock_consensus(&self) {
        self.can_propose.store(false, Ordering::Relaxed);

        debug!("Locked consensus");
    }

    /// Unlock the consensus instance
    pub fn unlock_consensus(&self) {
        self.can_propose.store(true, Ordering::Relaxed);

        self.event_waker.notify(usize::MAX);

        debug!("Unlocking consensus")
    }

    /// Get the next sequence number to propose to
    pub fn next_seq_no(&self) -> Option<(SeqNo, ViewInfo)> {
        let mut guard = self.seq_no_queue.lock().unwrap();

        guard.0.pop().map(|first| (first.0, guard.1.clone()))
    }

    /// Mark a given consensus sequence number as available to be proposed to
    pub fn make_seq_available(&self, seq: SeqNo) {
        debug!("Making sequence number {:?} available for the proposer", seq);

        let mut guard = self.seq_no_queue.lock().unwrap();

        guard.0.push(Reverse(seq));
    }

    /// Install a given sequence number onto this consensus guard
    pub fn install_seq_no(&self, installed_seq: SeqNo) {
        let mut guard = self.seq_no_queue.lock().unwrap();

        // Remove any sequence number that precedes the newly installed sequence number
        while let Some(seq) = guard.0.peek() {
            if seq.0 < installed_seq {
                guard.0.pop();
            }
        }
    }

    /// Install a new view onto this consensus guard
    pub fn install_view(&self, view: ViewInfo) {
        let mut guard = self.seq_no_queue.lock().unwrap();

        guard.1 = view;
        guard.0.clear();
    }

    /// Check if we have pending view change requests
    pub fn has_pending_view_change_reqs(&self) -> bool {
        self.has_pending_view_change_reqs.load(Ordering::Relaxed)
    }

    /// Get the information about the last view change requests
    pub fn last_view_change(&self) -> &Mutex<Option<BTreeMap<NodeId, BTreeMap<SeqNo, SeqNo>>>> {
        &self.last_view_change
    }

    /// Install the sync message requests onto this consensus guard
    pub(crate) fn install_sync_message_requests(&self, rqs: Vec<ClientRqInfo>) {
        let mut client_map = BTreeMap::new();

        warn!("Installing sync message requests. Total: {} requests", rqs.len());

        for req in rqs {
            let entry = client_map.entry(req.sender).or_insert_with(BTreeMap::new);

            let session_entry = entry.entry(req.session).or_insert_with(|| req.seq_no);

            if *session_entry < req.seq_no {
                *session_entry = req.seq_no;
            }
        }

        {
            let mut guard = self.last_view_change.lock().unwrap();

            //FIXME: If there is already a sync message, we should merge the two
            *guard = Some(client_map);

            self.has_pending_view_change_reqs.store(true, Ordering::Relaxed);
        }
    }

    /// Clear the messages we are storing (as it is already empty)
    pub(crate) fn sync_messages_clear(&self) {
        let mut guard = self.last_view_change.lock().unwrap();

        *guard = None;

        self.has_pending_view_change_reqs.store(false, Ordering::Relaxed);
    }

    /// Clear all of the pending decisions waiting for a propose from this consensus guard
    fn clear(&self) {
        self.seq_no_queue.lock().unwrap().0.clear();
    }
}

impl Signals {
    fn new(watermark: u32) -> Self {
        Self {
            signaled_nos: Default::default(),
            signaled_seq_no: BinaryHeap::with_capacity(watermark as usize),
        }
    }

    /// Pop a signalled sequence number
    fn pop_signalled(&mut self) -> Option<SeqNo> {
        self.signaled_seq_no.pop().map(|reversed| {
            let seq_no = reversed.0;

            self.signaled_nos.remove(&seq_no);

            seq_no
        })
    }

    /// Mark a given sequence number as signalled
    fn push_signalled(&mut self, seq: SeqNo) {
        if self.signaled_nos.insert(seq) {
            self.signaled_seq_no.push(Reverse(seq));
        }
    }

    fn clear(&mut self) {
        self.signaled_nos.clear();
        self.signaled_seq_no.clear();
    }
}