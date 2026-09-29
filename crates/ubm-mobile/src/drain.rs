//! Shared bounded native outbox; mobile and desktop use identical cutoff,
//! accounting and FIFO rules.
pub use ubm_desktop::continuation_outbox::{
    AfterCutoffLoss, CONTROL_RECORD_CAP, DATA_RECORD_BYTES, DATA_RECORD_CAP, DataIngressFailure,
    Outbox,
};
