// crates/ubm-mobile/src/ordered_public.rs
//! Public session commands for retained ordered groups.
use ubm_desktop::central::OrderedClaimRequest;

pub(crate) enum OrderedCommand {
    Version,
    FailureMetadataVersion,
    CapacityVersion,
    Reserve {
        peer: String,
    },
    Reclaim {
        peer: String,
        id: u64,
        capability: String,
        recording_id: Option<String>,
    },
    Open {
        peer: String,
        id: u64,
        connection: String,
        database: String,
        members: Vec<OrderedClaimRequest>,
        items: usize,
        bytes: usize,
        controls: usize,
        context: serde_json::Value,
    },
    IncreaseCapacity {
        peer: String,
        id: u64,
        generation: ubm_core::ordered_notifications::SourceGeneration,
        max_bytes: usize,
    },
    Group {
        action: GroupAction,
        peer: String,
        id: u64,
        delivery: Option<u64>,
        capability: Option<String>,
    },
}

pub(crate) enum GroupAction {
    Status,
    Poll,
    AckDelivery,
    Discard,
    PollDiscard,
    AckDiscard,
    Close,
    AckGroup,
    ForgetGroup,
}
