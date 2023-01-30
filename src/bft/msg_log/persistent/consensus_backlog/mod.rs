use std::collections::BTreeMap;

use log::{error, warn};

use crate::bft::{
    benchmarks::BatchMeta,
    communication::{
        channel::{self, ChannelSyncRx, ChannelSyncTx},
    },
    crypto::hash::Digest,
    executable::{ExecutorHandle, Request, Service, UpdateBatch},
    ordering::{Orderable, SeqNo},
};

use crate::bft::error::*;
use crate::bft::msg_log::decided_log::BatchExecutionInfo;
use crate::bft::msg_log::Info;

use super::{ResponseMessage, ResponseMsg};

struct AwaitingPersistence<S: Service> {

    info: BatchExecutionInfo<S>,
    pending_requests: Vec<Digest>

}

///This is made to handle the backlog when the consensus is working faster than the persistent storage layer.
/// It holds update batches that are yet to be executed since they are still waiting for the confirmation of the persistent log
/// This is only needed (and only instantiated) when the persistency mode is strict
pub struct ConsensusBacklog<S: Service> {
    rx: ChannelSyncRx<BatchExecutionInfo<S>>,

    //Receives messages from the persistent log
    logger_rx: ChannelSyncRx<ResponseMsg>,

    //The handle to the executor
    executor_handle: ExecutorHandle<S>,

    //This is the batch that is currently waiting for it's messages to be persisted
    //Even if we already persisted the consensus instance that came after it (for some reason)
    // We can only deliver it when all the previous ones have been delivered,
    // As it must be ordered
    currently_waiting_for: Option<AwaitingPersistence<S>>,

    //Message confirmations that we have already received but pertain to a further ahead consensus instance
    messages_received_ahead: BTreeMap<SeqNo, Vec<Digest>>,
}

///A detachable handle so we deliver work to the
/// consensus back log thread
pub struct ConsensusBackLogHandle<S: Service> {
    rq_tx: ChannelSyncTx<BatchExecutionInfo<S>>,
    logger_tx: ChannelSyncTx<ResponseMsg>,
}

impl<S: Service> ConsensusBackLogHandle<S> {

    pub fn logger_tx(&self) -> ChannelSyncTx<ResponseMsg> {
        self.logger_tx.clone()
    }

    pub fn queue_batch(&self, batch: PendingBatch<S>) -> Result<()> {
        if let Err(err) =  self.rq_tx.send(batch) {
            Err(Error::simple_with_msg(ErrorKind::MsgLogPersistent, format!("{:?}", err).as_str()))
        } else {
            Ok(())
        }
    }

}

impl<S: Service> Clone for ConsensusBackLogHandle<S> {
    fn clone(&self) -> Self {
        Self {
            rq_tx: self.rq_tx.clone(),
            logger_tx: self.logger_tx.clone()
        }
    }
}

///This channel size serves as the "buffer" for the amount of consensus instances
///That can be waiting for messages
const CHANNEL_SIZE: usize = 1024;

impl<S: Service + 'static> ConsensusBacklog<S> {
    ///Initialize the consensus backlog
    pub fn init_backlog(executor: ExecutorHandle<S>) -> ConsensusBackLogHandle<S> {
        let (logger_tx, logger_rx) = channel::new_bounded_sync(CHANNEL_SIZE);

        let (batch_tx, batch_rx) = channel::new_bounded_sync(CHANNEL_SIZE);

        let backlog_thread = ConsensusBacklog {
            rx: batch_rx,
            logger_rx,
            executor_handle: executor,
            currently_waiting_for: None,
            messages_received_ahead: BTreeMap::new(),
        };

        backlog_thread.start_thread();

        let handle = ConsensusBackLogHandle {
            rq_tx: batch_tx,
            logger_tx,
        };

        handle
    }

    fn start_thread(self) {
        std::thread::Builder::new()
            .name(format!("Consensus Backlog thread"))
            .spawn(move || {
                self.run();
            })
            .expect("Failed to start consensus backlog thread.");
    }

    fn run(mut self) {
        loop {
            if let Some(info) = &mut self.currently_waiting_for {
                let notification = match self.logger_rx.recv() {
                    Ok(notification) => notification,
                    Err(_) => break,
                };

                let curr_seq = info.info().update_batch().sequence_number();

                match notification {
                    ResponseMessage::WroteMessage(seq, digest) => {
                        if curr_seq == seq {
                            if !Self::remove_from_vec(&mut info.pending_requests, &digest) {
                                warn!("Received message for consensus instance {:?} but was not expecting it?",curr_seq);
                                continue;
                            }
                        } else {
                            if let Some(received_msg) = self.messages_received_ahead.get_mut(&seq) {
                                received_msg.push(digest);
                            } else {
                                self.messages_received_ahead.insert(seq, vec![digest]);
                            }
                        }
                    }
                    _ => {
                        //TODO: Well, what do we want to do with this?
                    }
                }

                if info.pending_requests().is_empty() {
                    let finished_batch = self.currently_waiting_for.take().unwrap();

                    self.dispatch_batch(finished_batch.into());
                }
            } else {
                let batch_info = match self.rx.recv() {
                    Ok(rcved) => rcved,
                    Err(err) => {
                        error!("{:?}", err);

                        break;
                    }
                };

                let mut awaiting = AwaitingPersistence::from(batch_info);

                let seq_num = awaiting.info().update_batch().sequence_number();

                //Remove the messages that we have already received
                let messages_ahead = self.messages_received_ahead.remove(&seq_num);

                if let Some(messages_ahead) = messages_ahead {
                    for persisted_message in messages_ahead {
                        if !Self::remove_from_vec(&mut awaiting.pending_requests, &persisted_message) {
                            warn!("Received message for consensus instance {:?} but was not expecting it?",
                            seq_num);
                        }
                    }
                }

                if awaiting.pending_requests().is_empty() {

                    //If we have already received everything, dispatch the batch immediately
                    self.dispatch_batch(awaiting.into());

                    continue;
                }

                self.currently_waiting_for = Some(awaiting);
            }
        }
    }

    fn dispatch_batch(&self, batch: BatchExecutionInfo<S>) {
        let (info, requests, batch) = batch.into();
        let checkpoint = match info {
            Info::Nil => self.executor_handle.queue_update(meta, requests),
            Info::BeginCheckpoint => self
                .executor_handle
                .queue_update_and_get_appstate(meta, requests),
        };

        if let Err(err) = checkpoint {
            error!("Failed to enqueue consensus {:?}", err);
        }
    }

    fn remove_from_vec(missing_msgs: &mut Vec<Digest>, persisted_message: &Digest) -> bool {
        match missing_msgs.iter().position(|p| *p == *persisted_message) {
            Some(msg_index) => {
                missing_msgs.swap_remove(msg_index);

                true
            }
            None => false,
        }
    }
}

impl<S> From<BatchExecutionInfo<S>> for AwaitingPersistence<S> where S: Service
{
    fn from(value: BatchExecutionInfo<S>) -> Self {
        AwaitingPersistence {
            info: value,
            pending_requests: value.completed_batch().messages_to_persist().clone(),
        }
    }
}

impl<S> Into<BatchExecutionInfo<S>>  for AwaitingPersistence<S> where S: Service{
    fn into(self) -> BatchExecutionInfo<S> {
        self.info
    }
}

impl<S> AwaitingPersistence<S> where S: Service {
    pub fn info(&self) -> &BatchExecutionInfo<S> {
        &self.info
    }
    pub fn pending_requests(&self) -> &Vec<Digest> {
        &self.pending_requests
    }
}
