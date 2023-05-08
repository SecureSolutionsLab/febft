//! This module contains the implementation details of `febft`.
//!
//! By default, it is hidden to the user, unless explicitly enabled
//! with the feature flag `expose_impl`.

pub mod consensus;
pub mod proposer;
pub mod sync;
pub mod msg_log;
pub mod config;
pub mod message;
pub mod observer;
pub mod metric;

use std::ops::Drop;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use log::{debug, info, LevelFilter};
use log4rs::{
    append::file::FileAppender,
    config::{Appender, Logger, Root},
    Config,
    encode::pattern::PatternEncoder,
};
use febft_common::error::*;
use febft_common::{async_runtime, socket, threadpool};
use febft_common::error::ErrorKind::CommunicationPeerNotFound;
use febft_common::globals::{Flag, ReadOnly};
use febft_common::node_id::NodeId;
use febft_common::ordering::{Orderable, SeqNo};
use febft_communication::message::{Header, StoredMessage};
use febft_communication::Node;
use febft_communication::serialize::Serializable;
use febft_execution::app::{Request, Service, State};
use febft_execution::ExecutorHandle;
use febft_execution::serialize::SharedData;
use febft_messages::messages::{ForwardedRequestsMessage, Protocol, SystemMessage};
use febft_messages::ordering_protocol::{OrderingProtocol, OrderProtocolExecResult, OrderProtocolPoll, View};
use febft_messages::serialize::{OrderingProtocolMessage, ServiceMsg, StateTransferMessage};
use febft_messages::state_transfer::{Checkpoint, DecLog, StatefulOrderProtocol};
use febft_messages::timeouts::{ClientRqInfo, Timeout, TimeoutKind, Timeouts};
use crate::bft::config::PBFTConfig;
use crate::bft::consensus::{Consensus, ProposerConsensusGuard, ConsensusPollStatus, ConsensusStatus};
use crate::bft::message::{ConsensusMessage, ConsensusMessageKind, ObserveEventKind, PBFTMessage, ViewChangeMessage};
use crate::bft::message::serialize::PBFTConsensus;
use crate::bft::msg_log::decided_log::Log;
use crate::bft::msg_log::{Info, initialize_decided_log, initialize_pending_request_log, initialize_persistent_log};
use crate::bft::msg_log::pending_decision::PendingRequestLog;
use crate::bft::msg_log::persistent::{NoPersistentLog, PersistentLogModeTrait};
use crate::bft::observer::{MessageType, ObserverHandle};
use crate::bft::proposer::Proposer;
use crate::bft::sync::{AbstractSynchronizer, Synchronizer, SynchronizerPollStatus, SynchronizerStatus};
use crate::bft::sync::view::ViewInfo;

// The types responsible for this protocol
pub type PBFT<D, ST> = ServiceMsg<D, PBFTConsensus<D>, ST>;
// The message type for this consensus protocol
pub type SysMsg<D, ST> = <PBFT<D, ST> as Serializable>::Message;

#[derive(Clone, PartialEq, Eq, Debug)]
/// Which phase of the consensus algorithm are we currently executing
pub enum ConsensusPhase {
    // The normal phase of the consensus
    NormalPhase,
    // The synchronizer phase of the consensus
    SyncPhase,
}

/// The result of advancing the sync phase
#[derive(Debug)]
pub enum SyncPhaseRes {
    SyncProtocolNotNeeded,
    RunSyncProtocol,
    SyncProtocolFinished,
    RunCSTProtocol,
}


/// a PBFT based ordering protocol
pub struct PBFTOrderProtocol<D, ST, NT>
    where
        D: SharedData + 'static,
        ST: StateTransferMessage + 'static,
        NT: Node<PBFT<D, ST>> + 'static {
    // What phase of the consensus algorithm are we currently executing
    phase: ConsensusPhase,

    /// The consensus state machine
    consensus: Consensus<D, ST>,
    /// The synchronizer state machine
    synchronizer: Arc<Synchronizer<D>>,

    // A reference to the timeouts layer
    timeouts: Timeouts,

    //The proposer guard
    consensus_guard: Arc<ProposerConsensusGuard>,
    // Check if unordered requests can be proposed.
    // This can only occur when we are in the normal phase of the state machine
    unordered_rq_guard: Arc<AtomicBool>,

    // The pending request log. Handles requests received by this replica
    // Or forwarded by others that have not yet made it into a consensus instance
    pending_request_log: Arc<PendingRequestLog<D>>,
    // The log of the decided consensus messages
    // This is completely owned by the server thread and therefore does not
    // Require any synchronization
    message_log: Log<D>,
    // The proposer of this replica
    proposer: Arc<Proposer<D, NT>>,
    // The networking layer for a Node in the network (either Client or Replica)
    node: Arc<NT>,

    executor: ExecutorHandle<D>,
}

impl<D, ST, NT> Orderable for PBFTOrderProtocol<D, ST, NT> where D: 'static + SharedData, NT: 'static + Node<PBFT<D, ST>>, ST: 'static + StateTransferMessage {
    fn sequence_number(&self) -> SeqNo {
        self.consensus.sequence_number()
    }
}

impl<D, ST, NT> OrderingProtocol<D, NT> for PBFTOrderProtocol<D, ST, NT>
    where D: SharedData + 'static,
          ST: StateTransferMessage + 'static,
          NT: Node<PBFT<D, ST>> + 'static, {
    type Serialization = PBFTConsensus<D>;
    type Config = PBFTConfig<D, ST>;

    fn initialize(config: PBFTConfig<D, ST>, executor: ExecutorHandle<D>,
                  timeouts: Timeouts, node: Arc<NT>) -> Result<Self> where
        Self: Sized,
    {
        Self::initialize_protocol(config, executor, timeouts, node, None)
    }

    fn view(&self) -> View<Self::Serialization> {
        self.synchronizer.view()
    }

    fn handle_off_ctx_message(&mut self, message: StoredMessage<Protocol<PBFTMessage<D::Request>>>) {
        let (header, message) = message.into_inner();

        match message.into_inner() {
            PBFTMessage::Consensus(consensus) => {
                debug!("{:?} // Received off context consensus message {:?}", self.node.id(), consensus);
                self.consensus.queue(header, consensus);
            }
            PBFTMessage::ViewChange(view_change) => {
                debug!("{:?} // Received off context view change message {:?}", self.node.id(), view_change);
                self.synchronizer.queue(header, view_change);

                self.synchronizer.signal();
            }
            _ => { todo!() }
        }
    }

    fn poll(&mut self) -> OrderProtocolPoll<PBFTMessage<D::Request>> {
        match self.phase {
            ConsensusPhase::NormalPhase => {
                self.poll_normal_phase()
            }
            ConsensusPhase::SyncPhase => {
                self.poll_sync_phase()
            }
        }
    }

    fn process_message(&mut self, message: StoredMessage<Protocol<PBFTMessage<D::Request>>>) -> Result<OrderProtocolExecResult> {
        match self.phase {
            ConsensusPhase::NormalPhase => {
                self.update_normal_phase(message)
            }
            ConsensusPhase::SyncPhase => {
                self.update_sync_phase(message)
            }
        }
    }

    fn handle_timeout(&mut self, timeout: Vec<ClientRqInfo>) -> Result<OrderProtocolExecResult> {

        let status = self.synchronizer
            .client_requests_timed_out(self.node.id(), &timeout);

        match status {
            SynchronizerStatus::RequestsTimedOut { forwarded, stopped } => {
                if forwarded.len() > 0 {
                    let requests = self.pending_request_log.clone_pending_requests(&forwarded);

                    self.synchronizer.forward_requests(
                        requests,
                        &*self.node,
                        &self.pending_request_log,
                    );
                }

                if stopped.len() > 0 {
                    let stopped = self.pending_request_log.clone_pending_requests(&stopped);

                    self.synchronizer.begin_view_change(Some(stopped),
                                                        &*self.node,
                                                        &self.timeouts,
                                                        &self.message_log);

                    self.switch_phase(ConsensusPhase::SyncPhase)
                }
            }
            // nothing to do
            _ => (),
        }

        Ok(OrderProtocolExecResult::Success)
    }

    fn handle_execution_changed(&mut self, is_executing: bool) -> Result<()> {
        if !is_executing {
            self.consensus_guard.lock_consensus();
        } else {
            match self.phase {
                ConsensusPhase::NormalPhase => {
                    self.consensus_guard.unlock_consensus();
                }
                ConsensusPhase::SyncPhase => {}
            }
        }

        info!("Execution has changed to {} with our current phase being {:?}", is_executing, self.phase);

        Ok(())
    }

    fn handle_forwarded_requests(&mut self, requests: StoredMessage<ForwardedRequestsMessage<D::Request>>) -> Result<()> {
        let (_header, mut requests) = requests.into_inner();

        let init_req_count = requests.requests().len();

        self.pending_request_log.filter_rqs(requests.mut_requests());

        info!("{:?} // Received forwarded requests {:?} from {:?}, after filtering: {:?}", self.node.id(), init_req_count, _header.from(), requests.requests().len());

        if requests.requests().is_empty() {
            return Ok(()); // nothing to do
        }

        self.synchronizer.watch_forwarded_requests(&requests, &self.timeouts);

        self.pending_request_log.insert_forwarded(requests.into_inner());

        Ok(())
    }
}

impl<D, ST, NT> PBFTOrderProtocol<D, ST, NT> where D: SharedData + 'static,
                                                   ST: StateTransferMessage + 'static,
                                                   NT: Node<PBFT<D, ST>> + 'static {
    fn initialize_protocol(config: PBFTConfig<D, ST>, executor: ExecutorHandle<D>,
                           timeouts: Timeouts, node: Arc<NT>, initial_state: Option<Arc<ReadOnly<Checkpoint<D::State>>>>) -> Result<Self> {
        let PBFTConfig {
            node_id,
            follower_handle,
            view, timeout_dur, db_path,
            proposer_config, _phantom_data
        } = config;

        let watermark = 30;

        let sync = Synchronizer::new_replica(view.clone(), timeout_dur);

        let consensus_guard = ProposerConsensusGuard::new(view.clone(), watermark);

        let consensus = Consensus::<D, ST>::new_replica(node_id, &sync.view(), executor.clone(),
                                                        SeqNo::ZERO, watermark, consensus_guard.clone());

        let pending_rq_log = Arc::new(initialize_pending_request_log()?);

        let persistent_log = initialize_persistent_log::<D, String, NoPersistentLog>(executor.clone(), db_path)?;

        let dec_log = initialize_decided_log::<D>(node_id, persistent_log, initial_state)?;

        let proposer = Proposer::<D, NT>::new(node.clone(), sync.clone(),
                                              pending_rq_log.clone(), timeouts.clone(),
                                              executor.clone(), consensus_guard.clone(),
                                              proposer_config);

        let replica = Self {
            phase: ConsensusPhase::NormalPhase,
            consensus,
            synchronizer: sync,
            timeouts,
            consensus_guard,
            unordered_rq_guard: Arc::new(Default::default()),
            executor,
            pending_request_log: pending_rq_log,
            message_log: dec_log,
            proposer,
            node,
        };

        let crr_view = replica.synchronizer.view();

        info!("{:?} // Initialized ordering protocol with view: {:?} and sequence number: {:?}",
              replica.node.id(), crr_view.sequence_number(), replica.sequence_number());

        info!("{:?} // Leader count: {}, Leader set: {:?}", replica.node.id(),
        crr_view.leader_set().len(), crr_view.leader_set());

        replica.proposer.clone().start();

        Ok(replica)
    }

    fn poll_sync_phase(&mut self) -> OrderProtocolPoll<PBFTMessage<D::Request>> {
        // retrieve a view change message to be processed
        match self.synchronizer.poll() {
            SynchronizerPollStatus::Recv => OrderProtocolPoll::ReceiveFromReplicas,
            SynchronizerPollStatus::NextMessage(h, m) => {
                OrderProtocolPoll::Exec(StoredMessage::new(h, Protocol::new(PBFTMessage::ViewChange(m))))
            }
            SynchronizerPollStatus::ResumeViewChange => {
                self.synchronizer.resume_view_change(
                    &mut self.message_log,
                    &self.timeouts,
                    &mut self.consensus,
                    &*self.node,
                );

                self.switch_phase(ConsensusPhase::NormalPhase);

                OrderProtocolPoll::RePoll
            }
        }
    }

    fn poll_normal_phase(&mut self) -> OrderProtocolPoll<PBFTMessage<D::Request>> {
        // check if we have STOP messages to be processed,
        // and update our phase when we start installing
        // the new view
        // Consume them until we reached a point where no new messages can be processed
        while self.synchronizer.can_process_stops() {
            let sync_protocol = self.poll_sync_phase();

            if let OrderProtocolPoll::Exec(message) = sync_protocol {
                let (header, message) = message.into_inner();

                if let PBFTMessage::ViewChange(view_change) = message.into_inner() {
                    let result = self.adv_sync(header, view_change);

                    match result {
                        SyncPhaseRes::RunSyncProtocol => {
                            self.switch_phase(ConsensusPhase::SyncPhase);

                            return OrderProtocolPoll::RePoll;
                        }
                        SyncPhaseRes::RunCSTProtocol => {
                            // We don't need to switch to the sync phase
                            // As that has already been done by the adv sync method
                            return OrderProtocolPoll::RunCst;
                        }
                        _ => {}
                    }
                } else {
                    // The synchronizer should never return anything other than a view
                    // change message
                    unreachable!("Synchronizer returned a message other than a view change message");
                }
            }
        }

        // retrieve the next message to be processed.
        //
        // the order of the next consensus message is guaranteed by
        // `TboQueue`, in the consensus module.
        let polled_message = self.consensus.poll();

        match polled_message {
            ConsensusPollStatus::Recv => OrderProtocolPoll::ReceiveFromReplicas,
            ConsensusPollStatus::NextMessage(h, m) => {
                OrderProtocolPoll::Exec(StoredMessage::new(h, Protocol::new(PBFTMessage::Consensus(m))))
            }
            ConsensusPollStatus::Decided => {
                self.finalize_all_possible().expect("Failed to finalize all possible decided instances");

                OrderProtocolPoll::RePoll
            }
        }
    }

    fn update_sync_phase(&mut self, message: StoredMessage<Protocol<PBFTMessage<D::Request>>>) -> Result<OrderProtocolExecResult> {
        let (header, protocol) = message.into_inner();

        match protocol.into_inner() {
            PBFTMessage::ViewChange(view_change) => {
                return Ok(match self.adv_sync(header, view_change) {
                    SyncPhaseRes::SyncProtocolNotNeeded => {
                        OrderProtocolExecResult::Success
                    }
                    SyncPhaseRes::RunSyncProtocol => {
                        OrderProtocolExecResult::Success
                    }
                    SyncPhaseRes::SyncProtocolFinished => {
                        OrderProtocolExecResult::Success
                    }
                    SyncPhaseRes::RunCSTProtocol => {
                        OrderProtocolExecResult::RunCst
                    }
                });
            }
            PBFTMessage::Consensus(message) => {
                self.consensus.queue(header, message);
            }
            _ => {}
        }

        Ok(OrderProtocolExecResult::Success)
    }

    fn update_normal_phase(&mut self, message: StoredMessage<Protocol<PBFTMessage<D::Request>>>) -> Result<OrderProtocolExecResult> {
        let (header, protocol) = message.into_inner();

        match protocol.into_inner() {
            PBFTMessage::Consensus(message) => {
                return self.adv_consensus(header, message);
            }
            PBFTMessage::ViewChange(view_change) => {
                let status = self.synchronizer.process_message(
                    header,
                    view_change,
                    &self.timeouts,
                    &mut self.message_log,
                    &self.pending_request_log,
                    &mut self.consensus,
                    &*self.node,
                );

                self.synchronizer.signal();

                match status {
                    SynchronizerStatus::Nil => (),
                    SynchronizerStatus::Running => {
                        self.switch_phase(ConsensusPhase::SyncPhase)
                    }
                    // should not happen...
                    _ => {
                        unreachable!()
                    }
                }
            }
            _ => {}
        }

        Ok(OrderProtocolExecResult::Success)
    }

    /// Advance the consensus phase with a received message
    fn adv_consensus(
        &mut self,
        header: Header,
        message: ConsensusMessage<D::Request>,
    ) -> Result<OrderProtocolExecResult> {
        let seq = self.consensus.sequence_number();

        // debug!(
        //     "{:?} // Processing consensus message {:?} ",
        //     self.id(),
        //     message
        // );

        // let start = Instant::now();


        let status = self.consensus.process_message(
            header,
            message,
            &self.synchronizer,
            &self.timeouts,
            &mut self.message_log,
            &*self.node,
        )?;

        match status {
            // if deciding, nothing to do
            ConsensusStatus::Deciding => {}
            // FIXME: implement this
            ConsensusStatus::VotedTwice(_) => todo!(),
            // reached agreement, execute requests
            //
            // FIXME: execution layer needs to receive the id
            // attributed by the consensus layer to each op,
            // to execute in order
            ConsensusStatus::Decided => {
                self.finalize_all_possible()?;

                // When we can no longer finalize then
            }
        }

        //
        // debug!(
        //     "{:?} // Done processing consensus message. Took {:?}",
        //     self.id(),
        //     Instant::now().duration_since(start)
        // );

        Ok(OrderProtocolExecResult::Success)
    }

    /// Finalize all possible consensus instances
    fn finalize_all_possible(&mut self) -> Result<()> {
        let view = self.synchronizer.view();

        while self.consensus.can_finalize() {
            // This will automatically move the consensus machine to the next state
            let completed_batch = self.consensus.finalize(&view)?.unwrap();

            let seq = completed_batch.sequence_number();

            // Clear the requests from the pending request log
            self.pending_request_log.delete_requests_in_batch(&completed_batch);

            // Update the latest ops in the pending request log so we don't repeat requests
            self.pending_request_log.insert_latest_ops_from_batch(&completed_batch);

            //Should the execution be scheduled here or will it be scheduled by the persistent log?
            if let Some(exec_info) =
                self.message_log.finalize_batch(seq, completed_batch)? {
                let (info, batch, completed_batch) = exec_info.into();

                match info {
                    Info::Nil => self.executor.queue_update(batch),
                    // execute and begin local checkpoint
                    Info::BeginCheckpoint => {
                        self.executor.queue_update_and_get_appstate(batch)
                    }
                }.unwrap();
            }
        }

        Ok(())
    }


    /// Advance the sync phase of the algorithm
    fn adv_sync(&mut self, header: Header,
                message: ViewChangeMessage<D::Request>) -> SyncPhaseRes {
        let status = self.synchronizer.process_message(
            header,
            message,
            &self.timeouts,
            &mut self.message_log,
            &self.pending_request_log,
            &mut self.consensus,
            &*self.node,
        );

        self.synchronizer.signal();

        return match status {
            SynchronizerStatus::Nil => SyncPhaseRes::SyncProtocolNotNeeded,
            SynchronizerStatus::Running => SyncPhaseRes::RunSyncProtocol,
            SynchronizerStatus::NewView => {
                //Our current view has been updated and we have no more state operations
                //to perform. This happens if we are a correct replica and therefore do not need
                //To update our state or if we are a replica that was incorrect and whose state has
                //Already been updated from the Cst protocol
                self.switch_phase(ConsensusPhase::NormalPhase);

                SyncPhaseRes::SyncProtocolFinished
            }
            SynchronizerStatus::RunCst => {
                //This happens when a new view is being introduced and we are not up to date
                //With the rest of the replicas. This might happen because the replica was faulty
                //or any other reason that might cause it to lose some updates from the other replicas

                //After we update the state, we go back to the sync phase (this phase) so we can check if we are missing
                //Anything or to finalize and go back to the normal phase
                self.switch_phase(ConsensusPhase::SyncPhase);

                SyncPhaseRes::RunCSTProtocol
            }
            // should not happen...
            _ => {
                unreachable!()
            }
        };
    }
}

impl<D, ST, NT> StatefulOrderProtocol<D, NT> for PBFTOrderProtocol<D, ST, NT>
    where D: SharedData + 'static,
          ST: StateTransferMessage + 'static,
          NT: Node<PBFT<D, ST>> + 'static {
    type StateSerialization = PBFTConsensus<D>;


    fn initialize_with_initial_state(config: Self::Config, executor: ExecutorHandle<D>, timeouts: Timeouts, node: Arc<NT>, initial_state: Arc<ReadOnly<Checkpoint<D::State>>>) -> Result<Self> where Self: Sized {
        Self::initialize_protocol(config, executor, timeouts, node, Some(initial_state))
    }

    fn install_state(&mut self, state: Arc<ReadOnly<Checkpoint<D::State>>>,
                     view_info: View<Self::Serialization>,
                     dec_log: DecLog<Self::StateSerialization>) -> Result<(D::State, Vec<D::Request>)> {
        info!("{:?} // Installing state with Seq No {:?} and View {:?}", self.node.id(),
                state.sequence_number(), view_info);

        let last_exec = if let Some(last_exec) = dec_log.last_execution() {
            last_exec
        } else {
            SeqNo::ZERO
        };

        info!("{:?} // Installing decision log with last execution {:?}", self.node.id(),last_exec);

        self.synchronizer.install_view(view_info.clone());

        let res = self.consensus.install_state(state.state().clone(), view_info, &dec_log)?;

        self.message_log.install_state(state, dec_log);

        Ok(res)
    }

    fn install_seq_no(&mut self, seq_no: SeqNo) -> Result<()> {
        self.consensus.install_sequence_number(seq_no, &self.synchronizer.view());

        Ok(())
    }

    fn snapshot_log(&mut self) -> Result<(Arc<ReadOnly<Checkpoint<D::State>>>, View<Self::Serialization>, DecLog<Self::StateSerialization>)> {
        self.message_log.snapshot(self.synchronizer.view())
    }

    fn finalize_checkpoint(&mut self, checkpoint: Arc<ReadOnly<Checkpoint<D::State>>>) -> Result<()> {
        self.message_log.finalize_checkpoint(checkpoint)
    }
}

impl<D, ST, NT> PBFTOrderProtocol<D, ST, NT>
    where D: SharedData + 'static,
          ST: StateTransferMessage + 'static,
          NT: Node<PBFT<D, ST>> + 'static {
    pub(crate) fn switch_phase(&mut self, new_phase: ConsensusPhase) {
        info!("{:?} // Switching from phase {:?} to phase {:?}", self.node.id(), self.phase, new_phase);

        let old_phase = self.phase.clone();

        self.phase = new_phase;

        //TODO: Handle locking the consensus to the proposer thread
        //When we change to another phase to prevent the proposer thread from
        //Constantly proposing new batches. This is also "fixed" by the fact that the proposer
        //thread never proposes two batches to the same sequence id (this might have to be changed
        //however if it's possible to have various proposals for the same seq number in case of leader
        //Failure or something like that. I think that's impossible though so lets keep it as is)

        if self.phase != old_phase {
            //If the phase is the same, then we got nothing to do as no states have changed

            match (&old_phase, &self.phase) {
                (ConsensusPhase::NormalPhase, _) => {
                    //We want to stop the proposer from trying to propose any requests while we are performing
                    //Other operations.
                    self.consensus_guard.lock_consensus();
                }
                (ConsensusPhase::SyncPhase, ConsensusPhase::NormalPhase) => {}
                (_, _) => {}
            }

            /*
            Observe event stuff
            @{
             */
            let to_send = match (&old_phase, &self.phase) {
                (_, ConsensusPhase::SyncPhase) => ObserveEventKind::ViewChangePhase,
                (_, ConsensusPhase::NormalPhase) => {
                    let current_view = self.synchronizer.view();

                    let current_seq = self.consensus.sequence_number();

                    ObserveEventKind::NormalPhase((current_view, current_seq))
                }
            };

            /*self.observer_handle
                .tx()
                .send(MessageType::Event(to_send))
                .expect("Failed to notify observer thread");
            */
            /*
            }@
            */
        }
    }
}
