// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

use crate::api::ValueNotification;
#[cfg(any(not(target_os = "linux"), test))]
use futures::future::ready;
#[cfg(any(not(target_os = "linux"), test))]
use futures::stream::{Stream, StreamExt};
#[cfg(any(not(target_os = "linux"), test))]
use std::pin::Pin;
#[cfg(any(not(target_os = "linux"), test))]
use tokio::sync::broadcast::Receiver;
#[cfg(any(not(target_os = "linux"), test))]
use tokio_stream::wrappers::BroadcastStream;
#[cfg(any(not(target_os = "linux"), test))]
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// UBM patch (UBM_PATCHES.md #10): a receiver that fell behind the bounded
/// broadcast learns how many notifications it missed on the next one it
/// gets (`ValueNotification::lost_before`). Upstream filtered the lag out.
#[cfg(any(not(target_os = "linux"), test))]
pub fn notifications_stream_from_broadcast_receiver(
    receiver: Receiver<ValueNotification>,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>> {
    Box::pin(
        BroadcastStream::new(receiver)
            .scan(0u64, |missed, item| {
                ready(Some(match item {
                    Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                        *missed = missed.saturating_add(skipped);
                        None
                    }
                    Ok(mut notification) => {
                        notification.lost_before = notification
                            .lost_before
                            .saturating_add(std::mem::take(missed));
                        Some(notification)
                    }
                }))
            })
            .filter_map(ready),
    )
}

/// Reports each broadcast gap independently of the following value or EOF.
#[cfg(any(not(target_os = "linux"), test))]
pub fn notification_events_from_broadcast_receiver(
    receiver: Receiver<ValueNotification>,
) -> Pin<Box<dyn Stream<Item = crate::api::NotificationEvent> + Send>> {
    use crate::api::NotificationEvent;
    Box::pin(BroadcastStream::new(receiver).map(|item| match item {
        Ok(value) => NotificationEvent::Value(value),
        Err(BroadcastStreamRecvError::Lagged(skipped_records)) => {
            NotificationEvent::Lagged { skipped_records }
        }
    }))
}

/// Private native ordering envelope: the public notification contract is unchanged.
#[cfg(any(not(target_os = "linux"), test))]
#[derive(Clone, Debug)]
pub struct NotificationEnvelope {
    sequence: u64,
    delivery_index: u128,
    // Broadcast clones and replay queues retain the same payload. A public
    // ValueNotification is copied only when an item is actually delivered.
    notification: std::sync::Arc<ValueNotification>,
    // A queued delivery overflow belongs to the receivers that observed this
    // sequence even after its graph's retained-fault table is cleared.
    broadcast_overflow: bool,
}

type NotificationKey = (uuid::Uuid, u64, uuid::Uuid, u64);

#[derive(Default)]
struct FaultState {
    #[cfg(any(not(target_os = "linux"), test))]
    sequence: u64,
    #[cfg(any(not(target_os = "linux"), test))]
    sequence_exhausted: bool,
    #[cfg(any(not(target_os = "linux"), test))]
    delivery_index: u128,
    #[cfg(any(not(target_os = "linux"), test))]
    in_flight_delivery: Option<(u128, u64)>,
    #[cfg(any(not(target_os = "linux"), test))]
    faults: std::collections::BTreeMap<NotificationKey, NotificationEnvelope>,
    #[cfg(any(not(target_os = "linux"), test))]
    quota: Option<NotificationEnvelope>,
    #[cfg(any(not(target_os = "linux"), test))]
    shared_broadcast_terminal: Option<std::sync::Arc<NotificationEnvelope>>,
    #[cfg(any(not(target_os = "linux"), test))]
    broadcast_receivers: Vec<std::sync::Weak<std::sync::Mutex<BroadcastTerminal>>>,
    enable_epochs: std::collections::BTreeMap<NotificationKey, u64>,
    direct_faults: std::collections::BTreeMap<NotificationKey, ValueNotification>,
    held_direct_faults:
        std::collections::BTreeMap<NotificationKey, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    direct_faults_exhausted: bool,
    source_activations: std::collections::BTreeSet<(NotificationKey, u64)>,
    activation_faults: std::collections::BTreeMap<(NotificationKey, u64), ValueNotification>,
    held_activation_faults: std::collections::BTreeMap<
        (NotificationKey, u64),
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    >,
    source_generation: Option<u64>,
    pending_source_failure: bool,
    // Includes owners of generations that have since been replaced.
    pending_failure_owners: usize,
    future_failure_owners: std::collections::BTreeMap<u64, source_groups::FutureFailureSlots>,
    next_future_failure_owner: u64,
    next_source_ordinal: Option<u64>,
    last_source_generation: Option<u64>,
    // Allocated before the native producer is enabled. It follows the
    // physical generation so a terminal cut transfers the exact admission to
    // the desktop ACK path.
    terminal_evidence: Option<(
        std::sync::Arc<dyn crate::ubm_peer_notification_ingress::TerminalEvidencePort>,
        String,
    )>,
    next_group: u64,
    groups: std::collections::BTreeMap<u64, SourceGroup>,
}

#[cfg(any(not(target_os = "linux"), test))]
const OVERFLOW_RECEIVER_CAPACITY: usize = 16;

// The original metadata is an ordered suffix, including duplicate and
// colliding keys. The explicit range makes it distinguishable from wrapper
// fields without changing PlatformError's static metadata key type.
#[cfg(any(not(target_os = "linux"), test))]
fn with_original_error(
    mut wrapper: crate::PlatformError,
    original: crate::PlatformError,
) -> crate::PlatformError {
    wrapper = wrapper
        .with("originalDomain", original.domain)
        .with("originalCode", original.code)
        .with("originalMessage", original.message);
    let start = wrapper.metadata.len() + 2;
    wrapper = wrapper
        .with("originalMetadataStart", start.to_string())
        .with("originalMetadataCount", original.metadata.len().to_string());
    wrapper.metadata.extend(original.metadata);
    wrapper
}

#[cfg(any(not(target_os = "linux"), test))]
struct PendingOverflow {
    envelope: std::sync::Arc<NotificationEnvelope>,
    // Last reserved delivery at or before this sequence; an unsent
    // replacement sits immediately after this boundary.
    delivery_end: u128,
    sent: bool,
}

#[cfg(any(not(target_os = "linux"), test))]
struct OmittedOverflows {
    first: NotificationEnvelope,
    last_sequence: u64,
    count: u64,
    delivery_end: u128,
    sent: bool,
}

#[cfg(any(not(target_os = "linux"), test))]
#[derive(Default)]
struct BroadcastTerminal {
    pending: std::collections::VecDeque<PendingOverflow>,
    omitted: Option<OmittedOverflows>,
    omitted_through: u64,
    floor: u64,
    // FIFO delivery makes the pre-floor positions contiguous, even when a
    // queued terminal is replaced. Include a possibly in-flight send: if it
    // completed before subscribe, its position is outside the receiver's gap.
    old_deliveries: Option<(u128, u128)>,
}

#[cfg(any(not(target_os = "linux"), test))]
impl BroadcastTerminal {
    fn retain_overflow(
        &mut self,
        envelope: std::sync::Arc<NotificationEnvelope>,
        delivery_end: u128,
    ) {
        if self.pending.len() < OVERFLOW_RECEIVER_CAPACITY && self.omitted.is_none() {
            self.pending.push_back(PendingOverflow {
                envelope,
                delivery_end,
                sent: false,
            });
        } else if let Some(omitted) = &mut self.omitted {
            // Keep aggregating until the receipt is actually delivered, even
            // if a consumer has already polled a preceding lag event.
            omitted.count = omitted.count.saturating_add(1);
            omitted.last_sequence = envelope.sequence;
        } else {
            let mut first = (*envelope).clone();
            let note = &*first.notification;
            first.notification = std::sync::Arc::new(ValueNotification {
                uuid: note.uuid,
                instance: note.instance,
                service_uuid: note.service_uuid,
                service_instance: note.service_instance,
                value: Vec::new(),
                source_failure: None,
                lost_before: 0,
            });
            self.omitted = Some(OmittedOverflows {
                first,
                last_sequence: envelope.sequence,
                count: 1,
                delivery_end,
                sent: false,
            });
        }
    }

    fn retained(&self, last: u64, upper: Option<u64>) -> Vec<NotificationEnvelope> {
        let in_range = |sequence| sequence > last && upper.is_none_or(|upper| sequence <= upper);
        let mut entries: Vec<_> = self
            .pending
            .iter()
            .filter(|entry| in_range(entry.envelope.sequence))
            .map(|entry| (*entry.envelope).clone())
            .collect();
        if let Some(omitted) = self
            .omitted
            .as_ref()
            .filter(|entry| in_range(entry.first.sequence))
        {
            entries.push(omitted.first.clone());
        }
        entries
    }

    fn position(&self, sequence: u64) -> Option<(u128, bool)> {
        self.pending
            .iter()
            .find(|entry| entry.envelope.sequence == sequence)
            .map(|entry| (entry.delivery_end, entry.sent))
            .or_else(|| {
                self.omitted
                    .as_ref()
                    .filter(|entry| entry.first.sequence == sequence)
                    .map(|entry| (entry.delivery_end, entry.sent))
            })
    }

    fn delivered(&mut self, mut envelope: NotificationEnvelope) -> NotificationEnvelope {
        if self
            .pending
            .front()
            .is_some_and(|entry| entry.envelope.sequence == envelope.sequence)
        {
            self.pending.pop_front();
        } else if self
            .omitted
            .as_ref()
            .is_some_and(|entry| entry.first.sequence == envelope.sequence)
        {
            let omitted = self.omitted.take().unwrap();
            self.omitted_through = omitted.last_sequence;
            let note = std::sync::Arc::make_mut(&mut envelope.notification);
            note.source_failure = Some(
                crate::PlatformError::new(
                    "ubm-native",
                    "notification-overflow-evidence-omitted",
                    "Bounded receiver overflow evidence capacity exhausted",
                )
                .with("omittedCount", omitted.count.to_string())
                .with("tailUncertain", "true"),
            );
        }
        envelope
    }
}

#[cfg(any(not(target_os = "linux"), test))]
fn register_broadcast_receiver(
    state: &mut FaultState,
) -> std::sync::Arc<std::sync::Mutex<BroadcastTerminal>> {
    let mut terminal = BroadcastTerminal {
        floor: state.sequence,
        // A send may be between reserving its position and calling send().
        // If it precedes subscribe(), its position falls outside any lag range.
        old_deliveries: state.in_flight_delivery.map(|(index, _)| (index, index)),
        ..BroadcastTerminal::default()
    };
    if let Some(entry) = &state.shared_broadcast_terminal {
        terminal.retain_overflow(entry.clone(), state.delivery_index);
    }
    let receiver = std::sync::Arc::new(std::sync::Mutex::new(terminal));
    state
        .broadcast_receivers
        .retain(|receiver| receiver.strong_count() != 0);
    state
        .broadcast_receivers
        .push(std::sync::Arc::downgrade(&receiver));
    receiver
}

/// Intersect a broadcast gap with this receiver's lifetime, excluding delayed
/// pre-subscription deliveries. A retained overflow divides the gap into
/// generations, but does not end the stream or erase later loss.
#[cfg(any(not(target_os = "linux"), test))]
fn subscription_lag(
    terminal: &std::sync::Mutex<BroadcastTerminal>,
    skipped: u64,
    envelope: &NotificationEnvelope,
) -> u64 {
    let terminal = terminal.lock().unwrap();
    let start = envelope.delivery_index.saturating_sub(u128::from(skipped));
    let end = envelope.delivery_index;
    let count = end.saturating_sub(start);
    let old = terminal.old_deliveries.map_or(0, |(first, last)| {
        end.min(last.saturating_add(1))
            .saturating_sub(start.max(first))
    });
    u64::try_from(count.saturating_sub(old)).unwrap_or(u64::MAX)
}

#[cfg(any(not(target_os = "linux"), test))]
fn subscription_lag_between(terminal: &BroadcastTerminal, start: u128, end: u128) -> u64 {
    let count = end.saturating_sub(start);
    let old = terminal.old_deliveries.map_or(0, |(first, last)| {
        end.min(last.saturating_add(1))
            .saturating_sub(start.max(first))
    });
    u64::try_from(count.saturating_sub(old)).unwrap_or(u64::MAX)
}

#[path = "util_source_groups.rs"]
mod source_groups;
#[cfg(target_os = "windows")]
pub(crate) use source_groups::FutureFailureCut;
#[cfg(target_os = "windows")]
pub(crate) use source_groups::RetriableFutureFailureCut;
use source_groups::SourceGroup;
#[cfg(target_os = "windows")]
pub(crate) use source_groups::SourceRouteRetirement;
pub(crate) use source_groups::SourceValueReservation;
pub use source_groups::{
    SOURCE_MARKER_MAX_REQUEST_BYTES, SOURCE_MARKER_MAX_WRITE_BYTES, SourceGroupConsumer,
    SourceGroupError, SourceGroupHandle, SourceUnresolvedWriteRequest, SourceWriteReceipt,
    SourceWriteRequest, SourceWriteSelector, SourceWriteStart, SourceWriteStatus,
    marker_graph_identity,
};

/// Terminal callback evidence has a separate bounded lifetime from lossy data.
/// Only confirmed native disable or disconnect releases it.
pub struct NotificationFaults {
    state: std::sync::Mutex<FaultState>,
    capacity: usize,
    /// Optional host-selected durable evidence owner. Absence is explicit: a
    /// backend may not silently select a filesystem path for raw errors.
    failure_evidence_store:
        Option<std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    terminal_evidence_port:
        Option<std::sync::Arc<dyn crate::ubm_peer_notification_ingress::TerminalEvidencePort>>,
    owner: std::sync::Arc<()>,
    wake_coordinator: std::sync::Arc<crate::ubm_peer_notification_ingress::WakeCoordinator>,
    #[cfg(any(not(target_os = "linux"), test))]
    broadcast_delivery: std::sync::Mutex<BroadcastDelivery>,
}

#[cfg(any(not(target_os = "linux"), test))]
#[derive(Default)]
struct BroadcastDelivery {
    queue: std::collections::VecDeque<(
        tokio::sync::broadcast::Sender<NotificationEnvelope>,
        NotificationEnvelope,
    )>,
    terminal: Option<(
        tokio::sync::broadcast::Sender<NotificationEnvelope>,
        NotificationEnvelope,
    )>,
    draining: bool,
    holds: usize,
}

struct SourceStateGuard<'a> {
    state: Option<std::sync::MutexGuard<'a, FaultState>>,
    wake_scope: Option<SourceWakeScope>,
}

pub(crate) struct SourceWakeScope(
    std::sync::Arc<crate::ubm_peer_notification_ingress::WakeCoordinator>,
);
impl Drop for SourceWakeScope {
    fn drop(&mut self) {
        self.0.release();
    }
}
/// Keeps coalesced wakes in the bounded owner queue while an async lifecycle
/// operation owns the connection; it does not hold source admission locks.
pub(crate) struct SourceWakeDeliveryHold(
    std::sync::Arc<crate::ubm_peer_notification_ingress::WakeCoordinator>,
);
impl Drop for SourceWakeDeliveryHold {
    fn drop(&mut self) {
        self.0.release_delivery();
    }
}

/// Defers both delivery paths, without retaining a source admission lock.
#[cfg(any(
    target_os = "windows",
    test,
    all(feature = "reclaim-test-support", target_vendor = "apple")
))]
pub(crate) struct NotificationDeliveryHold<'a> {
    faults: &'a NotificationFaults,
    wakes: Option<SourceWakeDeliveryHold>,
}
#[cfg(any(
    target_os = "windows",
    test,
    all(feature = "reclaim-test-support", target_vendor = "apple")
))]
impl Drop for NotificationDeliveryHold<'_> {
    fn drop(&mut self) {
        let drain = {
            let mut delivery = self.faults.broadcast_delivery.lock().unwrap();
            delivery.holds -= 1;
            if delivery.holds == 0
                && !delivery.draining
                && (!delivery.queue.is_empty() || delivery.terminal.is_some())
            {
                delivery.draining = true;
                true
            } else {
                false
            }
        };
        drop(self.wakes.take());
        if drain {
            self.faults.drain_broadcast();
        }
    }
}

impl std::ops::Deref for SourceStateGuard<'_> {
    type Target = FaultState;
    fn deref(&self) -> &Self::Target {
        self.state.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for SourceStateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state.as_mut().unwrap()
    }
}
impl Drop for SourceStateGuard<'_> {
    fn drop(&mut self) {
        drop(self.state.take());
        drop(self.wake_scope.take());
    }
}

impl NotificationFaults {
    #[cfg(all(test, target_vendor = "apple"))]
    pub(crate) fn with_publication_lock_for_test<T>(&self, f: impl FnOnce() -> T) -> T {
        let _state = self.state.lock().unwrap();
        f()
    }

    fn lock_source_state(&self) -> SourceStateGuard<'_> {
        let wake_scope = self.defer_wakes();
        let state = self.state.lock().unwrap();
        SourceStateGuard {
            state: Some(state),
            wake_scope: Some(wake_scope),
        }
    }
    pub(crate) fn defer_wakes(&self) -> SourceWakeScope {
        self.wake_coordinator.defer();
        SourceWakeScope(self.wake_coordinator.clone())
    }
    pub(crate) fn hold_wake_delivery(&self) -> SourceWakeDeliveryHold {
        self.wake_coordinator.hold_delivery();
        SourceWakeDeliveryHold(self.wake_coordinator.clone())
    }
    #[cfg(any(
        target_os = "windows",
        test,
        all(feature = "reclaim-test-support", target_vendor = "apple")
    ))]
    pub(crate) fn hold_notification_delivery(&self) -> NotificationDeliveryHold<'_> {
        let wakes = self.hold_wake_delivery();
        self.broadcast_delivery.lock().unwrap().holds += 1;
        NotificationDeliveryHold {
            faults: self,
            wakes: Some(wakes),
        }
    }
    pub fn new(capacity: usize) -> Self {
        Self::new_with_failure_evidence_store(capacity, None)
    }

    /// Construct with an explicit host-owned failure-evidence store. The
    /// source uses it only for retained recovery evidence; ordinary values and
    /// callback transport remain in memory-bound ingress queues.
    pub fn new_with_failure_evidence_store(
        capacity: usize,
        failure_evidence_store: Option<
            std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
        >,
    ) -> Self {
        Self::new_with_terminal_evidence_port(capacity, failure_evidence_store, None)
    }

    /// Construct with a host-owned terminal-evidence port. The port is fixed
    /// before the source can be shared, so a live generation cannot switch
    /// durable ownership.
    pub fn new_with_terminal_evidence_port(
        capacity: usize,
        failure_evidence_store: Option<
            std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>,
        >,
        terminal_evidence_port: Option<
            std::sync::Arc<dyn crate::ubm_peer_notification_ingress::TerminalEvidencePort>,
        >,
    ) -> Self {
        Self {
            state: std::sync::Mutex::new(FaultState::default()),
            capacity,
            failure_evidence_store,
            terminal_evidence_port,
            owner: std::sync::Arc::new(()),
            wake_coordinator: std::sync::Arc::new(
                crate::ubm_peer_notification_ingress::WakeCoordinator::new(capacity),
            ),
            #[cfg(any(not(target_os = "linux"), test))]
            broadcast_delivery: std::sync::Mutex::new(BroadcastDelivery::default()),
        }
    }

    /// Trusted desktop host installs this before any source activation.  A
    /// second installation is refused so a live generation cannot silently
    /// switch durable ownership.
    pub fn install_terminal_evidence_port(
        &mut self,
        port: std::sync::Arc<dyn crate::ubm_peer_notification_ingress::TerminalEvidencePort>,
    ) -> Result<(), crate::ubm_peer_notification_ingress::IngressError> {
        if self.terminal_evidence_port.is_some() {
            return Err(crate::ubm_peer_notification_ingress::IngressError::NotCurrent);
        }
        self.terminal_evidence_port = Some(port);
        Ok(())
    }

    pub(crate) fn failure_evidence_store(
        &self,
    ) -> Option<&std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>
    {
        self.failure_evidence_store.as_ref()
    }

    fn key(note: &ValueNotification) -> NotificationKey {
        (
            note.service_uuid,
            note.service_instance,
            note.uuid,
            note.instance,
        )
    }

    #[cfg(any(
        test,
        target_vendor = "apple",
        all(feature = "reclaim-test-support", target_os = "windows")
    ))]
    pub fn publish(
        &self,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        notification: ValueNotification,
    ) {
        let mut state = self.lock_source_state();
        let drain = self.publish_locked(&mut state, sender, notification);
        drop(state);
        if drain {
            self.drain_broadcast();
        }
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn publish_locked(
        &self,
        state: &mut FaultState,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        mut notification: ValueNotification,
    ) -> bool {
        if state.quota.is_some() {
            return false;
        }
        let Some(sequence) = state.sequence.checked_add(1) else {
            return false;
        };
        let full = notification.source_failure.is_some()
            && !state.faults.contains_key(&Self::key(&notification))
            && state.faults.len() >= self.capacity;
        if full || sequence == u64::MAX {
            let original = notification.source_failure.take();
            let mut error = crate::PlatformError::new(
                "ubm-native",
                "notification-fault-quota",
                "Bounded native notification terminal evidence capacity exhausted",
            );
            if sequence == u64::MAX {
                error = error
                    .with("sequenceExhausted", "true")
                    .with("rejectedBytes", notification.value.len().to_string())
                    .with("tailUncertain", "true");
            }
            if let Some(original) = original {
                error = with_original_error(error, original);
            }
            notification.value.clear();
            notification.source_failure = Some(error);
            let envelope = NotificationEnvelope {
                sequence,
                delivery_index: 0,
                notification: std::sync::Arc::new(notification),
                broadcast_overflow: false,
            };
            state.sequence = sequence;
            state.quota = Some(envelope.clone());
            if sequence == u64::MAX {
                // The final identity is a permanent peer-wide terminal. It
                // must not enter the bounded overflow summary: that could
                // hide exhaustion from both existing and future receivers.
                state.sequence_exhausted = true;
                let mut delivery = self.broadcast_delivery.lock().unwrap();
                if delivery.queue.len() >= self.capacity || delivery.terminal.is_some() {
                    delivery.terminal = Some((sender.clone(), envelope));
                } else {
                    delivery.queue.push_back((sender.clone(), envelope));
                }
                return if delivery.draining || delivery.holds > 0 {
                    false
                } else {
                    delivery.draining = true;
                    true
                };
            }
            return self.enqueue_broadcast(state, sender, envelope);
        }
        state.sequence = sequence;
        let envelope = NotificationEnvelope {
            sequence,
            delivery_index: 0,
            notification: std::sync::Arc::new(notification),
            broadcast_overflow: false,
        };
        if envelope.notification.source_failure.is_some() {
            state
                .faults
                .entry(Self::key(&envelope.notification))
                .or_insert_with(|| envelope.clone());
        }
        self.enqueue_broadcast(state, sender, envelope)
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn enqueue_broadcast(
        &self,
        state: &mut FaultState,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        mut envelope: NotificationEnvelope,
    ) -> bool {
        let mut delivery = self.broadcast_delivery.lock().unwrap();
        if delivery.queue.len() >= self.capacity || delivery.terminal.is_some() {
            // A fault reserved above was rejected by this queue: its evidence
            // belongs in the overflow terminal, not the accepted fault table.
            let key = Self::key(&envelope.notification);
            if state
                .faults
                .get(&key)
                .is_some_and(|fault| fault.sequence == envelope.sequence)
            {
                state.faults.remove(&key);
            }
            let rejected_bytes = envelope.notification.value.len();
            let original = std::sync::Arc::make_mut(&mut envelope.notification)
                .source_failure
                .take();
            let mut failure = crate::PlatformError::new(
                "ubm-native",
                "notification-broadcast-overflow",
                "Bounded native broadcast delivery overflowed",
            )
            .with("rejectedBytes", rejected_bytes.to_string())
            .with("tailUncertain", "true");
            if let Some(original) = original {
                failure = with_original_error(failure, original);
            }
            let notification = std::sync::Arc::make_mut(&mut envelope.notification);
            notification.value.clear();
            notification.source_failure = Some(failure);
            envelope.broadcast_overflow = true;
            let shared = std::sync::Arc::new(envelope.clone());
            state.broadcast_receivers.retain(|receiver| {
                let Some(receiver) = receiver.upgrade() else {
                    return false;
                };
                let mut receiver = receiver.lock().unwrap();
                receiver.retain_overflow(shared.clone(), state.delivery_index);
                true
            });
            state.shared_broadcast_terminal = Some(shared);
            state.quota = Some(envelope.clone());
            delivery.terminal = Some((sender.clone(), envelope));
        } else {
            delivery.queue.push_back((sender.clone(), envelope));
        }
        if delivery.draining || delivery.holds > 0 {
            false
        } else {
            delivery.draining = true;
            true
        }
    }

    #[cfg(any(not(target_os = "linux"), test))]
    fn drain_broadcast(&self) {
        loop {
            let next = {
                let mut delivery = self.broadcast_delivery.lock().unwrap();
                if delivery.holds > 0 {
                    delivery.draining = false;
                    return;
                }
                match delivery
                    .queue
                    .pop_front()
                    .or_else(|| delivery.terminal.take())
                {
                    Some(next) => Some(next),
                    None => {
                        delivery.draining = false;
                        None
                    }
                }
            };
            let Some((sender, mut envelope)) = next else {
                return;
            };
            {
                let mut state = self.state.lock().unwrap();
                state.delivery_index += 1;
                envelope.delivery_index = state.delivery_index;
                if let Some(fault) = state.faults.get_mut(&Self::key(&envelope.notification)) {
                    if fault.sequence == envelope.sequence {
                        fault.delivery_index = envelope.delivery_index;
                    }
                }
                if let Some(quota) = state.quota.as_mut() {
                    if quota.sequence == envelope.sequence {
                        quota.delivery_index = envelope.delivery_index;
                    }
                }
                state.in_flight_delivery = Some((envelope.delivery_index, envelope.sequence));
                state.broadcast_receivers.retain(|receiver| {
                    let Some(receiver) = receiver.upgrade() else {
                        return false;
                    };
                    let mut receiver = receiver.lock().unwrap();
                    if envelope.sequence <= receiver.floor {
                        match &mut receiver.old_deliveries {
                            Some((_, end)) => *end = envelope.delivery_index,
                            None => {
                                receiver.old_deliveries =
                                    Some((envelope.delivery_index, envelope.delivery_index))
                            }
                        }
                    }
                    for pending in &mut receiver.pending {
                        if envelope.sequence <= pending.envelope.sequence {
                            pending.delivery_end = envelope.delivery_index;
                            if envelope.sequence == pending.envelope.sequence {
                                pending.sent = true;
                            }
                        }
                    }
                    if let Some(omitted) = &mut receiver.omitted {
                        if envelope.sequence <= omitted.first.sequence {
                            omitted.delivery_end = envelope.delivery_index;
                            if envelope.sequence == omitted.first.sequence {
                                omitted.sent = true;
                            }
                        }
                    }
                    true
                });
            }
            // No listeners is not an error: retained evidence remains owned.
            let _ = sender.send(envelope);
            self.state.lock().unwrap().in_flight_delivery = None;
        }
    }

    #[cfg(any(target_os = "windows", test))]
    pub fn begin(&self, characteristic: &crate::api::Characteristic) -> Result<u64, crate::Error> {
        let mut state = self.lock_source_state();
        let key = (
            characteristic.service_uuid,
            characteristic.service_instance,
            characteristic.uuid,
            characteristic.instance,
        );
        if !state.enable_epochs.contains_key(&key) && state.enable_epochs.len() >= self.capacity {
            return Err(crate::Error::Platform(crate::PlatformError::new(
                "ubm-native",
                "notification-fault-quota",
                "Native notification subscription capacity exhausted",
            )));
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .filter(|sequence| *sequence < u64::MAX)
            .ok_or_else(|| {
                crate::Error::Platform(crate::PlatformError::new(
                    "ubm-native",
                    "notification-fault-quota",
                    "Native notification epoch exhausted",
                ))
            })?;
        let epoch = state.sequence;
        state.enable_epochs.insert(key, epoch);
        Ok(epoch)
    }

    #[cfg(any(target_os = "windows", test))]
    pub fn publish_for_epoch(
        &self,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        notification: ValueNotification,
        epoch: u64,
    ) {
        // The callback's captured native enable identity must still be live.
        // Publish under the same lock as disable so a queued old callback
        // cannot resurrect a retired terminal in a fresh enable.
        let mut state = self.lock_source_state();
        if state.enable_epochs.get(&Self::key(&notification)) != Some(&epoch) {
            return;
        }
        Self::admit_source_value_locked(
            &mut state,
            self.capacity,
            notification.clone(),
            crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown,
        );
        let drain = self.publish_locked(&mut state, sender, notification);
        drop(state);
        if drain {
            self.drain_broadcast();
        }
    }

    /// Publish only to the legacy broadcast while the native enable epoch is
    /// current. Group admission is performed separately by its activation owner.
    #[cfg(any(target_os = "windows", test))]
    pub fn publish_legacy_for_epoch(
        &self,
        sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
        notification: ValueNotification,
        epoch: u64,
    ) {
        let mut state = self.lock_source_state();
        if state.enable_epochs.get(&Self::key(&notification)) != Some(&epoch) {
            return;
        }
        let drain = self.publish_locked(&mut state, sender, notification);
        drop(state);
        if drain {
            self.drain_broadcast();
        }
    }

    /// Admit at a serialized native source before any buffered stream.
    /// `Lagged` describes peer-wide lost records; a value failure is scoped to
    /// its full native route. The caller must serialize source registration and
    /// retirement against its callbacks using this owner, not a downstream queue.
    pub fn publish_source_event(
        &self,
        generation: u64,
        event: crate::api::NotificationEvent,
    ) -> Result<(), SourceGroupError> {
        self.publish_source_event_with_provenance(
            generation,
            event,
            crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown,
        )
    }

    /// Publish a native callback with the origin known at its source.
    pub fn publish_source_event_with_provenance(
        &self,
        generation: u64,
        event: crate::api::NotificationEvent,
        provenance: crate::ubm_peer_notification_ingress::CallbackProvenance,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        match event {
            crate::api::NotificationEvent::Value(value) => {
                Self::admit_source_value_locked(&mut state, self.capacity, value, provenance);
            }
            crate::api::NotificationEvent::Lagged { skipped_records } => {
                for group in state.groups.values() {
                    let _ = group.ingress.admit(
                        generation,
                        None,
                        crate::api::NotificationEvent::Lagged { skipped_records },
                    );
                }
            }
        }
        Ok(())
    }

    fn source_ordinal_locked(state: &mut FaultState, rejected_bytes: usize) -> Option<u64> {
        let generation = state.source_generation?;
        match state.next_source_ordinal {
            Some(ordinal) => {
                state.next_source_ordinal = ordinal.checked_add(1);
                Some(ordinal)
            }
            None => {
                for group in state.groups.values() {
                    group
                        .ingress
                        .exhaust_source_ordinal(generation, rejected_bytes);
                }
                None
            }
        }
    }

    fn admit_source_value_locked(
        state: &mut FaultState,
        capacity: usize,
        notification: ValueNotification,
        provenance: crate::ubm_peer_notification_ingress::CallbackProvenance,
    ) {
        if notification.source_failure.is_some() {
            // This direct evidence is independent of broadcast retention.
            let key = Self::key(&notification);
            if state.direct_faults.contains_key(&key) || state.direct_faults.len() < capacity {
                state.direct_faults.entry(key).or_insert_with(|| {
                    let mut failure = notification.clone();
                    // This gap occurred before any future group joined.
                    failure.lost_before = 0;
                    failure
                });
            } else {
                // A future route join cannot assume unretained failure state is clean.
                state.direct_faults_exhausted = true;
            }
        }
        if let Some(generation) = state.source_generation {
            let Some(ordinal) = Self::source_ordinal_locked(state, notification.value.len()) else {
                return;
            };
            for group in state.groups.values() {
                let memberships: Vec<_> = group
                    .routes
                    .iter()
                    .filter(|(route, _, _, retiring)| !retiring && route.matches(&notification))
                    .map(|(_, membership, _, _)| *membership)
                    .collect();
                if !memberships.is_empty() {
                    // A single callback fans out under one source ordinal.
                    let _ = group.ingress.admit_members_at_ordinal(
                        generation,
                        &memberships,
                        crate::api::NotificationEvent::Value(notification.clone()),
                        provenance,
                        ordinal,
                    );
                } else if notification.lost_before != 0 {
                    // Loss is peer-wide, but the native failure belongs only
                    // to its characteristic. Do not pass it to other groups.
                    let _ = group.ingress.admit(
                        generation,
                        None,
                        crate::api::NotificationEvent::Lagged {
                            skipped_records: notification.lost_before,
                        },
                    );
                }
            }
        }
    }

    /// Fail all groups in one generation without attributing the failure or tail.
    pub fn fail_source_generation(
        &self,
        generation: u64,
        error: crate::PlatformError,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        state.source_generation = None;
        state.pending_source_failure = false;
        for (_, group) in std::mem::take(&mut state.groups) {
            // An earlier terminal remains authoritative.
            let _ = group.ingress.fail_source(generation, error.clone());
        }
        state.enable_epochs.clear();
        state.direct_faults.clear();
        state.held_direct_faults.clear();
        state.direct_faults_exhausted = false;
        state.source_activations.clear();
        state.activation_faults.clear();
        state.held_activation_faults.clear();
        let terminal_evidence = state.terminal_evidence.take();
        drop(state);
        if let Some((port, reservation)) = terminal_evidence {
            port.release(&reservation)
                .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        }
        Ok(())
    }

    /// Close one physical generation. Already-terminated groups retain their
    /// original terminal; all others observe closure with uncertain tail.
    pub fn close_source_generation(&self, generation: u64) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        Self::close_source_groups(&mut state);
        // Native callback epochs belong to this physical generation. An old
        // callback must not become eligible after the next generation starts.
        state.enable_epochs.clear();
        state.direct_faults.clear();
        state.held_direct_faults.clear();
        state.direct_faults_exhausted = false;
        state.source_activations.clear();
        state.activation_faults.clear();
        state.held_activation_faults.clear();
        let terminal_evidence = state.terminal_evidence.take();
        drop(state);
        if let Some((port, reservation)) = terminal_evidence {
            port.release(&reservation)
                .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        }
        Ok(())
    }

    #[cfg(any(not(target_os = "linux"), test))]
    pub fn clear_key(&self, key: NotificationKey) {
        let mut state = self.lock_source_state();
        state.faults.remove(&key);
        state.direct_faults.remove(&key);
        state.held_direct_faults.remove(&key);
        state.enable_epochs.remove(&key);
    }

    #[cfg(any(not(target_os = "linux"), test))]
    pub fn clear(&self, characteristic: &crate::api::Characteristic) {
        self.clear_key((
            characteristic.service_uuid,
            characteristic.service_instance,
            characteristic.uuid,
            characteristic.instance,
        ));
    }

    /// Retire evidence only if this disable still owns the native enable epoch.
    #[cfg(any(target_os = "windows", test))]
    pub fn clear_for_epoch(&self, characteristic: &crate::api::Characteristic, epoch: u64) -> bool {
        let key = (
            characteristic.service_uuid,
            characteristic.service_instance,
            characteristic.uuid,
            characteristic.instance,
        );
        let mut state = self.lock_source_state();
        if state.enable_epochs.get(&key) != Some(&epoch) {
            return false;
        }
        state.faults.remove(&key);
        state.direct_faults.remove(&key);
        state.held_direct_faults.remove(&key);
        state.enable_epochs.remove(&key);
        true
    }

    #[cfg(any(not(target_os = "linux"), test))]
    pub fn clear_all(&self) {
        let mut state = self.lock_source_state();
        Self::close_source_groups(&mut state);
        let terminal_evidence = state.terminal_evidence.take();
        state.faults.clear();
        state.direct_faults.clear();
        state.held_direct_faults.clear();
        state.direct_faults_exhausted = false;
        state.source_activations.clear();
        state.activation_faults.clear();
        state.held_activation_faults.clear();
        state.enable_epochs.clear();
        if !state.sequence_exhausted {
            state.quota = None;
        }
        // The last overflow belongs to the old graph; the permanent
        // sequence-exhaustion quota, if present, remains observable.
        state.shared_broadcast_terminal = None;
        // Preserve the sequence watermark so old queued evidence cannot gain
        // a fresh subscription identity after a confirmed native close.
        drop(state);
        if let Some((port, reservation)) = terminal_evidence {
            if let Err(error) = port.release(&reservation) {
                log::error!("terminal evidence release after source clear failed: {error}");
            }
        }
    }
}

#[cfg(test)]
pub fn retained_notifications(
    receiver: Receiver<NotificationEnvelope>,
    faults: std::sync::Arc<NotificationFaults>,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>> {
    retained_notifications_with_receiver(faults, || receiver)
}

/// Subscribe and register the terminal owner under the publication lock.
#[cfg(any(target_os = "windows", target_vendor = "apple", test))]
pub fn retained_notifications_from_sender(
    sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
    faults: std::sync::Arc<NotificationFaults>,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>> {
    retained_notifications_with_receiver(faults, || sender.subscribe())
}

/// Test-only bridge for exercising the real bounded source and both retained
/// streams across the desktop crate boundary, without a native radio.
#[cfg(all(
    feature = "reclaim-test-support",
    any(target_os = "windows", target_vendor = "apple")
))]
pub fn legacy_overflow_receiver_fixture(
    prefix: [ValueNotification; 2],
    rejected: ValueNotification,
) -> [Pin<Box<dyn Stream<Item = ValueNotification> + Send>>; 2] {
    let faults = std::sync::Arc::new(NotificationFaults::new(2));
    let (sender, _) = tokio::sync::broadcast::channel(1);
    let streams = [
        retained_notifications_from_sender(&sender, faults.clone()),
        retained_notifications_from_sender(&sender, faults.clone()),
    ];
    let hold = faults.hold_notification_delivery();
    for note in prefix {
        faults.publish(&sender, note);
    }
    faults.publish(&sender, rejected);
    drop(hold);
    streams
}

/// Test-only access to the native source-group owner and its real consumer.
#[cfg(feature = "reclaim-test-support")]
pub struct PeerGroupTestSource {
    faults: std::sync::Arc<NotificationFaults>,
    handle: SourceGroupHandle,
    generation: u64,
}

#[cfg(feature = "reclaim-test-support")]
impl PeerGroupTestSource {
    pub fn new(generation: u64, capacity: usize) -> (Self, SourceGroupConsumer) {
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        faults.start_source_generation(generation).unwrap();
        let (handle, consumer) = faults.open_source_group(generation, capacity, 128).unwrap();
        (
            Self {
                faults,
                handle,
                generation,
            },
            consumer,
        )
    }

    pub fn register(
        &self,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
    ) -> Result<crate::ubm_peer_notification_ingress::Membership, SourceGroupError> {
        self.faults
            .start_source_activation(self.generation, route, member)?;
        self.faults
            .register_source_route_for_activation(&self.handle, route, member, member)
    }

    pub fn publish(
        &self,
        activation: u64,
        value: ValueNotification,
    ) -> Result<(), SourceGroupError> {
        let route = crate::ubm_peer_notification_ingress::NotificationRoute {
            service_uuid: value.service_uuid,
            service_instance: value.service_instance,
            characteristic_uuid: value.uuid,
            characteristic_instance: value.instance,
        };
        self.faults
            .publish_source_value_for_activation(self.generation, route, activation, value)
    }

    /// Exercise two native operations admitted before either one fails.
    pub fn publish_two_failures(
        &self,
        first: crate::PlatformError,
        second: crate::PlatformError,
    ) -> Result<(), SourceGroupError> {
        let mut a = self
            .faults
            .reserve_future_source_failure(self.generation, 8)?;
        let mut b = self
            .faults
            .reserve_future_source_failure(self.generation, 8)?;
        let a = a.capture(first, usize::MAX).map_err(|(reason, _)| reason)?;
        let b = b
            .capture(second, usize::MAX)
            .map_err(|(reason, _)| reason)?;
        a.release();
        b.release();
        Ok(())
    }
}

#[cfg(any(not(target_os = "linux"), test))]
fn retained_notifications_with_receiver(
    faults: std::sync::Arc<NotificationFaults>,
    subscribe: impl FnOnce() -> Receiver<NotificationEnvelope>,
) -> Pin<Box<dyn Stream<Item = ValueNotification> + Send>> {
    let (receiver, floor, initial, terminal) = {
        let mut state = faults.state.lock().unwrap();
        let receiver = subscribe();
        let mut initial: Vec<_> = state
            .faults
            .values()
            .chain(state.quota.iter().filter(|entry| !entry.broadcast_overflow))
            .cloned()
            .collect();
        initial.extend(
            state
                .shared_broadcast_terminal
                .iter()
                .map(|entry| (**entry).clone()),
        );
        initial.sort_by_key(|fault| fault.sequence);
        let terminal = register_broadcast_receiver(&mut state);
        (receiver, state.sequence, initial, terminal)
    };
    let initial_faults = faults.clone();
    let initial = futures::stream::iter(initial).filter_map(move |envelope| {
        let state = initial_faults.state.lock().unwrap();
        let live = envelope.broadcast_overflow
            || state
                .faults
                .get(&NotificationFaults::key(&envelope.notification))
                .is_some_and(|fault| fault.sequence == envelope.sequence)
            || state
                .quota
                .as_ref()
                .is_some_and(|fault| fault.sequence == envelope.sequence);
        ready(live.then(|| (envelope, 0u64, 0u64)))
    });
    let faults_for_delivery = faults.clone();
    let delivery_terminal = terminal.clone();
    let values = initial.chain(
        BroadcastStream::new(receiver)
            .scan((0u64, floor), move |(missed, last_sequence), item| {
                let mut out = Vec::new();
                match item {
                    Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                        *missed = missed.saturating_add(skipped)
                    }
                    Ok(envelope) => {
                        let gap_start = envelope.delivery_index.saturating_sub(u128::from(*missed));
                        *missed = subscription_lag(&terminal, *missed, &envelope);
                        let mut retained = Vec::new();
                        if *missed > 0 {
                            let state = faults.state.lock().unwrap();
                            retained = state
                                .faults
                                .values()
                                .chain(state.quota.iter().filter(|entry| !entry.broadcast_overflow))
                                .filter(|fault| {
                                    fault.sequence > *last_sequence
                                        && fault.sequence < envelope.sequence
                                })
                                .cloned()
                                .collect();
                        }
                        for entry in terminal
                            .lock()
                            .unwrap()
                            .retained(*last_sequence, Some(envelope.sequence))
                        {
                            retained.retain(|other| other.sequence != entry.sequence);
                            retained.push(entry);
                        }
                        retained.sort_by_key(|fault| fault.sequence);
                        let current_retained = retained
                            .iter()
                            .any(|entry| entry.sequence == envelope.sequence);
                        // Attribute each gap only to the next retained control or
                        // value. Publication sequence can contain unsent holes;
                        // only broadcast delivery positions describe loss.
                        let terminal_state = terminal.lock().unwrap();
                        let mut cursor = gap_start;
                        for fault in retained {
                            let pending = fault
                                .broadcast_overflow
                                .then(|| terminal_state.position(fault.sequence))
                                .flatten();
                            let position = if let Some((end, sent)) = pending {
                                if sent { end } else { end.saturating_add(1) }
                            } else {
                                fault.delivery_index
                            };
                            let end = position.min(envelope.delivery_index).max(cursor);
                            let loss = subscription_lag_between(&terminal_state, cursor, end);
                            *missed = missed.saturating_sub(loss);

                            let mut overwritten_control = 0;
                            if position >= cursor
                                && position < envelope.delivery_index
                                && pending.is_none_or(|(_, sent)| sent)
                            {
                                let control = subscription_lag_between(
                                    &terminal_state,
                                    position,
                                    position.saturating_add(1),
                                );
                                *missed = missed.saturating_sub(control);
                                overwritten_control = control;
                                cursor = position.saturating_add(1);
                            } else {
                                cursor = end;
                            }
                            out.push((fault, loss, overwritten_control));
                        }
                        drop(terminal_state);
                        let current = if envelope.broadcast_overflow {
                            let terminal = terminal.lock().unwrap();
                            envelope.sequence > terminal.omitted_through
                                && !terminal.omitted.as_ref().is_some_and(|omitted| {
                                    envelope.sequence >= omitted.first.sequence
                                        && envelope.sequence <= omitted.last_sequence
                                })
                        } else if envelope.notification.source_failure.is_some() {
                            let state = faults.state.lock().unwrap();
                            state
                                .faults
                                .get(&NotificationFaults::key(&envelope.notification))
                                .is_some_and(|fault| fault.sequence == envelope.sequence)
                                || state
                                    .quota
                                    .as_ref()
                                    .is_some_and(|fault| fault.sequence == envelope.sequence)
                        } else {
                            true
                        };
                        if envelope.sequence > floor && current && !current_retained {
                            out.push((envelope.clone(), 0, 0));
                        }
                        *last_sequence = (*last_sequence).max(envelope.sequence);
                        if !current_retained && envelope.sequence > floor && current {
                            if let Some(last) = out.last_mut() {
                                last.1 = last.1.saturating_add(std::mem::take(missed));
                            }
                        }
                    }
                }
                ready(Some(futures::stream::iter(out)))
            })
            .flatten(),
    );
    let delivery_faults = faults_for_delivery;
    Box::pin(
        values
            .scan(
                0u64,
                move |carried, (mut envelope, loss, overwritten_control)| {
                    let state = delivery_faults.state.lock().unwrap();
                    let live = envelope.broadcast_overflow
                        || envelope.notification.source_failure.is_none()
                        || state
                            .faults
                            .get(&NotificationFaults::key(&envelope.notification))
                            .is_some_and(|fault| fault.sequence == envelope.sequence)
                        || state
                            .quota
                            .as_ref()
                            .is_some_and(|fault| fault.sequence == envelope.sequence);
                    *carried = carried.saturating_add(loss);
                    if !live {
                        // The replay no longer accounts for its overwritten broadcast slot.
                        *carried = carried.saturating_add(overwritten_control);
                    }
                    let value = live.then(|| {
                        if envelope.broadcast_overflow {
                            envelope = delivery_terminal
                                .lock()
                                .unwrap()
                                .delivered(envelope.clone());
                        }
                        let mut notification = (*envelope.notification).clone();
                        notification.lost_before = notification
                            .lost_before
                            .saturating_add(std::mem::take(carried));
                        notification
                    });
                    ready(Some(value))
                },
            )
            .filter_map(ready),
    )
}

/// Ordered peer stream with retained native terminal evidence. A lag counts
/// overwritten post-subscription broadcast records, including terminal
/// records replayed below.
#[cfg(any(not(target_os = "linux"), test))]
pub fn retained_notification_events(
    sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
    faults: std::sync::Arc<NotificationFaults>,
) -> Pin<Box<dyn Stream<Item = crate::api::NotificationEvent> + Send>> {
    retained_notification_events_with_hook(sender, faults, || {})
}

#[cfg(any(not(target_os = "linux"), test))]
fn retained_notification_events_with_hook(
    sender: &tokio::sync::broadcast::Sender<NotificationEnvelope>,
    faults: std::sync::Arc<NotificationFaults>,
    after_subscribe: impl FnOnce(),
) -> Pin<Box<dyn Stream<Item = crate::api::NotificationEvent> + Send>> {
    use crate::api::NotificationEvent;
    use std::collections::VecDeque;
    let (receiver, floor, initial, terminal) = {
        let mut state = faults.state.lock().unwrap();
        let receiver = sender.subscribe();
        after_subscribe();
        let mut initial: Vec<_> = state
            .faults
            .values()
            .chain(state.quota.iter().filter(|entry| !entry.broadcast_overflow))
            .cloned()
            .collect();
        initial.extend(
            state
                .shared_broadcast_terminal
                .iter()
                .map(|entry| (**entry).clone()),
        );
        initial.sort_by_key(|entry| entry.sequence);
        let terminal = register_broadcast_receiver(&mut state);
        (receiver, state.sequence, initial, terminal)
    };
    let initial_faults = faults.clone();
    let initial_terminal = terminal.clone();
    let initial = futures::stream::iter(initial).filter_map(move |entry| {
        let state = initial_faults.state.lock().unwrap();
        let live = entry.broadcast_overflow
            || state
                .faults
                .get(&NotificationFaults::key(&entry.notification))
                .is_some_and(|fault| fault.sequence == entry.sequence)
            || state
                .quota
                .as_ref()
                .is_some_and(|fault| fault.sequence == entry.sequence);
        ready(live.then(|| {
            if entry.broadcast_overflow {
                initial_terminal.lock().unwrap().delivered(entry.clone());
            }
            NotificationEvent::Value((*entry.notification).clone())
        }))
    });
    enum Pending {
        Lag(u64),
        Envelope(NotificationEnvelope),
    }
    let state = (
        BroadcastStream::new(receiver),
        faults,
        floor,
        floor,
        0u64,
        VecDeque::new(),
        false,
        terminal,
    );
    let live = futures::stream::unfold(
        state,
        |(mut receiver, faults, floor, mut last, mut missed, mut pending, mut closed, terminal)| async move {
            loop {
                if let Some(item) = pending.pop_front() {
                    let event = match item {
                        Pending::Lag(skipped_records) => {
                            Some(NotificationEvent::Lagged { skipped_records })
                        }
                        Pending::Envelope(mut entry) => {
                            if entry.broadcast_overflow {
                                entry = terminal.lock().unwrap().delivered(entry);
                            }
                            let current = if entry.broadcast_overflow {
                                true
                            } else if entry.notification.source_failure.is_some() {
                                let state = faults.state.lock().unwrap();
                                state
                                    .faults
                                    .get(&NotificationFaults::key(&entry.notification))
                                    .is_some_and(|fault| fault.sequence == entry.sequence)
                                    || state
                                        .quota
                                        .as_ref()
                                        .is_some_and(|fault| fault.sequence == entry.sequence)
                            } else {
                                true
                            };
                            current.then(|| NotificationEvent::Value((*entry.notification).clone()))
                        }
                    };
                    if let Some(event) = event {
                        return Some((
                            event,
                            (
                                receiver, faults, floor, last, missed, pending, closed, terminal,
                            ),
                        ));
                    }
                    continue;
                }
                if closed {
                    return None;
                }
                match receiver.next().await {
                    Some(Err(BroadcastStreamRecvError::Lagged(skipped))) => {
                        missed = missed.saturating_add(skipped)
                    }
                    item => {
                        let gap_start = item
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .map(|entry| entry.delivery_index.saturating_sub(u128::from(missed)));
                        if let Some(Ok(entry)) = &item {
                            missed = subscription_lag(&terminal, missed, entry);
                        }
                        let upper = item
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .map(|entry| entry.sequence);
                        let mut retained = Vec::new();
                        if missed > 0 {
                            let state = faults.state.lock().unwrap();
                            retained = state
                                .faults
                                .values()
                                .chain(state.quota.iter().filter(|entry| !entry.broadcast_overflow))
                                .filter(|entry| {
                                    entry.sequence > last
                                        && upper.is_none_or(|upper| entry.sequence < upper)
                                })
                                .cloned()
                                .collect();
                        }
                        for entry in terminal.lock().unwrap().retained(last, upper) {
                            retained.retain(|other| other.sequence != entry.sequence);
                            retained.push(entry);
                        }
                        retained.sort_by_key(|entry| entry.sequence);
                        let mut cursor = gap_start.unwrap_or(0);
                        let terminal_state = terminal.lock().unwrap();
                        for entry in retained {
                            if let Some(Ok(current)) = &item {
                                let terminal_position = entry
                                    .broadcast_overflow
                                    .then(|| terminal_state.position(entry.sequence))
                                    .flatten();
                                let position = if let Some((end, sent)) = terminal_position {
                                    if sent { end } else { end.saturating_add(1) }
                                } else {
                                    entry.delivery_index
                                };
                                let end = if terminal_position.is_some_and(|(_, sent)| !sent) {
                                    position
                                } else {
                                    position.saturating_add(1)
                                };
                                let end = end.min(current.delivery_index).max(cursor);
                                let loss = subscription_lag_between(&terminal_state, cursor, end);
                                if loss > 0 {
                                    pending.push_back(Pending::Lag(loss));
                                }
                                missed = missed.saturating_sub(loss);
                                cursor = end;
                            }
                            last = last.max(entry.sequence);
                            pending.push_back(Pending::Envelope(entry));
                        }
                        drop(terminal_state);
                        if missed > 0 {
                            pending.push_back(Pending::Lag(std::mem::take(&mut missed)));
                        }
                        if let Some(Ok(entry)) = item {
                            let sequence = entry.sequence;
                            let omitted = entry.broadcast_overflow && {
                                let terminal = terminal.lock().unwrap();
                                sequence <= terminal.omitted_through
                                    || terminal.omitted.as_ref().is_some_and(|omitted| {
                                        sequence >= omitted.first.sequence
                                            && sequence <= omitted.last_sequence
                                    })
                            };
                            if sequence > floor && sequence > last && !omitted {
                                pending.push_back(Pending::Envelope(entry));
                            }
                            last = last.max(sequence);
                        } else {
                            closed = true;
                        }
                    }
                }
            }
        },
    );
    Box::pin(initial.chain(live))
}

#[cfg(test)]
mod ubm_lag_tests {
    use super::notifications_stream_from_broadcast_receiver;
    use crate::api::ValueNotification;
    use futures::stream::StreamExt;
    use uuid::Uuid;

    fn value(byte: u8) -> ValueNotification {
        ValueNotification {
            uuid: Uuid::nil(),
            instance: 0,
            service_uuid: Uuid::nil(),
            service_instance: 0,
            value: vec![byte],
            source_failure: None,
            lost_before: 0,
        }
    }

    #[test]
    fn event_stream_reports_lag_before_values_and_close() {
        use super::notification_events_from_broadcast_receiver;
        use crate::api::NotificationEvent;
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let stream = notification_events_from_broadcast_receiver(receiver);
        for byte in 1..=4 {
            sender.send(value(byte)).unwrap();
        }
        drop(sender);
        let received = futures::executor::block_on(stream.collect::<Vec<_>>());
        assert_eq!(
            received,
            vec![
                NotificationEvent::Lagged { skipped_records: 2 },
                NotificationEvent::Value(value(3)),
                NotificationEvent::Value(value(4))
            ]
        );
    }

    #[test]
    fn a_lagging_receiver_learns_what_it_missed() {
        // The real bounded broadcast every platform peripheral uses: two
        // slots, five sends before the receiver reads.
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = notifications_stream_from_broadcast_receiver(receiver);
        for byte in 1..=5 {
            sender.send(value(byte)).expect("receiver alive");
        }
        drop(sender);
        let received: Vec<(u8, u64)> = futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(note) = stream.next().await {
                out.push((note.value[0], note.lost_before));
            }
            out
        });
        assert_eq!(
            received,
            vec![(4, 3), (5, 0)],
            "the three overwritten values are reported on the next one"
        );
    }

    #[test]
    fn a_receiver_that_keeps_up_loses_nothing() {
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = notifications_stream_from_broadcast_receiver(receiver);
        let received = futures::executor::block_on(async {
            let mut out = Vec::new();
            for byte in 1..=4 {
                sender.send(value(byte)).expect("receiver alive");
                out.push(stream.next().await.expect("value").lost_before);
            }
            out
        });
        assert_eq!(received, vec![0, 0, 0, 0]);
    }
}

#[cfg(test)]
#[path = "../winrtble/callback_gate.rs"]
mod callback_gate;

#[cfg(test)]
mod retained_fault_tests {
    use super::*;
    use crate::{PlatformError, api::Characteristic};
    use uuid::Uuid;

    #[test]
    fn legacy_revalidates_each_lagged_fault_after_retirement() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut stream = retained_notifications_from_sender(&sender, faults.clone());
        faults.publish(&sender, note(1, true));
        faults.publish(&sender, note(2, true));
        faults.publish(&sender, note(3, false));
        assert_eq!(stream.next().now_or_never().unwrap().unwrap().instance, 1);
        faults.clear_key((Uuid::nil(), 7, Uuid::nil(), 2));
        faults.publish(&sender, note(2, true));
        assert_eq!(stream.next().now_or_never().unwrap().unwrap().instance, 3);
        assert_eq!(stream.next().now_or_never().unwrap().unwrap().instance, 2);
    }

    #[test]
    fn retired_queued_fault_preserves_preceding_loss() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut stream = retained_notifications_from_sender(&sender, faults.clone());
        for (instance, failure) in [(1, true), (2, false), (3, true), (4, false)] {
            faults.publish(&sender, note(instance, failure));
        }
        assert_eq!(stream.next().now_or_never().unwrap().unwrap().instance, 1);
        faults.clear_key((Uuid::nil(), 7, Uuid::nil(), 3));
        let value = stream.next().now_or_never().unwrap().unwrap();
        assert_eq!((value.instance, value.lost_before), (4, 2));
    }

    #[test]
    fn graph_clear_after_partial_replay_counts_retired_overwritten_fault() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut stream = retained_notifications_from_sender(&sender, faults.clone());
        for (instance, failure) in [(1, true), (2, false), (3, true), (4, false)] {
            faults.publish(&sender, note(instance, failure));
        }
        assert_eq!(stream.next().now_or_never().unwrap().unwrap().instance, 1);
        faults.clear_all();
        let value = stream.next().now_or_never().unwrap().unwrap();
        assert_eq!((value.instance, value.lost_before), (4, 2));
    }

    #[test]
    fn idle_receivers_observe_replaced_overflow_evidence() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut idle = retained_notifications_from_sender(&sender, faults.clone());
        let mut events = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        faults.publish(&sender, note(2, false));
        faults.clear_all();
        faults.publish(&sender, note(3, true));
        faults.clear_all();
        drop(hold);
        faults.publish(&sender, note(4, false));
        faults.publish(&sender, note(5, false));
        let mut idle_failures = Vec::new();
        while let Some(Some(value)) = idle.next().now_or_never() {
            if let Some(error) = value.source_failure {
                idle_failures.push((value.instance, error.code));
            }
        }
        let mut event_failures = Vec::new();
        while let Some(Some(event)) = events.next().now_or_never() {
            if let crate::api::NotificationEvent::Value(value) = event {
                if let Some(error) = value.source_failure {
                    event_failures.push((value.instance, error.code));
                }
            }
        }
        for failures in [idle_failures, event_failures] {
            assert_eq!(
                failures,
                vec![
                    (2, "notification-broadcast-overflow".to_owned()),
                    (3, "notification-broadcast-overflow".to_owned()),
                ]
            );
        }
    }

    #[test]
    fn rejected_callback_retains_all_original_metadata_in_order() {
        use futures::{FutureExt, StreamExt};
        for original in [
            PlatformError::new("corebluetooth", "17", "NSError")
                .with("nsErrorDomain", "CBATTErrorDomain")
                .with("tailUncertain", "native detail")
                .with("originalMetadataStart", "native detail")
                .with("nsErrorDomain", "second domain"),
            PlatformError::new("winrt", "hresult", "HRESULT")
                .with("hresult", "0x800710DF")
                .with("gattStatus", "unreachable"),
        ] {
            let faults = std::sync::Arc::new(NotificationFaults::new(1));
            let (sender, _) = tokio::sync::broadcast::channel(2);
            let mut stream = retained_notifications_from_sender(&sender, faults.clone());
            let hold = faults.hold_notification_delivery();
            faults.publish(&sender, note(1, false));
            let mut rejected = note(2, true);
            rejected.source_failure = Some(original.clone());
            faults.publish(&sender, rejected);
            drop(hold);
            let _ = stream.next().now_or_never().unwrap().unwrap();
            let error = stream
                .next()
                .now_or_never()
                .unwrap()
                .unwrap()
                .source_failure
                .unwrap();
            assert_eq!(error.code, "notification-broadcast-overflow");
            let start: usize = error
                .metadata
                .iter()
                .find(|(key, _)| *key == "originalMetadataStart")
                .unwrap()
                .1
                .parse()
                .unwrap();
            let count: usize = error
                .metadata
                .iter()
                .find(|(key, _)| *key == "originalMetadataCount")
                .unwrap()
                .1
                .parse()
                .unwrap();
            assert_eq!(count, original.metadata.len());
            assert!(
                error
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "originalDomain" && value == original.domain)
            );
            assert!(
                error
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "originalCode" && value == &original.code)
            );
            assert!(
                error
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "originalMessage" && value == &original.message)
            );
            assert_eq!(&error.metadata[start..], original.metadata.as_slice());
        }
    }

    #[test]
    fn sequence_exhaustion_survives_clear_and_saturated_receivers() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        faults.state.lock().unwrap().sequence = u64::MAX - 20;
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut values = retained_notifications_from_sender(&sender, faults.clone());
        let mut events = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        for instance in 2..=20 {
            faults.publish(&sender, note(instance, true));
            faults.clear_all();
        }
        faults.publish(&sender, note(21, false));
        faults.clear_all();
        drop(hold);
        let mut value_items = Vec::new();
        while let Some(Some(value)) = values.next().now_or_never() {
            value_items.push(value);
        }
        let mut event_items = Vec::new();
        while let Some(Some(event)) = events.next().now_or_never() {
            if let NotificationEvent::Value(value) = event {
                event_items.push(value);
            }
        }
        for items in [&value_items, &event_items] {
            assert!(
                items.iter().all(|value| value.source_failure.is_some()),
                "unexpected successful value: {items:?}"
            );
            assert_eq!(
                items
                    .iter()
                    .filter(|value| value.source_failure.as_ref().unwrap().code
                        == "notification-broadcast-overflow")
                    .count(),
                16
            );
            assert_eq!(
                items
                    .iter()
                    .filter(|value| value.source_failure.as_ref().unwrap().code
                        == "notification-overflow-evidence-omitted")
                    .count(),
                1
            );
            let exhaustion = items.last().unwrap().source_failure.as_ref().unwrap();
            assert_eq!(exhaustion.code, "notification-fault-quota");
            assert!(
                exhaustion
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "sequenceExhausted" && value == "true")
            );
        }
        faults.publish(&sender, note(22, false));
        assert!(values.next().now_or_never().is_none());
        assert!(events.next().now_or_never().is_none());
        let mut late = retained_notifications_from_sender(&sender, faults.clone());
        let terminal = late
            .next()
            .now_or_never()
            .unwrap()
            .unwrap()
            .source_failure
            .unwrap();
        assert!(
            terminal
                .metadata
                .iter()
                .any(|(key, value)| *key == "sequenceExhausted" && value == "true")
        );
    }

    #[test]
    fn enable_epoch_cannot_consume_the_last_exhaustion_identity() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(2));
        faults.state.lock().unwrap().sequence = u64::MAX - 1;
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        assert!(faults.begin(&characteristic).is_err());
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notification_events(&sender, faults.clone());
        let mut value = note(1, true);
        let original =
            PlatformError::new("winrt", "hresult", "failure").with("hresult", "0x800710DF");
        value.source_failure = Some(original.clone());
        faults.publish(&sender, value);
        faults.clear_all();
        let event = stream.next().now_or_never().unwrap().unwrap();
        let crate::api::NotificationEvent::Value(value) = event else {
            panic!("expected exhaustion terminal")
        };
        let error = value.source_failure.unwrap();
        assert_eq!(error.code, "notification-fault-quota");
        assert!(
            error
                .metadata
                .iter()
                .any(|(key, value)| *key == "sequenceExhausted" && value == "true")
        );
        assert!(
            error
                .metadata
                .iter()
                .any(|(key, value)| *key == "rejectedBytes" && value == "1")
        );
        assert!(
            error
                .metadata
                .iter()
                .any(|(key, value)| *key == "tailUncertain" && value == "true")
        );
        let start: usize = error
            .metadata
            .iter()
            .find(|(key, _)| *key == "originalMetadataStart")
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert_eq!(&error.metadata[start..], original.metadata.as_slice());
        assert!(faults.begin(&characteristic).is_err());
        let mut late = retained_notification_events(&sender, faults.clone());
        assert!(
            matches!(late.next().now_or_never(), Some(Some(crate::api::NotificationEvent::Value(value))) if value.source_failure.as_ref().is_some_and(|error| error.metadata.iter().any(|(key, value)| *key == "sequenceExhausted" && value == "true")))
        );
    }

    #[test]
    fn saturated_receivers_report_ordered_omission_and_resume() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Wake, Waker};
        struct WakeCount(AtomicUsize);
        impl Wake for WakeCount {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut values = retained_notifications_from_sender(&sender, faults.clone());
        let mut events = retained_notification_events(&sender, faults.clone());
        let wake = std::sync::Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            values.as_mut().poll_next(&mut context),
            Poll::Pending
        ));
        assert!(matches!(
            events.as_mut().poll_next(&mut context),
            Poll::Pending
        ));
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        for instance in 2..=21 {
            faults.publish(&sender, note(instance, true));
            faults.clear_all();
        }
        {
            let state = faults.state.lock().unwrap();
            let first = state.broadcast_receivers[0].upgrade().unwrap();
            let second = state.broadcast_receivers[1].upgrade().unwrap();
            let first = first.lock().unwrap();
            let second = second.lock().unwrap();
            assert_eq!(first.pending.len(), OVERFLOW_RECEIVER_CAPACITY);
            assert_eq!(second.pending.len(), OVERFLOW_RECEIVER_CAPACITY);
            assert!(std::sync::Arc::ptr_eq(
                &first.pending[0].envelope,
                &second.pending[0].envelope
            ));
            assert_eq!(first.omitted.as_ref().unwrap().count, 4);
            assert!(
                first
                    .omitted
                    .as_ref()
                    .unwrap()
                    .first
                    .notification
                    .source_failure
                    .is_none()
            );
        }
        assert_eq!(wake.0.load(Ordering::SeqCst), 0);
        drop(hold);
        assert!(wake.0.load(Ordering::SeqCst) >= 2);
        faults.publish(&sender, note(22, false));
        faults.publish(&sender, note(23, false));
        let mut value_faults = Vec::new();
        while let Some(Some(value)) = values.next().now_or_never() {
            if let Some(error) = value.source_failure {
                value_faults.push((value.instance, error));
            }
        }
        let mut event_faults = Vec::new();
        while let Some(Some(event)) = events.next().now_or_never() {
            if let NotificationEvent::Value(value) = event {
                if let Some(error) = value.source_failure {
                    event_faults.push((value.instance, error));
                }
            }
        }
        for found in [value_faults, event_faults] {
            assert_eq!(found.len(), 17);
            assert_eq!(
                found
                    .iter()
                    .take(16)
                    .map(|(instance, _)| *instance)
                    .collect::<Vec<_>>(),
                (2..=17).collect::<Vec<_>>()
            );
            let receipt = &found[16].1;
            assert_eq!(receipt.code, "notification-overflow-evidence-omitted");
            assert!(
                receipt
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "omittedCount" && value == "4")
            );
            assert!(
                receipt
                    .metadata
                    .iter()
                    .any(|(key, value)| *key == "tailUncertain" && value == "true")
            );
        }
        faults.publish(&sender, note(24, false));
        assert_eq!(values.next().now_or_never().unwrap().unwrap().instance, 24);
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 24)
        );
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(25, false));
        faults.publish(&sender, note(26, true));
        faults.clear_all();
        drop(hold);
        faults.publish(&sender, note(27, false));
        let value = values.next().now_or_never().unwrap().unwrap();
        assert_eq!(value.instance, 26);
        assert_eq!(
            value.source_failure.unwrap().code,
            "notification-broadcast-overflow"
        );
        let mut found = None;
        while let Some(Some(event)) = events.next().now_or_never() {
            if let NotificationEvent::Value(value) = event {
                if let Some(error) = value.source_failure {
                    found = Some((value.instance, error.code));
                }
            }
        }
        assert_eq!(
            found,
            Some((26, "notification-broadcast-overflow".to_owned()))
        );
    }

    #[test]
    fn omission_receipt_includes_overflows_added_after_lag_was_polled() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut events = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        for instance in 2..=19 {
            faults.publish(&sender, note(instance, true));
            faults.clear_all();
        }
        drop(hold);
        faults.publish(&sender, note(20, false));
        faults.publish(&sender, note(21, false));
        assert!(matches!(
            events.next().now_or_never(),
            Some(Some(NotificationEvent::Lagged { .. }))
        ));
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(22, false));
        faults.publish(&sender, note(23, true));
        faults.clear_all();
        drop(hold);
        let mut receipts = Vec::new();
        while let Some(Some(event)) = events.next().now_or_never() {
            if let NotificationEvent::Value(value) = event {
                if let Some(error) = value.source_failure {
                    if error.code == "notification-overflow-evidence-omitted" {
                        receipts.push(error);
                    }
                }
            }
        }
        assert_eq!(receipts.len(), 1);
        assert!(
            receipts[0]
                .metadata
                .iter()
                .any(|(key, value)| *key == "omittedCount" && value == "3")
        );
    }

    #[test]
    fn partially_polled_event_receiver_observes_later_overflow_evidence() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut stream = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        faults.publish(&sender, note(2, false));
        faults.clear_all();
        drop(hold);
        faults.publish(&sender, note(3, false));
        faults.publish(&sender, note(4, false));
        assert!(matches!(
            stream.next().now_or_never(),
            Some(Some(NotificationEvent::Lagged { .. }))
        ));
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(5, false));
        faults.publish(&sender, note(6, true));
        faults.clear_all();
        drop(hold);
        faults.publish(&sender, note(7, false));
        faults.publish(&sender, note(8, false));
        let mut failures = Vec::new();
        while let Some(Some(event)) = stream.next().now_or_never() {
            if let NotificationEvent::Value(value) = event {
                if value.source_failure.is_some() {
                    failures.push(value.instance);
                }
            }
        }
        assert_eq!(failures, vec![2, 6]);
    }

    #[test]
    fn replay_keeps_large_diagnostics_shared_for_idle_receivers() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(2));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut values: Vec<_> = (0..16)
            .map(|_| retained_notifications_from_sender(&sender, faults.clone()))
            .collect();
        let mut events: Vec<_> = (0..16)
            .map(|_| retained_notification_events(&sender, faults.clone()))
            .collect();
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, true));
        faults.publish(&sender, note(2, false));
        let mut large = note(3, true);
        large.source_failure = Some(PlatformError::new(
            "native",
            "large",
            "x".repeat(128 * 1024),
        ));
        faults.publish(&sender, large);
        drop(hold);
        let payload = faults
            .state
            .lock()
            .unwrap()
            .shared_broadcast_terminal
            .as_ref()
            .unwrap()
            .notification
            .clone();
        let before = std::sync::Arc::strong_count(&payload);
        for stream in &mut values {
            assert_eq!(stream.next().now_or_never().unwrap().unwrap().instance, 1);
        }
        for stream in &mut events {
            assert!(matches!(
                stream.next().now_or_never().unwrap(),
                Some(crate::api::NotificationEvent::Lagged { .. })
            ));
            assert!(
                matches!(stream.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 1)
            );
        }
        // The broadcast releases its one retained slot after the final read.
        assert!(
            std::sync::Arc::strong_count(&payload) >= before + 31,
            "partially polled replay must retain the original payload"
        );
        drop(values);
        drop(events);
        assert!(std::sync::Arc::strong_count(&payload) <= before);

        let faults = std::sync::Arc::new(NotificationFaults::new(2));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut large = note(1, true);
        large.source_failure = Some(PlatformError::new(
            "native",
            "large",
            "x".repeat(128 * 1024),
        ));
        faults.publish(&sender, large);
        let payload = faults
            .state
            .lock()
            .unwrap()
            .faults
            .values()
            .next()
            .unwrap()
            .notification
            .clone();
        let before = std::sync::Arc::strong_count(&payload);
        let late_values: Vec<_> = (0..16)
            .map(|_| retained_notifications_from_sender(&sender, faults.clone()))
            .collect();
        let late_events: Vec<_> = (0..16)
            .map(|_| retained_notification_events(&sender, faults.clone()))
            .collect();
        assert_eq!(
            std::sync::Arc::strong_count(&payload),
            before + 32,
            "late snapshots must share the retained diagnostic"
        );
        drop(late_values);
        drop(late_events);
        assert_eq!(std::sync::Arc::strong_count(&payload), before);
    }

    #[test]
    fn overflow_streams_continue_after_graph_clear() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(8);
        let mut values = retained_notifications_from_sender(&sender, faults.clone());
        let mut events = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        faults.publish(&sender, note(2, false));
        drop(hold);
        assert_eq!(values.next().now_or_never().unwrap().unwrap().instance, 1);
        assert!(
            values
                .next()
                .now_or_never()
                .unwrap()
                .unwrap()
                .source_failure
                .is_some()
        );
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 1)
        );
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.source_failure.is_some())
        );
        faults.clear_all();
        faults.publish(&sender, note(3, false));
        assert_eq!(values.next().now_or_never().unwrap().unwrap().instance, 3);
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 3)
        );
    }

    #[test]
    fn legacy_loss_is_split_around_retained_faults_by_delivery_position() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut stream = retained_notifications_from_sender(&sender, faults.clone());
        faults.publish(&sender, note(1, false));
        faults.publish(&sender, note(2, true));
        faults.publish(&sender, note(3, false));
        faults.publish(&sender, note(4, true));
        faults.publish(&sender, note(5, false));
        faults.publish(&sender, note(6, false));
        let first = stream.next().now_or_never().unwrap().unwrap();
        let second = stream.next().now_or_never().unwrap().unwrap();
        let third = stream.next().now_or_never().unwrap().unwrap();
        assert_eq!((first.instance, first.lost_before), (2, 1));
        assert_eq!((second.instance, second.lost_before), (4, 1));
        assert_eq!((third.instance, third.lost_before), (6, 1));
    }

    #[test]
    fn event_lag_is_split_around_retained_faults_by_delivery_position() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut stream = retained_notification_events(&sender, faults.clone());
        for (instance, failure) in [
            (1, false),
            (2, true),
            (3, false),
            (4, true),
            (5, false),
            (6, false),
        ] {
            faults.publish(&sender, note(instance, failure));
        }
        for (loss, instance) in [(2, 2), (2, 4), (1, 6)] {
            assert_eq!(
                stream.next().now_or_never().unwrap(),
                Some(NotificationEvent::Lagged {
                    skipped_records: loss
                })
            );
            assert!(
                matches!(stream.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == instance)
            );
        }
    }

    #[test]
    fn idle_receivers_share_large_overflow_evidence_and_release_it() {
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let streams: Vec<_> = (0..128)
            .map(|_| retained_notifications_from_sender(&sender, faults.clone()))
            .collect();
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        let mut rejected = note(2, true);
        rejected.source_failure = Some(PlatformError::new(
            "native",
            "large",
            "x".repeat(128 * 1024),
        ));
        faults.publish(&sender, rejected);
        let late = retained_notifications_from_sender(&sender, faults.clone());
        let (payloads, shared) = {
            let state = faults.state.lock().unwrap();
            let payloads: Vec<_> = state
                .broadcast_receivers
                .iter()
                .map(|receiver| {
                    let receiver = receiver.upgrade().unwrap();
                    let receiver = receiver.lock().unwrap();
                    let message = &receiver
                        .pending
                        .front()
                        .unwrap()
                        .envelope
                        .notification
                        .source_failure
                        .as_ref()
                        .unwrap()
                        .metadata
                        .iter()
                        .find(|(key, _)| *key == "originalMessage")
                        .unwrap()
                        .1;
                    (message.as_ptr() as usize, message.len())
                })
                .collect();
            (
                payloads,
                std::sync::Arc::downgrade(state.shared_broadcast_terminal.as_ref().unwrap()),
            )
        };
        assert_eq!(payloads.len(), 129);
        assert!(
            payloads
                .iter()
                .all(|&(pointer, len)| pointer == payloads[0].0 && len == 128 * 1024)
        );
        drop(late);
        drop(streams);
        faults.clear_all();
        drop(hold);
        let state = faults.state.lock().unwrap();
        assert!(
            state
                .broadcast_receivers
                .iter()
                .all(|receiver| receiver.upgrade().is_none())
        );
        assert!(state.quota.is_none());
        assert!(shared.upgrade().is_none());
    }

    #[test]
    fn winrt_callback_defers_both_wakes_until_publication_permit_retires() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, NotificationRoute};
        use std::future::Future;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        use std::task::{Context, Poll, Wake, Waker};
        use std::time::Duration;

        struct Reenter {
            source: Arc<Mutex<()>>,
            gate: Arc<callback_gate::CallbackGate>,
            wakes: AtomicUsize,
        }
        impl Wake for Reenter {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                let _source = self.source.lock().unwrap();
                self.gate.set(false);
                self.wakes.fetch_add(1, Ordering::SeqCst);
            }
        }
        let faults = Arc::new(NotificationFaults::new(8));
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        faults.start_source_generation(1).unwrap();
        let (group, consumer) = faults.open_source_group(1, 8, 32).unwrap();
        faults
            .register_source_route(
                &group,
                NotificationRoute {
                    service_uuid: Uuid::nil(),
                    service_instance: 7,
                    characteristic_uuid: Uuid::nil(),
                    characteristic_instance: 1,
                },
                1,
            )
            .unwrap();
        let epoch = faults.begin(&characteristic).unwrap();
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        let gate = Arc::new(callback_gate::CallbackGate::new(true));
        let source = Arc::new(Mutex::new(()));
        let probe = Arc::new(Reenter {
            source: source.clone(),
            gate: gate.clone(),
            wakes: AtomicUsize::new(0),
        });
        let waker = Waker::from(probe.clone());
        let mut cx = Context::from_waker(&waker);
        let mut group_wait = Box::pin(consumer.recv());
        let mut broadcast_wait = Box::pin(receiver.recv());
        assert!(matches!(group_wait.as_mut().poll(&mut cx), Poll::Pending));
        assert!(matches!(
            broadcast_wait.as_mut().poll(&mut cx),
            Poll::Pending
        ));
        let (published_tx, published_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let callback_gate = gate.clone();
        let callback_faults = faults.clone();
        let (completed_tx, completed_rx) = mpsc::channel();
        let callback = std::thread::spawn(move || {
            callback_gate.admit_with_delivery(
                || callback_faults.hold_notification_delivery(),
                || {
                    callback_faults.publish_for_epoch(&sender, note(1, false), epoch);
                    published_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            );
            completed_tx.send(()).unwrap();
        });
        published_rx.recv().unwrap();
        assert_eq!(
            probe.wakes.load(Ordering::SeqCst),
            0,
            "delivery escaped the admitted callback"
        );
        assert!(
            gate.has_admitted_callback(),
            "publication released its callback permit prematurely"
        );
        let (retired_tx, retired_rx) = mpsc::channel();
        let (validated_tx, validated_rx) = mpsc::channel();
        let retired = std::thread::spawn(move || {
            {
                let _source = source.lock().unwrap();
                validated_tx.send(()).unwrap();
            }
            gate.set(false);
            let _source = source.lock().unwrap();
            retired_tx.send(()).unwrap();
        });
        validated_rx.recv().unwrap();
        assert!(retired_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reentrant wake deadlocked callback delivery");
        retired_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("retirement did not finish");
        callback.join().unwrap();
        retired.join().unwrap();
        assert_eq!(probe.wakes.load(Ordering::SeqCst), 2);
        assert!(matches!(
            group_wait.as_mut().poll(&mut cx),
            Poll::Ready(Some(IngressRecord::Value { .. }))
        ));
        assert!(matches!(
            broadcast_wait.as_mut().poll(&mut cx),
            Poll::Ready(Ok(_))
        ));
    }

    #[test]
    fn notification_delivery_hold_bounds_broadcast_and_preserves_prefix() {
        let faults = NotificationFaults::new(2);
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        let hold = faults.hold_notification_delivery();
        for instance in 1..=4 {
            faults.publish(&sender, note(instance, false));
        }
        assert!(receiver.try_recv().is_err());
        drop(hold);
        let first = receiver.try_recv().unwrap();
        let second = receiver.try_recv().unwrap();
        let terminal = receiver.try_recv().unwrap();
        assert_eq!(
            (first.sequence, second.sequence, terminal.sequence),
            (1, 2, 3)
        );
        assert!(
            matches!(&terminal.notification.source_failure, Some(error) if error.code == "notification-broadcast-overflow")
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn held_overflow_survives_graph_clear_for_existing_receivers_only() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(2));
        let (sender, _) = tokio::sync::broadcast::channel(16);
        let mut values = retained_notifications_from_sender(&sender, faults.clone());
        let mut events = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        for instance in 1..=3 {
            faults.publish(&sender, note(instance, false));
        }
        // The same clear is used by the WinRT Services Changed callback.
        faults.clear_all();
        let mut replacement = retained_notification_events(&sender, faults.clone());
        let mut replacement_values = retained_notifications_from_sender(&sender, faults.clone());
        drop(hold);
        for instance in 1..=2 {
            assert_eq!(
                values.next().now_or_never().unwrap().unwrap().instance,
                instance
            );
            assert!(
                matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == instance)
            );
        }
        let terminal = values.next().now_or_never().unwrap().unwrap();
        assert!(
            matches!(terminal.source_failure, Some(error) if error.code == "notification-broadcast-overflow")
        );
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if matches!(value.source_failure, Some(ref error) if error.code == "notification-broadcast-overflow"))
        );
        assert!(replacement.next().now_or_never().is_none());
        assert!(replacement_values.next().now_or_never().is_none());
        faults.publish(&sender, note(4, false));
        assert_eq!(values.next().now_or_never().unwrap(), Some(note(4, false)));
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 4)
        );
        assert_eq!(
            replacement_values.next().now_or_never().unwrap(),
            Some(note(4, false))
        );
        assert!(
            matches!(replacement.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 4)
        );
    }

    #[test]
    fn delayed_pre_subscription_records_do_not_inflate_lag() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        for fresh_count in [1, 3] {
            let faults = std::sync::Arc::new(NotificationFaults::new(8));
            let (sender, _) = tokio::sync::broadcast::channel(2);
            let hold = faults.hold_notification_delivery();
            faults.publish(&sender, note(1, false));
            faults.publish(&sender, note(2, false));
            faults.clear_all();
            let mut values = retained_notifications_from_sender(&sender, faults.clone());
            let mut events = retained_notification_events(&sender, faults.clone());
            drop(hold);
            for instance in 3..3 + fresh_count {
                faults.publish(&sender, note(instance, false));
            }
            let expected_missed = fresh_count.saturating_sub(2);
            if expected_missed > 0 {
                assert_eq!(
                    events.next().now_or_never().unwrap(),
                    Some(NotificationEvent::Lagged {
                        skipped_records: expected_missed
                    })
                );
            }
            let first = values.next().now_or_never().unwrap().unwrap();
            assert_eq!(first.lost_before, expected_missed);
            assert_eq!(first.instance, 3 + expected_missed);
            assert!(
                matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == first.instance)
            );
        }
    }

    #[test]
    fn lag_uses_delivered_positions_when_old_terminal_was_replaced() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(2));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let hold = faults.hold_notification_delivery();
        for instance in 1..=3 {
            faults.publish(&sender, note(instance, false));
        }
        faults.clear_all();
        // Replacing the queued terminal leaves a publication sequence hole.
        faults.publish(&sender, note(4, false));
        faults.clear_all();
        let mut values = retained_notifications_from_sender(&sender, faults.clone());
        let mut events = retained_notification_events(&sender, faults.clone());
        drop(hold);
        for instance in 5..=7 {
            faults.publish(&sender, note(instance, false));
        }
        let first = values.next().now_or_never().unwrap().unwrap();
        assert_eq!((first.instance, first.lost_before), (6, 1));
        assert_eq!(
            events.next().now_or_never().unwrap(),
            Some(NotificationEvent::Lagged { skipped_records: 1 })
        );
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 6)
        );
    }

    #[test]
    fn lag_handles_subscription_during_an_in_flight_send() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        use std::future::Future;
        use std::sync::{Arc, Mutex, mpsc};
        use std::task::{Context, Poll, Wake, Waker};
        use std::time::Duration;
        struct Block {
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl Wake for Block {
            fn wake(self: Arc<Self>) {
                self.entered.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
        }
        let faults = Arc::new(NotificationFaults::new(2));
        let (sender, mut wake_receiver) = tokio::sync::broadcast::channel(2);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let waker = Waker::from(Arc::new(Block {
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        }));
        let mut cx = Context::from_waker(&waker);
        let mut waiting = Box::pin(wake_receiver.recv());
        assert!(matches!(waiting.as_mut().poll(&mut cx), Poll::Pending));
        let hold = faults.hold_notification_delivery();
        for instance in 1..=3 {
            faults.publish(&sender, note(instance, false));
        }
        faults.clear_all();
        std::thread::scope(|scope| {
            let (drained_tx, drained_rx) = mpsc::channel();
            scope.spawn(move || {
                drop(hold);
                drained_tx.send(()).unwrap();
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // The in-flight old send has already reached this waker, but its
            // delivery bookkeeping has not yet returned from send().
            let mut values = retained_notifications_from_sender(&sender, faults.clone());
            let mut events = retained_notification_events(&sender, faults.clone());
            resume_tx.send(()).unwrap();
            drop(waiting);
            drained_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            faults.publish(&sender, note(4, false));
            faults.publish(&sender, note(5, false));
            faults.publish(&sender, note(6, false));
            let first = values.next().now_or_never().unwrap().unwrap();
            assert_eq!((first.instance, first.lost_before), (5, 1));
            assert_eq!(
                events.next().now_or_never().unwrap(),
                Some(NotificationEvent::Lagged { skipped_records: 1 })
            );
            assert!(
                matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 5)
            );
        });
    }

    #[test]
    fn undelivered_overflow_survives_later_broadcast_lag_and_clear() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(2));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let mut events = retained_notification_events(&sender, faults.clone());
        let mut values = retained_notifications_from_sender(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        for instance in 1..=3 {
            faults.publish(&sender, note(instance, false));
        }
        faults.clear_all();
        drop(hold);
        for instance in 4..=8 {
            faults.publish(&sender, note(instance, false));
        }
        let mut found = false;
        for _ in 0..8 {
            match events.next().now_or_never() {
                Some(Some(crate::api::NotificationEvent::Value(value)))
                    if value
                        .source_failure
                        .as_ref()
                        .is_some_and(|e| e.code == "notification-broadcast-overflow") =>
                {
                    found = true;
                    break;
                }
                Some(Some(_)) => {}
                _ => break,
            }
        }
        assert!(
            found,
            "broadcast lag erased the previously accepted overflow terminal"
        );
        assert_eq!(
            events.next().now_or_never().unwrap(),
            Some(crate::api::NotificationEvent::Lagged { skipped_records: 3 })
        );
        assert!(
            matches!(events.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 7)
        );
        let terminal = values.next().now_or_never().unwrap().unwrap();
        assert!(terminal.lost_before > 0);
        assert!(
            terminal
                .source_failure
                .as_ref()
                .is_some_and(|e| e.code == "notification-broadcast-overflow")
        );
        let next = values.next().now_or_never().unwrap().unwrap();
        assert_eq!((next.instance, next.lost_before), (7, 3));
    }

    #[test]
    fn lag_stops_at_each_retained_overflow_even_if_replaced() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        for replaced in [false, true] {
            let faults = std::sync::Arc::new(NotificationFaults::new(2));
            let (sender, _) = tokio::sync::broadcast::channel(2);
            let mut values = retained_notifications_from_sender(&sender, faults.clone());
            let mut events = retained_notification_events(&sender, faults.clone());
            let hold = faults.hold_notification_delivery();
            for instance in 1..=3 {
                faults.publish(&sender, note(instance, false));
            }
            faults.clear_all();
            if replaced {
                faults.publish(&sender, note(4, false));
                faults.clear_all();
            }
            drop(hold);
            for instance in 5..=9 {
                faults.publish(&sender, note(instance, false));
            }
            let first = values.next().now_or_never().unwrap().unwrap();
            assert_eq!(first.lost_before, 2, "legacy loss, replaced={replaced}");
            assert_eq!(first.instance, 3);
            assert!(first.source_failure.is_some());
            assert_eq!(
                events.next().now_or_never().unwrap(),
                Some(NotificationEvent::Lagged {
                    skipped_records: if replaced { 2 } else { 3 }
                }),
                "event loss, replaced={replaced}"
            );
            assert!(
                matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 3 && value.source_failure.is_some())
            );
            if replaced {
                let second = values.next().now_or_never().unwrap().unwrap();
                assert_eq!((second.instance, second.lost_before), (4, 0));
                assert!(second.source_failure.is_some());
                assert_eq!(
                    events.next().now_or_never().unwrap(),
                    Some(NotificationEvent::Lagged { skipped_records: 1 })
                );
                assert!(
                    matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 4 && value.source_failure.is_some())
                );
            }
            let next = values.next().now_or_never().unwrap().unwrap();
            assert_eq!((next.instance, next.lost_before), (8, 3));
            assert_eq!(
                events.next().now_or_never().unwrap(),
                Some(NotificationEvent::Lagged { skipped_records: 3 })
            );
            assert!(
                matches!(events.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 8)
            );
        }
    }

    #[test]
    fn many_idle_receivers_keep_bounded_old_delivery_metadata() {
        use crate::api::NotificationEvent;
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(128));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let hold = faults.hold_notification_delivery();
        for instance in 1..=128 {
            faults.publish(&sender, note(instance, false));
        }
        let mut streams: Vec<_> = (0..128)
            .map(|_| retained_notification_events(&sender, faults.clone()))
            .collect();
        drop(hold);
        {
            let state = faults.state.lock().unwrap();
            assert_eq!(state.broadcast_receivers.len(), 128);
            for receiver in &state.broadcast_receivers {
                assert_eq!(
                    receiver.upgrade().unwrap().lock().unwrap().old_deliveries,
                    Some((1, 128)),
                    "old positions occupy one interval"
                );
            }
        }
        for instance in 129..=131 {
            faults.publish(&sender, note(instance, false));
        }
        for stream in &mut streams {
            assert_eq!(
                stream.next().now_or_never().unwrap(),
                Some(NotificationEvent::Lagged { skipped_records: 1 })
            );
            assert!(
                matches!(stream.next().now_or_never().unwrap(), Some(NotificationEvent::Value(value)) if value.instance == 130)
            );
        }
    }

    #[test]
    fn held_overflows_are_bound_to_each_receiver_generation() {
        use futures::{FutureExt, StreamExt};
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(16);
        let mut old = retained_notification_events(&sender, faults.clone());
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(1, false));
        faults.publish(&sender, note(2, false));
        faults.clear_all();
        let mut next = retained_notification_events(&sender, faults.clone());
        faults.publish(&sender, note(3, false));
        faults.clear_all();
        let mut newest = retained_notification_events(&sender, faults.clone());
        drop(hold);
        assert!(
            matches!(old.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 1)
        );
        assert!(
            matches!(old.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 2 && value.source_failure.is_some())
        );
        assert!(
            matches!(old.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 3 && value.source_failure.is_some())
        );
        assert!(old.next().now_or_never().is_none());
        assert!(
            matches!(next.next().now_or_never().unwrap(), Some(crate::api::NotificationEvent::Value(value)) if value.instance == 3 && value.source_failure.is_some())
        );
        assert!(next.next().now_or_never().is_none());
        assert!(newest.next().now_or_never().is_none());
    }

    #[test]
    fn notification_delivery_hold_stops_an_existing_broadcast_drainer() {
        use std::future::Future;
        use std::sync::{Arc, Mutex, mpsc};
        use std::task::{Context, Poll, Wake, Waker};
        struct Block {
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl Wake for Block {
            fn wake(self: Arc<Self>) {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
        }
        let faults = Arc::new(NotificationFaults::new(4));
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let waker = Waker::from(Arc::new(Block {
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        }));
        let mut cx = Context::from_waker(&waker);
        let mut wait = Box::pin(receiver.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        let other = faults.clone();
        let other_sender = sender.clone();
        let drainer = std::thread::spawn(move || other.publish(&other_sender, note(1, false)));
        entered_rx.recv().unwrap();
        let hold = faults.hold_notification_delivery();
        faults.publish(&sender, note(2, false));
        resume_tx.send(()).unwrap();
        drainer.join().unwrap();
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
        drop(wait);
        assert!(receiver.try_recv().is_err());
        drop(hold);
        assert_eq!(receiver.try_recv().unwrap().notification.value, vec![2]);
    }

    #[test]
    fn broadcast_reentrant_and_concurrent_publishers_preserve_order_without_source_lock() {
        use std::future::Future;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::task::{Context, Poll, Wake, Waker};
        struct Reenter {
            source: Arc<NotificationFaults>,
            sender: tokio::sync::broadcast::Sender<NotificationEnvelope>,
            unlocked: AtomicBool,
        }
        impl Wake for Reenter {
            fn wake(self: Arc<Self>) {
                self.unlocked
                    .store(self.source.state.try_lock().is_ok(), Ordering::SeqCst);
                self.source.publish(&self.sender, note(2, false));
            }
        }
        let source = Arc::new(NotificationFaults::new(16));
        let (sender, mut receiver) = tokio::sync::broadcast::channel(32);
        let waker_state = Arc::new(Reenter {
            source: source.clone(),
            sender: sender.clone(),
            unlocked: AtomicBool::new(false),
        });
        let waker = Waker::from(waker_state.clone());
        let mut cx = Context::from_waker(&waker);
        let mut recv = Box::pin(receiver.recv());
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Pending));
        source.publish(&sender, note(1, false));
        assert!(waker_state.unlocked.load(Ordering::SeqCst));
        let Poll::Ready(Ok(first)) = recv.as_mut().poll(&mut cx) else {
            panic!("first notification absent")
        };
        drop(recv);
        let second = receiver.try_recv().unwrap();
        assert_eq!((first.sequence, second.sequence), (1, 2));
        let a = source.clone();
        let b = source.clone();
        let sa = sender.clone();
        let sb = sender.clone();
        let left = std::thread::spawn(move || a.publish(&sa, note(3, false)));
        let right = std::thread::spawn(move || b.publish(&sb, note(4, false)));
        left.join().unwrap();
        right.join().unwrap();
        let third = receiver.try_recv().unwrap();
        let fourth = receiver.try_recv().unwrap();
        assert_eq!((third.sequence, fourth.sequence), (3, 4));
    }

    #[test]
    fn broadcast_queue_overflow_retains_explicit_terminal_after_accepted_prefix() {
        use std::future::Future;
        use std::sync::{Arc, Mutex, mpsc};
        use std::task::{Context, Poll, Wake, Waker};
        struct Block {
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl Wake for Block {
            fn wake(self: Arc<Self>) {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
        }
        let source = Arc::new(NotificationFaults::new(1));
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let waker = Waker::from(Arc::new(Block {
            entered: entered_tx,
            resume: Mutex::new(resume_rx),
        }));
        let mut cx = Context::from_waker(&waker);
        let mut recv = Box::pin(receiver.recv());
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Pending));
        let other = source.clone();
        let other_sender = sender.clone();
        let worker = std::thread::spawn(move || other.publish(&other_sender, note(1, false)));
        entered_rx.recv().unwrap();
        source.publish(&sender, note(2, false));
        source.publish(&sender, note(3, false));
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        let Poll::Ready(Ok(first)) = recv.as_mut().poll(&mut cx) else {
            panic!("accepted prefix absent")
        };
        drop(recv);
        let second = receiver.try_recv().unwrap();
        let terminal = receiver.try_recv().unwrap();
        assert_eq!(
            (first.sequence, second.sequence, terminal.sequence),
            (1, 2, 3)
        );
        assert!(
            matches!(&terminal.notification.source_failure, Some(error) if error.code == "notification-broadcast-overflow")
        );
    }

    pub(super) fn note(instance: u64, failure: bool) -> ValueNotification {
        ValueNotification {
            uuid: Uuid::nil(),
            instance,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            value: vec![instance as u8],
            lost_before: 0,
            source_failure: failure
                .then(|| PlatformError::new("corebluetooth", "17", "original NSError")),
        }
    }

    #[test]
    fn legacy_publish_checks_epoch_without_admitting_group_values() {
        use crate::ubm_peer_notification_ingress::{NotificationRoute, Poll};
        let ingress = NotificationFaults::new(4);
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        let route = NotificationRoute {
            service_uuid: Uuid::nil(),
            service_instance: 7,
            characteristic_uuid: Uuid::nil(),
            characteristic_instance: 1,
        };
        ingress.start_source_generation(1).unwrap();
        let (group, consumer) = ingress.open_source_group(1, 8, 32).unwrap();
        ingress.register_source_route(&group, route, 1).unwrap();
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        let stale = ingress.begin(&characteristic).unwrap();
        let current = ingress.begin(&characteristic).unwrap();
        ingress.publish_legacy_for_epoch(&sender, note(1, false), stale);
        assert!(receiver.try_recv().is_err());
        assert_eq!(consumer.poll(), Poll::Empty);
        ingress.publish_legacy_for_epoch(&sender, note(1, false), current);
        assert_eq!(receiver.try_recv().unwrap().notification.value, vec![1]);
        assert!(receiver.try_recv().is_err());
        assert_eq!(consumer.poll(), Poll::Empty);
        assert!(ingress.clear_for_epoch(&characteristic, current));
        ingress.publish_legacy_for_epoch(&sender, note(1, false), current);
        assert!(receiver.try_recv().is_err());
        assert_eq!(consumer.poll(), Poll::Empty);
    }

    #[test]
    fn epoch_retirement_preserves_replacement_fault_and_rejects_old_callbacks() {
        let ingress = NotificationFaults::new(4);
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        let old = ingress.begin(&characteristic).unwrap();
        assert!(ingress.clear_for_epoch(&characteristic, old));
        let current = ingress.begin(&characteristic).unwrap();
        ingress.publish_for_epoch(&sender, note(1, true), current);
        assert!(!ingress.clear_for_epoch(&characteristic, old));
        {
            let state = ingress.state.lock().unwrap();
            let key = NotificationFaults::key(&note(1, true));
            assert_eq!(state.enable_epochs.get(&key), Some(&current));
            assert!(state.faults.contains_key(&key));
            assert!(state.direct_faults.contains_key(&key));
        }
        assert!(ingress.clear_for_epoch(&characteristic, current));
        assert!(!ingress.clear_for_epoch(&characteristic, current));
        ingress.publish_for_epoch(&sender, note(1, false), current);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        let state = ingress.state.lock().unwrap();
        let key = NotificationFaults::key(&note(1, true));
        assert!(!state.enable_epochs.contains_key(&key));
        assert!(!state.faults.contains_key(&key));
        assert!(!state.direct_faults.contains_key(&key));
    }

    #[test]
    fn publication_during_subscription_is_not_filtered_by_snapshot() {
        use crate::api::NotificationEvent;
        let ingress = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let publisher_ingress = ingress.clone();
        let publisher_sender = sender.clone();
        let mut thread = None;
        let mut stream = retained_notification_events_with_hook(&sender, ingress.clone(), || {
            assert!(ingress.state.try_lock().is_err());
            thread = Some(std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                publisher_ingress.publish(&publisher_sender, note(1, false));
            }));
            started_rx.recv().unwrap();
        });
        thread.unwrap().join().unwrap();
        assert_eq!(
            futures::executor::block_on(stream.next()),
            Some(NotificationEvent::Value(note(1, false)))
        );
    }

    #[test]
    fn legacy_subscription_registers_before_publication_can_run() {
        let faults = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, _) = tokio::sync::broadcast::channel(4);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let publisher_faults = faults.clone();
        let publisher_sender = sender.clone();
        let mut worker = None;
        let mut stream = retained_notifications_with_receiver(faults.clone(), || {
            let receiver = sender.subscribe();
            assert!(faults.state.try_lock().is_err());
            worker = Some(std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let hold = publisher_faults.hold_notification_delivery();
                publisher_faults.publish(&publisher_sender, note(1, false));
                publisher_faults.publish(&publisher_sender, note(2, false));
                publisher_faults.clear_all();
                drop(hold);
            }));
            started_rx.recv().unwrap();
            receiver
        });
        worker.unwrap().join().unwrap();
        assert_eq!(
            futures::executor::block_on(stream.next()).unwrap().instance,
            1
        );
        let terminal = futures::executor::block_on(stream.next()).unwrap();
        assert!(
            terminal
                .source_failure
                .is_some_and(|error| error.code == "notification-broadcast-overflow")
        );
        use futures::FutureExt;
        assert!(stream.next().now_or_never().is_none());
    }

    #[test]
    fn queued_terminal_is_revalidated_after_lag_delivery() {
        use crate::api::NotificationEvent;
        let ingress = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notification_events(&sender, ingress.clone());
        ingress.publish(&sender, note(1, true));
        ingress.publish(&sender, note(2, false));
        ingress.publish(&sender, note(3, true));
        ingress.publish(&sender, note(4, false));
        assert_eq!(
            futures::executor::block_on(stream.next()),
            Some(NotificationEvent::Lagged { skipped_records: 1 })
        );
        ingress.clear_key((Uuid::nil(), 7, Uuid::nil(), 1));
        ingress.clear_key((Uuid::nil(), 7, Uuid::nil(), 3));
        assert_eq!(
            futures::executor::block_on(stream.next()),
            Some(NotificationEvent::Lagged { skipped_records: 1 })
        );
        assert_eq!(
            futures::executor::block_on(stream.next()),
            Some(NotificationEvent::Value(note(4, false)))
        );
    }

    #[test]
    fn ordered_gap_survives_stale_controls_and_close() {
        use crate::api::NotificationEvent;
        let ingress = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let stream = retained_notification_events(&sender, ingress.clone());
        ingress.publish(&sender, note(1, false));
        ingress.publish(&sender, note(2, false));
        ingress.publish(&sender, note(3, true));
        ingress.publish(&sender, note(4, true));
        ingress.clear_key((Uuid::nil(), 7, Uuid::nil(), 3));
        ingress.clear_key((Uuid::nil(), 7, Uuid::nil(), 4));
        drop(sender);
        let received = futures::executor::block_on(stream.collect::<Vec<_>>());
        assert_eq!(
            received,
            vec![NotificationEvent::Lagged { skipped_records: 2 }]
        );
    }

    #[test]
    fn ordered_gaps_report_records_and_replay_retained_controls() {
        use crate::api::NotificationEvent;
        let ingress = std::sync::Arc::new(NotificationFaults::new(8));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notification_events(&sender, ingress.clone());
        ingress.publish(&sender, note(1, true));
        ingress.publish(&sender, note(2, false));
        ingress.publish(&sender, note(3, false));
        let first = futures::executor::block_on(stream.next()).unwrap();
        assert_eq!(first, NotificationEvent::Lagged { skipped_records: 1 });
        let replay = futures::executor::block_on(stream.next()).unwrap();
        assert_eq!(replay, NotificationEvent::Value(note(1, true)));
        let _ = futures::executor::block_on(stream.next());
        let _ = futures::executor::block_on(stream.next());
        ingress.publish(&sender, note(4, false));
        ingress.publish(&sender, note(5, false));
        ingress.publish(&sender, note(6, false));
        assert_eq!(
            futures::executor::block_on(stream.next()),
            Some(NotificationEvent::Lagged { skipped_records: 1 })
        );
    }

    #[test]
    fn another_attribute_flood_cannot_overwrite_the_original_fault() {
        for original in [
            PlatformError::new("corebluetooth", "17", "original NSError")
                .with("nsErrorDomain", "CBATTErrorDomain"),
            PlatformError::new("windows-hresult", "0x800710DF", "original HRESULT")
                .with("operation", "gatt.notification.value"),
        ] {
            let ingress = std::sync::Arc::new(NotificationFaults::new(4));
            let (sender, receiver) = tokio::sync::broadcast::channel(2);
            let mut stream = retained_notifications(receiver, ingress.clone());
            let mut fault = note(1, true);
            fault.source_failure = Some(original.clone());
            ingress.publish(&sender, fault);
            for _ in 0..5 {
                ingress.publish(&sender, note(2, false));
            }
            let first = futures::executor::block_on(stream.next()).expect("retained terminal");
            assert_eq!(first.instance, 1);
            assert_eq!(first.service_instance, 7);
            assert_eq!(first.source_failure.expect("original fault"), original);
            assert_eq!(first.lost_before, 0);
            let next = futures::executor::block_on(stream.next()).expect("surviving value");
            assert_eq!((next.instance, next.lost_before), (2, 3));
        }
    }

    #[test]
    fn confirmed_disable_and_new_receiver_do_not_replay_an_old_fault() {
        let ingress = std::sync::Arc::new(NotificationFaults::new(4));
        let (sender, _) = tokio::sync::broadcast::channel(2);
        ingress.publish(&sender, note(1, true));
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        ingress.clear(&characteristic);
        let mut stream = retained_notifications(sender.subscribe(), ingress.clone());
        for _ in 0..5 {
            ingress.publish(&sender, note(2, false));
        }
        let first = futures::executor::block_on(stream.next()).expect("new source");
        assert_eq!(first.instance, 2);
        assert!(first.source_failure.is_none());
    }

    #[test]
    fn queued_callback_from_retired_enable_cannot_poison_new_enable() {
        let ingress = std::sync::Arc::new(NotificationFaults::new(4));
        let characteristic = Characteristic {
            uuid: Uuid::nil(),
            instance: 1,
            service_uuid: Uuid::nil(),
            service_instance: 7,
            properties: Default::default(),
            descriptors: Default::default(),
        };
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notifications(receiver, ingress.clone());
        let old = ingress.begin(&characteristic).expect("enable");
        ingress.publish_for_epoch(&sender, note(1, true), old);
        ingress.clear(&characteristic);
        let fresh = ingress.begin(&characteristic).expect("fresh enable");
        ingress.publish_for_epoch(&sender, note(1, true), old);
        ingress.publish_for_epoch(&sender, note(1, false), fresh);
        let first = futures::executor::block_on(stream.next()).expect("fresh value");
        assert!(first.source_failure.is_none());
        assert_eq!(first.instance, 1);
    }

    #[test]
    fn retained_fault_capacity_is_bounded_and_explicitly_fail_closed() {
        let ingress = std::sync::Arc::new(NotificationFaults::new(1));
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let mut stream = retained_notifications(receiver, ingress.clone());
        ingress.publish(&sender, note(1, true));
        ingress.publish(&sender, note(2, true));
        ingress.publish(&sender, note(3, false));
        assert_eq!(ingress.state.lock().unwrap().faults.len(), 1);
        let first = futures::executor::block_on(stream.next()).expect("first fault");
        assert_eq!(first.instance, 1);
        let quota = futures::executor::block_on(stream.next()).expect("quota terminal");
        assert_eq!(
            quota.source_failure.expect("failure").code,
            "notification-fault-quota"
        );
    }
}
