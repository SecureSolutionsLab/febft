use febft_execution::serialize::SharedData;
use febft_common::error::*;
use febft_messages::state_transfer::StatefulOrderProtocol;
use crate::message::CstMessage;

fn serialize_state_transfer<D, SOP, NT>(mut state_transfer: febft_capnp::cst_messages_capnp::cst_message::Builder,
                                    msg: &CstMessage<D::State, SOP::DecLog, SOP::ViewInfo>) -> Result<()>
    where D: SharedData, SOP: StatefulOrderProtocol<D, NT> {
    Ok(())
}

fn deserialize_state_transfer<D, SOP, NT>(state_transfer: febft_capnp::cst_messages_capnp::cst_message::Reader)
                                      -> Result<CstMessage<D::State, SOP::DecLog, SOP::ViewInfo>>
    where D: SharedData, SOP: StatefulOrderProtocol<D, NT> {
    Err(Error::simple(ErrorKind::CommunicationSerialize))
}
