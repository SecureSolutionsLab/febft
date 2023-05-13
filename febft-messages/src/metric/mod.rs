use febft_metrics::MetricRegistry;
use febft_metrics::metrics::MetricKind;

/// Core frameworks will get 0XX metric ID

/// Request pre processing (000-X09)
pub const RQ_PP_CLIENT_MSG: &str = "RQ_PRE_PROCESSING_CLIENT_MSGS";
pub const RQ_PP_CLIENT_MSG_ID: usize = 000;

pub const RQ_PP_CLIENT_COUNT: &str = "RQ_PRE_PROCESSING_CLIENT_COUNT";
pub const RQ_PP_CLIENT_COUNT_ID: usize = 001;

pub const RQ_PP_FWD_RQS: &str = "RQ_PRE_PROCESSING_FWD_RQS";
pub const RQ_PP_FWD_RQS_ID: usize = 002;

pub const RQ_PP_DECIDED_RQS: &str = "RQ_PRE_PROCESSING_DECIDED_RQS";
pub const RQ_PP_DECIDED_RQS_ID: usize = 003;

pub const RQ_PP_TIMEOUT_RQS: &str = "RQ_PRE_PROCESSING_TIMEOUT_RQS";
pub const RQ_PP_TIMEOUT_RQS_ID: usize = 004;

pub const RQ_PP_COLLECT_PENDING: &str = "RQ_PRE_PROCESSING_COLLECT_PENDING";
pub const RQ_PP_COLLECT_PENDING_ID: usize = 005;

pub const RQ_PP_CLONE_RQS: &str = "RQ_PRE_PROCESSING_CLONE_RQS";
pub const RQ_PP_CLONE_RQS_ID: usize = 006;

pub fn metrics() -> Vec<MetricRegistry> {
    vec![
        (RQ_PP_CLIENT_MSG_ID, RQ_PP_CLIENT_MSG.to_string(), MetricKind::Duration),
        (RQ_PP_CLIENT_COUNT_ID, RQ_PP_CLIENT_COUNT.to_string(), MetricKind::Counter),
        (RQ_PP_FWD_RQS_ID, RQ_PP_FWD_RQS.to_string(), MetricKind::Duration),
        (RQ_PP_DECIDED_RQS_ID, RQ_PP_DECIDED_RQS.to_string(), MetricKind::Duration),
        (RQ_PP_TIMEOUT_RQS_ID, RQ_PP_TIMEOUT_RQS.to_string(), MetricKind::Duration),
        (RQ_PP_COLLECT_PENDING_ID, RQ_PP_COLLECT_PENDING.to_string(), MetricKind::Duration),
        (RQ_PP_CLONE_RQS_ID, RQ_PP_CLONE_RQS.to_string(), MetricKind::Duration),
    ]
}