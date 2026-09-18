//! The core facts the mobile wire reads from `ubm-desktop` / `ubm-core`,
//! each in one place.

use ubm_desktop::{LifecycleEvent, PeerRecord, ResourceCounters};

/// Database generation the lifecycle transition applied to (read by the
/// core before the transition).
#[must_use]
pub fn lifecycle_database_generation(event: &LifecycleEvent) -> Option<String> {
    event.database_generation.clone()
}

/// Current database generation of one peer.
#[must_use]
pub fn peer_database_generation(record: &PeerRecord) -> Option<String> {
    record.database_generation.clone()
}

/// Queued and dispatched core operations.
#[must_use]
pub fn operation_split(counters: &ResourceCounters) -> Option<(u64, u64)> {
    Some((
        counters.core.queued_operations as u64,
        counters.core.dispatched_operations as u64,
    ))
}
