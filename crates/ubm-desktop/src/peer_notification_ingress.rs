// crates/ubm-desktop/src/peer_notification_ingress.rs
//! Desktop adapter and pump for the native source owner.

pub use btleplug::ubm_peer_notification_ingress::{
    CallbackProvenance, IngressError, IngressRecord, IngressTransfer, Membership,
    NotificationRoute, PeerNotificationIngress, Poll, RejectedRecord, TerminalReason,
};

/// Convert the existing desktop route without creating another state owner.
pub fn native_route(route: crate::btleplug_backend::NotificationRoute) -> NotificationRoute {
    NotificationRoute {
        service_uuid: route.service_uuid,
        service_instance: route.service_instance,
        characteristic_uuid: route.characteristic_uuid,
        characteristic_instance: route.characteristic_instance,
    }
}

use crate::btleplug_backend::{ForwardTarget, WithOs};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::{oneshot, watch};

struct Member {
    membership: Membership,
    target: ForwardTarget,
    retired: bool,
    fences: Vec<oneshot::Sender<()>>,
}

/// Serializes the host mapping with native route registration and retirement.
/// The source can enqueue a callback as soon as registration returns, so the
/// pump takes the same lock before consulting the mapping.
#[derive(Default)]
pub(crate) struct Members {
    entries: Mutex<HashMap<(u64, u64), Member>>,
    // A source transfer remains owned until ordinary fanout also completes.
    // Keep the exact allocation, not its bytes or ordinal: equal payloads are
    // distinct occurrences, and a restarted pump must not admit this one twice.
    admitted_occurrence: Mutex<Option<Arc<IngressRecord>>>,
    closed: std::sync::atomic::AtomicBool,
    retirement_reason: Mutex<Option<ubm_core::ordered_notifications::EndReason>>,
}

impl Members {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    /// Hold the pump's mapping lock across a complete source-registration and
    /// registry-install batch. Rollback retains registered mappings until fence.
    pub(crate) fn batch<R>(&self, apply: impl FnOnce(&mut MemberBatch<'_>) -> R) -> R {
        let mut members = self.entries.lock().expect("peer ingress members");
        apply(&mut MemberBatch {
            members: &mut members,
            closed: &self.closed,
        })
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "The single-route registration helper remains covered by source-owner tests alongside batch registration."
        )
    )]
    pub(crate) fn register<E>(
        &self,
        target: ForwardTarget,
        register: impl FnOnce() -> Result<Membership, E>,
    ) -> Result<Membership, E> {
        self.batch(|batch| batch.register(target, register))
    }

    pub(crate) fn target(&self, membership: Membership) -> Option<ForwardTarget> {
        self.entries
            .lock()
            .expect("peer ingress members")
            .get(&(membership.member, membership.epoch))
            .filter(|member| member.membership == membership)
            .map(|member| member.target.clone())
    }

    /// Resolve a source-time snapshot without substituting later members.
    pub(crate) fn snapshot(
        &self,
        snapshot: &[(NotificationRoute, Membership)],
    ) -> Option<Vec<(crate::boundary::InstanceKey, u64)>> {
        let members = self.entries.lock().expect("peer ingress members");
        snapshot
            .iter()
            .map(|(_, membership)| {
                let member = members.get(&(membership.member, membership.epoch))?;
                let target = &member.target;
                Some((
                    (
                        target.peer_id.clone(),
                        target.service_uuid.clone(),
                        target.service_occurrence,
                        target.characteristic_uuid.clone(),
                        target.characteristic_occurrence,
                    ),
                    target.epoch,
                ))
            })
            .collect()
    }

    /// Installs the fence waiter before the native source may enqueue it.
    pub(crate) fn retire<E>(
        &self,
        membership: Membership,
        retire: impl FnOnce() -> Result<(), E>,
    ) -> Result<Option<oneshot::Receiver<()>>, E> {
        self.batch(|batch| batch.retire(membership, retire))
    }

    pub(crate) fn set_retirement_reason(
        &self,
        reason: ubm_core::ordered_notifications::EndReason,
    ) -> Result<(), DesktopError> {
        let mut current = self.retirement_reason.lock().expect("retirement reason");
        if current.as_ref().is_some_and(|existing| *existing != reason) {
            return Err(pump_failure("ordered source retirement reason changed"));
        }
        *current = Some(reason);
        Ok(())
    }

    /// A terminal group cannot deliver further fences; fail all waiters.
    pub(crate) fn close(&self) {
        let mut members = self.entries.lock().expect("peer ingress members");
        self.closed.store(true, Ordering::Release);
        members.clear();
    }

    fn fail_snapshot(&self) -> (Vec<Membership>, Vec<(crate::boundary::InstanceKey, u64)>) {
        let members = self.entries.lock().expect("peer ingress members");
        self.closed.store(true, Ordering::Release);
        let mut native = Vec::new();
        let mut targets = Vec::new();
        for member in members.values() {
            native.push(member.membership);
            let target = &member.target;
            targets.push((
                (
                    target.peer_id.clone(),
                    target.service_uuid.clone(),
                    target.service_occurrence,
                    target.characteristic_uuid.clone(),
                    target.characteristic_occurrence,
                ),
                target.epoch,
            ));
        }
        (native, targets)
    }

    /// A membership stays resolvable until its ordered fence is consumed.
    pub(crate) fn fence(&self, membership: Membership) -> bool {
        let mut members = self.entries.lock().expect("peer ingress members");
        if !members
            .get(&(membership.member, membership.epoch))
            .is_some_and(|member| member.membership == membership)
        {
            return false;
        }
        let member = members
            .remove(&(membership.member, membership.epoch))
            .expect("checked membership");
        for fence in member.fences {
            let _ = fence.send(());
        }
        true
    }
}

pub(crate) struct MemberBatch<'a> {
    members: &'a mut HashMap<(u64, u64), Member>,
    closed: &'a std::sync::atomic::AtomicBool,
}

impl MemberBatch<'_> {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn register<E>(
        &mut self,
        target: ForwardTarget,
        register: impl FnOnce() -> Result<Membership, E>,
    ) -> Result<Membership, E> {
        let membership = register()?;
        self.members.insert(
            (membership.member, membership.epoch),
            Member {
                membership,
                target,
                retired: false,
                fences: Vec::new(),
            },
        );
        Ok(membership)
    }

    pub(crate) fn retire<E>(
        &mut self,
        membership: Membership,
        retire: impl FnOnce() -> Result<(), E>,
    ) -> Result<Option<oneshot::Receiver<()>>, E> {
        let Some(member) = self
            .members
            .get_mut(&(membership.member, membership.epoch))
            .filter(|member| member.membership == membership)
        else {
            return Ok(None);
        };
        let (tx, rx) = oneshot::channel();
        if !member.retired {
            retire()?;
            member.retired = true;
        }
        member.fences.push(tx);
        Ok(Some(rx))
    }
}

/// A source record resolved against the membership mapping that was installed
/// before native admission. No per-characteristic queues are introduced.
#[derive(Debug)]
pub(crate) enum MappedRecord {
    Value {
        membership: Membership,
        ordinal: u64,
        target: ForwardTarget,
        provenance: CallbackProvenance,
        value: btleplug::api::ValueNotification,
    },
    Loss {
        memberships: Vec<Membership>,
        members: Vec<(crate::boundary::InstanceKey, u64)>,
        skipped_records: u64,
    },
    Fence {
        membership: Membership,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Fence routing is asserted by the source-owner test route."
            )
        )]
        target: ForwardTarget,
    },
    Barrier {
        id: u64,
    },
    Marker {
        id: u64,
        ordinal: u64,
    },
    FailureOccurrence {
        memberships: Vec<Membership>,
        members: Vec<(crate::boundary::InstanceKey, u64)>,
        error: btleplug::ubm_peer_notification_ingress::SharedPlatformError,
    },
    Terminal {
        memberships: Vec<Membership>,
        members: Vec<(crate::boundary::InstanceKey, u64)>,
        reason: TerminalReason,
    },
}

/// Resolve only identities carried by the source record. A missing mapping is
/// a source-contract failure; it must not be converted into silent loss.
pub(crate) fn map_record(members: &Members, record: IngressRecord) -> Option<MappedRecord> {
    match record {
        IngressRecord::Value {
            ordinal,
            membership,
            provenance,
            value,
        } => Some(MappedRecord::Value {
            membership,
            ordinal,
            target: members.target(membership)?,
            provenance,
            value,
        }),
        IngressRecord::Loss {
            skipped_records,
            memberships,
        } => Some(MappedRecord::Loss {
            members: members.snapshot(&memberships)?,
            memberships: memberships
                .iter()
                .map(|(_, membership)| *membership)
                .collect(),
            skipped_records,
        }),
        IngressRecord::Fence { membership } => Some(MappedRecord::Fence {
            membership,
            target: members.target(membership)?,
        }),
        IngressRecord::Barrier { id } => Some(MappedRecord::Barrier { id }),
        IngressRecord::Marker { id, ordinal } => Some(MappedRecord::Marker { id, ordinal }),
        IngressRecord::FailureOccurrence { error, memberships } => {
            // Check the same source-time mapping as terminal and loss records.
            let source_members = members.snapshot(&memberships)?;
            Some(MappedRecord::FailureOccurrence {
                memberships: memberships
                    .iter()
                    .map(|(_, membership)| *membership)
                    .collect(),
                members: source_members,
                error,
            })
        }
        IngressRecord::Terminal {
            reason,
            memberships,
        } => Some(MappedRecord::Terminal {
            members: members.snapshot(&memberships)?,
            memberships: memberships
                .iter()
                .map(|(_, membership)| *membership)
                .collect(),
            reason,
        }),
    }
}

use crate::boundary::{PeerSourceFenceAck, RadioEvent};
use crate::errors::DesktopError;
use crate::errors::DesktopErrorEvidence;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

pub(crate) type BarrierAcks = Mutex<HashMap<u64, PeerSourceFenceAck>>;

/// A radio-owned lease store for exact peer-source failure heads. It is
/// injected from `BtleplugRadio`; no process-global recovery authority exists.
pub(crate) struct PeerFailureRecoveryStore {
    host: [u8; 16],
    next: AtomicU64,
    entries: Mutex<HashMap<[u8; 16], PeerFailureRecoveryEntry>>,
}

/// Owns a published-or-pending store entry while the pump awaits central.  A
/// cancelled pump must not strand an entry (and its retained source error) in
/// the radio map; dropping this lease removes only the recovery capability,
/// never acknowledges or advances the original source head.
pub(crate) struct PeerFailureRecoveryOffer {
    store: Arc<PeerFailureRecoveryStore>,
    descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    teardown: DesktopErrorEvidence,
    armed: bool,
}

fn failure_occurrence_teardown(
    error: &btleplug::ubm_peer_notification_ingress::SharedPlatformError,
) -> DesktopErrorEvidence {
    let native = btleplug::Error::Platform(error.as_arc().as_ref().clone());
    DesktopErrorEvidence::from(pump_failure("peer notification source failed").with_os(&native))
}

impl PeerFailureRecoveryOffer {
    pub(crate) fn descriptor(&self) -> ubm_core::oversized_evidence::PeerSourceFailureDescriptor {
        self.descriptor
    }

    pub(crate) fn teardown(&self) -> DesktopErrorEvidence {
        self.store
            .entries
            .lock()
            .expect("peer source recovery store")
            .get(&self.descriptor.identity.recovery)
            .filter(|entry| entry.descriptor == self.descriptor)
            .map(|entry| entry.teardown.clone())
            .unwrap_or_else(|| self.teardown.clone())
    }

    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PeerFailureRecoveryOffer {
    fn drop(&mut self) {
        if self.armed {
            self.store.discard(&self.descriptor);
        }
    }
}

struct PeerFailureRecoveryEntry {
    descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    recovery: btleplug::ubm_peer_notification_ingress::FailureRecovery,
    teardown: DesktopErrorEvidence,
    /// Private durable-commit receipt. It is deliberately separate from the
    /// public descriptor: possessing a descriptor can read bounded evidence,
    /// but only central's persisted transaction may advance and retire its
    /// native source head.
    source_ack_id: String,
    completed: watch::Sender<bool>,
}

fn opaque_recovery_id(value: u64) -> [u8; 16] {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&value.to_be_bytes());
    id
}

impl PeerFailureRecoveryStore {
    pub(crate) fn new(host: u64) -> Self {
        Self {
            host: opaque_recovery_id(host),
            next: AtomicU64::new(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn offer(
        self: &Arc<Self>,
        generation: u64,
        native_group: u64,
        recovery: btleplug::ubm_peer_notification_ingress::FailureRecovery,
        teardown: DesktopErrorEvidence,
    ) -> Result<PeerFailureRecoveryOffer, DesktopError> {
        let transfer = self.next.fetch_add(1, Ordering::Relaxed);
        let recovery_id = self.next.fetch_add(1, Ordering::Relaxed);
        if transfer == u64::MAX || recovery_id == u64::MAX {
            return Err(pump_failure(
                "peer source failure recovery identity exhausted",
            ));
        }
        let descriptor = ubm_core::oversized_evidence::PeerSourceFailureDescriptor {
            protocol: 1,
            representation: ubm_core::oversized_evidence::PEER_SOURCE_FAILURE_REPRESENTATION,
            identity: ubm_core::oversized_evidence::PeerSourceFailureIdentity {
                host: self.host,
                peer: opaque_recovery_id(transfer),
                source_generation: opaque_recovery_id(generation),
                native_group: opaque_recovery_id(native_group),
                transfer: opaque_recovery_id(transfer),
                recovery: opaque_recovery_id(recovery_id),
            },
            encoded_length: u64::try_from(recovery.encoded_len())
                .map_err(|_| pump_failure("peer source failure recovery length overflow"))?,
            chunk_bytes: u32::try_from(
                btleplug::ubm_peer_notification_ingress::FAILURE_RECOVERY_MAX_CHUNK,
            )
            .expect("chunk bound fits u32"),
        };
        let mut source_ack = [0_u8; 16];
        getrandom::fill(&mut source_ack).map_err(|_| {
            pump_failure("peer source failure acknowledgement receipt entropy is unavailable")
        })?;
        let source_ack_id = source_ack
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let (completed, _) = watch::channel(false);
        let mut entries = self.entries.lock().expect("peer source recovery store");
        if entries
            .insert(
                descriptor.identity.recovery,
                PeerFailureRecoveryEntry {
                    descriptor,
                    recovery,
                    teardown: teardown.clone(),
                    source_ack_id,
                    completed,
                },
            )
            .is_some()
        {
            return Err(pump_failure(
                "peer source failure recovery identity collision",
            ));
        }
        Ok(PeerFailureRecoveryOffer {
            store: Arc::clone(self),
            descriptor,
            teardown,
            armed: true,
        })
    }

    fn discard(&self, descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor) {
        let mut entries = self.entries.lock().expect("peer source recovery store");
        if entries
            .get(&descriptor.identity.recovery)
            .is_some_and(|entry| entry.descriptor == *descriptor)
        {
            entries.remove(&descriptor.identity.recovery);
        }
    }

    pub(crate) fn read(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        offset: u64,
    ) -> Result<ubm_core::oversized_evidence::PeerSourceFailureChunk, DesktopError> {
        let offset = usize::try_from(offset)
            .map_err(|_| pump_failure("peer source recovery offset overflow"))?;
        let entries = self.entries.lock().expect("peer source recovery store");
        let entry = entries
            .get(&descriptor.identity.recovery)
            .filter(|entry| entry.descriptor == *descriptor)
            .ok_or_else(|| pump_failure("stale or foreign peer source recovery descriptor"))?;
        let bytes = entry
            .recovery
            .read(offset, entry.descriptor.chunk_bytes as usize)
            .ok_or_else(|| pump_failure("invalid peer source recovery offset"))?;
        let next = offset
            .checked_add(bytes.len())
            .ok_or_else(|| pump_failure("peer source recovery offset overflow"))?;
        Ok(ubm_core::oversized_evidence::PeerSourceFailureChunk {
            offset: offset as u64,
            next_offset: next as u64,
            done: next == entry.recovery.encoded_len(),
            bytes,
        })
    }

    /// Advance the native source exactly once after central has durably staged
    /// every source-time recipient. The returned opaque receipt is retained
    /// until the durable owner commits that advance; it must never be exposed
    /// to public V2 consumers.
    pub(crate) fn begin_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<String, DesktopError> {
        let receipt = self.prepare_ack(descriptor)?;
        self.begin_ack_with_receipt(descriptor, &receipt)
    }

    /// Return the exact private receipt a durable journal must persist before
    /// the native head advances.  This does not acknowledge the source: a
    /// crash after this call leaves the FIFO head and exact evidence intact.
    pub(crate) fn prepare_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<String, DesktopError> {
        let entries = self.entries.lock().expect("peer source recovery store");
        let entry = entries
            .get(&descriptor.identity.recovery)
            .filter(|entry| entry.descriptor == *descriptor)
            .ok_or_else(|| pump_failure("stale or foreign peer source recovery descriptor"))?;
        if !entry.recovery.ready_for_begin_ack() {
            return Err(pump_failure(
                "peer source recovery was not completely read or is stale",
            ));
        }
        Ok(entry.source_ack_id.clone())
    }

    /// Advance the source head only for the receipt that a durable owner has
    /// already committed.  Repeating the same receipt is intentionally
    /// idempotent so recovery can resume after a lost boundary response.
    pub(crate) fn begin_ack_with_receipt(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
    ) -> Result<String, DesktopError> {
        let entries = self.entries.lock().expect("peer source recovery store");
        let entry = entries
            .get(&descriptor.identity.recovery)
            .filter(|entry| entry.descriptor == *descriptor)
            .ok_or_else(|| pump_failure("stale or foreign peer source recovery descriptor"))?;
        if entry.source_ack_id != source_ack_id || !entry.recovery.begin_ack() {
            return Err(pump_failure(
                "peer source recovery acknowledgement receipt is stale or not durable",
            ));
        }
        Ok(entry.source_ack_id.clone())
    }

    /// Release a source only after its exact begin-ACK receipt is committed
    /// durably. A mismatched or stale receipt leaves the retained evidence and
    /// source completion untouched.
    pub(crate) fn retire_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
    ) -> Result<(), DesktopError> {
        let mut entries = self.entries.lock().expect("peer source recovery store");
        let entry = entries
            .get(&descriptor.identity.recovery)
            .filter(|entry| entry.descriptor == *descriptor)
            .ok_or_else(|| pump_failure("stale or foreign peer source recovery descriptor"))?;
        if entry.source_ack_id != source_ack_id {
            return Err(pump_failure(
                "peer source recovery acknowledgement receipt is stale or not durable",
            ));
        }
        // This is the vendor-owned recovery handle obtained from the exact
        // `SourceGroupConsumer`, not a desktop copy.  Keep a failure from its
        // native finalizer observable and leave this entry in place for the
        // journal's retry: reducing `try_retire_ack` to its legacy boolean
        // would make a failed evidence deletion indistinguishable from a
        // stale receipt.
        match entry.recovery.try_retire_ack().map_err(|error| {
            pump_failure(format!(
                "peer source recovery native finalizer could not retire durable evidence: {error}"
            ))
        })? {
            true => {}
            false => {
                return Err(pump_failure(
                    "peer source recovery acknowledgement receipt is stale or not durable",
                ));
            }
        }
        // Wake only after durable commit and native retirement. The source
        // pump may then advance to the next occurrence.
        entry.completed.send_replace(true);
        entries.remove(&descriptor.identity.recovery);
        Ok(())
    }

    /// Compatibility helper for internal callers that have no journal owner.
    /// New central recovery paths must use `begin_ack` and `retire_ack` around
    /// the durable occurrence transaction.
    pub(crate) fn acknowledge(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<(), DesktopError> {
        let receipt = self.begin_ack(descriptor)?;
        self.retire_ack(descriptor, &receipt)
    }

    /// Subscribe before publishing the offer.  The central may stage, read,
    /// and ACK immediately; obtaining this receiver after publication would
    /// race entry removal and incorrectly report a completed source cut stale.
    pub(crate) fn acknowledgement_receiver(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> Result<watch::Receiver<bool>, DesktopError> {
        self.entries
            .lock()
            .expect("peer source recovery store")
            .get(&descriptor.identity.recovery)
            .filter(|entry| entry.descriptor == *descriptor)
            .map(|entry| entry.completed.subscribe())
            .ok_or_else(|| pump_failure("stale or foreign peer source recovery descriptor"))
    }

    pub(crate) async fn wait_for_ack(
        mut completed: watch::Receiver<bool>,
    ) -> Result<(), DesktopError> {
        loop {
            if *completed.borrow_and_update() {
                return Ok(());
            }
            completed.changed().await.map_err(|_| {
                pump_failure("peer source failure recovery was withdrawn before acknowledgement")
            })?;
        }
    }
}

fn pump_failure(detail: impl Into<String>) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Gatt,
        "gatt.notification.source",
    )
    .with_detail(detail.into())
}

pub(crate) trait GroupSource: Send + Sync {
    fn recv_transfer(&self) -> impl std::future::Future<Output = Option<IngressTransfer>> + Send;
    fn ack_transfer(&self, transfer: &IngressTransfer) -> bool;
    fn prepare_failure_recovery(
        &self,
        _transfer: &IngressTransfer,
    ) -> Option<btleplug::ubm_peer_notification_ingress::FailureRecovery> {
        None
    }
}

impl GroupSource for btleplug::platform::SourceGroupConsumer {
    async fn recv_transfer(&self) -> Option<IngressTransfer> {
        self.recv_transfer().await
    }
    fn ack_transfer(&self, transfer: &IngressTransfer) -> bool {
        self.ack_transfer(transfer)
    }
    fn prepare_failure_recovery(
        &self,
        transfer: &IngressTransfer,
    ) -> Option<btleplug::ubm_peer_notification_ingress::FailureRecovery> {
        btleplug::platform::SourceGroupConsumer::prepare_failure_recovery(self, transfer)
    }
}

#[cfg(test)]
impl GroupSource for PeerNotificationIngress {
    async fn recv_transfer(&self) -> Option<IngressTransfer> {
        self.recv_transfer().await
    }
    fn ack_transfer(&self, transfer: &IngressTransfer) -> bool {
        self.ack_transfer(transfer)
    }
    fn prepare_failure_recovery(
        &self,
        transfer: &IngressTransfer,
    ) -> Option<btleplug::ubm_peer_notification_ingress::FailureRecovery> {
        self.prepare_failure_recovery(transfer)
    }
}

fn registry_membership(membership: Membership) -> ubm_core::ordered_notifications::Membership {
    ubm_core::ordered_notifications::Membership {
        member: membership.member,
        epoch: membership.epoch,
    }
}

/// Update collection owners in source order before ordinary subscriber fanout.
#[derive(Debug)]
struct CollectionAdmissionError {
    error: DesktopError,
    retryable: bool,
}

impl From<DesktopError> for CollectionAdmissionError {
    fn from(error: DesktopError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

fn admit_to_collections(
    registry: &Mutex<crate::ordered_notification_groups::OrderedGroupRegistry>,
    source: &crate::ordered_notification_groups::SourceKey,
    record: &MappedRecord,
    source_members: &Members,
) -> Result<(), CollectionAdmissionError> {
    use ubm_core::ordered_notifications::{EndReason, ObservedDelivery, SourceRejection};
    let mut registry = registry.lock().expect("ordered notification groups");
    let result = match record {
        MappedRecord::Value {
            membership,
            ordinal,
            provenance,
            value,
            ..
        } => {
            let provenance = match provenance {
                CallbackProvenance::Notification => {
                    ubm_core::central::NotificationProvenance::Notification
                }
                CallbackProvenance::ReadOrNotification => {
                    ubm_core::central::NotificationProvenance::ReadOrNotification
                }
                CallbackProvenance::Unknown => ubm_core::central::NotificationProvenance::Unknown,
            };
            registry
                .admit_value(
                    source,
                    registry_membership(*membership),
                    *ordinal,
                    &value.value,
                    ObservedDelivery::Unknown,
                    provenance,
                )
                .map(|_| ())
        }
        MappedRecord::Loss {
            memberships,
            skipped_records,
            ..
        } => {
            let members: Vec<_> = memberships
                .iter()
                .copied()
                .map(registry_membership)
                .collect();
            registry
                .admit_source_loss(source, &members, Some(*skipped_records))
                .map(|_| ())
        }
        MappedRecord::Fence { membership, .. } => {
            registry.retire_membership(source, registry_membership(*membership));
            Ok(())
        }
        MappedRecord::Barrier { .. } => Ok(()),
        MappedRecord::Marker { id, ordinal } => {
            registry.admit_marker(source, *id, *ordinal).map(|_| ())
        }
        MappedRecord::FailureOccurrence {
            memberships, error, ..
        } => {
            // A durable retained failure carries only a bounded summary here.
            // Its exact bytes remain behind FailureRecovery and must never be
            // re-materialized merely to produce a normal stream error.
            let native = btleplug::Error::Platform(error.as_arc().as_ref().clone());
            let desktop = pump_failure("peer notification source failed").with_os(&native);
            let failure = Arc::new(crate::ordered_notification_groups::failure_from_desktop(
                &desktop,
            ));
            let members: Vec<_> = memberships
                .iter()
                .copied()
                .map(registry_membership)
                .collect();
            registry
                .admit_source_failure(source, &members, &failure, None, true)
                .map(|_| ())
        }
        MappedRecord::Terminal {
            memberships,
            reason,
            ..
        } => {
            let members: Vec<_> = memberships
                .iter()
                .copied()
                .map(registry_membership)
                .collect();
            let (end, rejection, failed_member, tail_uncertain) = match reason {
                TerminalReason::Closed { tail_uncertain } => (
                    source_members
                        .retirement_reason
                        .lock()
                        .expect("retirement reason")
                        .take()
                        .unwrap_or(EndReason::Closed),
                    None,
                    None,
                    *tail_uncertain,
                ),
                TerminalReason::Overflow {
                    rejected,
                    tail_uncertain,
                } => {
                    let rejection = match rejected {
                        RejectedRecord::Value { bytes } => SourceRejection::Value {
                            bytes: u64::try_from(*bytes).map_err(|_| {
                                pump_failure("source rejection byte count overflow")
                            })?,
                        },
                        RejectedRecord::Loss { skipped_records } => SourceRejection::Loss {
                            skipped_records: *skipped_records,
                        },
                        RejectedRecord::Fence { membership } => SourceRejection::Fence {
                            membership: registry_membership(*membership),
                        },
                        RejectedRecord::Barrier { id } => SourceRejection::Barrier { id: *id },
                        RejectedRecord::Marker { id } => SourceRejection::Marker { marker_id: *id },
                    };
                    (
                        EndReason::SourceOverflow,
                        Some(rejection),
                        None,
                        *tail_uncertain,
                    )
                }
                TerminalReason::MarkerOrdinalExhausted { id, tail_uncertain } => (
                    EndReason::SourceOrdinalExhausted,
                    Some(SourceRejection::Marker { marker_id: *id }),
                    None,
                    *tail_uncertain,
                ),
                TerminalReason::OrdinalExhausted {
                    rejected_bytes,
                    tail_uncertain,
                } => (
                    EndReason::SourceOrdinalExhausted,
                    Some(SourceRejection::OrdinalExhausted {
                        bytes: u64::try_from(*rejected_bytes)
                            .map_err(|_| pump_failure("source rejection byte count overflow"))?,
                    }),
                    None,
                    *tail_uncertain,
                ),
                TerminalReason::SourceFailure {
                    membership,
                    error,
                    rejected_loss,
                    tail_uncertain,
                } => {
                    let native = btleplug::Error::Platform((**error).clone());
                    let error = pump_failure("peer notification source failed").with_os(&native);
                    (
                        EndReason::SourceFailed(
                            crate::ordered_notification_groups::failure_from_desktop(&error),
                        ),
                        rejected_loss
                            .map(|skipped_records| SourceRejection::Loss { skipped_records }),
                        membership.map(registry_membership),
                        *tail_uncertain,
                    )
                }
            };
            registry
                .source_terminal(
                    source,
                    &members,
                    end,
                    rejection,
                    failed_member,
                    tail_uncertain,
                )
                .map(|_| ())
        }
    };
    result.map_err(|error| CollectionAdmissionError {
        retryable: matches!(
            error,
            crate::ordered_notification_groups::RegistryError::Collection(
                ubm_core::ordered_notifications::Error::PendingCapacity
            )
        ) || (matches!(
            record,
            MappedRecord::Terminal { .. } | MappedRecord::FailureOccurrence { .. }
        ) && matches!(
            error,
            crate::ordered_notification_groups::RegistryError::Collection(
                ubm_core::ordered_notifications::Error::RecordTooLarge
            )
        )),
        error: pump_failure(format!(
            "ordered collection source admission failed: {error:?}"
        )),
    })
}

pub(crate) struct PumpFailure {
    pub(crate) error: DesktopError,
    pub(crate) rejected: Option<ubm_core::ordered_notifications::SourceRejection>,
}

impl From<DesktopError> for PumpFailure {
    fn from(error: DesktopError) -> Self {
        Self {
            error,
            rejected: None,
        }
    }
}

fn rejected_ingress(
    record: &IngressRecord,
) -> Option<ubm_core::ordered_notifications::SourceRejection> {
    use ubm_core::ordered_notifications::SourceRejection;
    match record {
        IngressRecord::Value { value, .. } => u64::try_from(value.value.len())
            .ok()
            .map(|bytes| SourceRejection::Value { bytes }),
        IngressRecord::Loss {
            skipped_records, ..
        } => Some(SourceRejection::Loss {
            skipped_records: *skipped_records,
        }),
        IngressRecord::Fence { membership } => Some(SourceRejection::Fence {
            membership: registry_membership(*membership),
        }),
        IngressRecord::Barrier { id } => Some(SourceRejection::Barrier { id: *id }),
        IngressRecord::Marker { id, .. } => Some(SourceRejection::Marker { marker_id: *id }),
        IngressRecord::FailureOccurrence { .. } => None,
        IngressRecord::Terminal { .. } => None,
    }
}

fn rejected_record(
    record: &MappedRecord,
) -> Option<ubm_core::ordered_notifications::SourceRejection> {
    use ubm_core::ordered_notifications::SourceRejection;
    match record {
        MappedRecord::Value { value, .. } => u64::try_from(value.value.len())
            .ok()
            .map(|bytes| SourceRejection::Value { bytes }),
        MappedRecord::Loss {
            skipped_records, ..
        } => Some(SourceRejection::Loss {
            skipped_records: *skipped_records,
        }),
        MappedRecord::Fence { membership, .. } => Some(SourceRejection::Fence {
            membership: registry_membership(*membership),
        }),
        MappedRecord::Barrier { id } => Some(SourceRejection::Barrier { id: *id }),
        MappedRecord::Marker { id, .. } => Some(SourceRejection::Marker { marker_id: *id }),
        MappedRecord::FailureOccurrence { .. } => None,
        MappedRecord::Terminal { .. } => None,
    }
}

/// One consumer drains a peer FIFO and sends its records in the same order.
/// Every native fence becomes a central-consumption acknowledgement.
#[expect(
    clippy::too_many_arguments,
    reason = "The pump preserves the established native ingress ownership boundary without introducing a second configuration object."
)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "The direct pump entrypoint remains covered by ingress ownership tests."
    )
)]
pub(crate) async fn pump_group<S: GroupSource>(
    peer_id: String,
    generation: u64,
    native_group: u64,
    fanout: bool,
    registry: Arc<Mutex<crate::ordered_notification_groups::OrderedGroupRegistry>>,
    consumer: S,
    members: Arc<Members>,
    barriers: Arc<BarrierAcks>,
    sender: mpsc::Sender<RadioEvent>,
    queued_bytes: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    pause: Option<(
        tokio::sync::watch::Receiver<bool>,
        tokio::sync::watch::Sender<bool>,
    )>,
) -> Result<(), PumpFailure> {
    pump_group_with_failure_recovery(
        peer_id,
        generation,
        native_group,
        fanout,
        registry,
        consumer,
        members,
        barriers,
        sender,
        queued_bytes,
        dropped,
        pause,
        None,
    )
    .await
}

/// Production peer-source pump with a radio-owned retained-failure store.
/// The store exposes producer-backed range pages; it never copies the error
/// payload into an unbounded event queue.  A failure head remains in the
/// source until central staging *and* the eventual recipient ACK complete.
#[expect(
    clippy::too_many_arguments,
    reason = "The recovery pump receives its explicit ownership and recovery dependencies from the existing host call shape."
)]
#[allow(
    clippy::collapsible_if,
    reason = "Rust 2021 does not support the let-chain form Clippy suggests for the retained pause and retry ownership checks."
)]
pub(crate) async fn pump_group_with_failure_recovery<S: GroupSource>(
    peer_id: String,
    generation: u64,
    native_group: u64,
    fanout: bool,
    registry: Arc<Mutex<crate::ordered_notification_groups::OrderedGroupRegistry>>,
    consumer: S,
    members: Arc<Members>,
    barriers: Arc<BarrierAcks>,
    sender: mpsc::Sender<RadioEvent>,
    queued_bytes: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    mut pause: Option<(
        tokio::sync::watch::Receiver<bool>,
        tokio::sync::watch::Sender<bool>,
    )>,
    failure_recoveries: Option<Arc<PeerFailureRecoveryStore>>,
) -> Result<(), PumpFailure> {
    if fanout {
        sender
            .send(RadioEvent::PeerNotificationSourceStarted {
                peer_id: peer_id.clone(),
                generation,
            })
            .await
            .map_err(|_| pump_failure("peer notification event channel closed"))?;
    }
    let source = crate::ordered_notification_groups::SourceKey {
        peer_id: peer_id.clone(),
        generation: ubm_core::ordered_notifications::SourceGeneration::Counter(generation),
        native_group,
    };
    loop {
        if let Some((request, acknowledged)) = pause.as_mut() {
            if *request.borrow_and_update() {
                acknowledged.send_replace(true);
                while *request.borrow_and_update() {
                    if request.changed().await.is_err() {
                        return Err(pump_failure("source pause owner closed").into());
                    }
                }
                acknowledged.send_replace(false);
            }
        }
        let record = if let Some((request, _)) = pause.as_mut() {
            tokio::select! {
                biased;
                change = request.changed() => { if change.is_err() { return Err(pump_failure("source pause owner closed").into()); } continue; }
                record = consumer.recv_transfer() => record,
            }
        } else {
            consumer.recv_transfer().await
        };
        let Some(transfer) = record else {
            break;
        };
        let retained_failure = if fanout
            && matches!(
                &*transfer.record,
                IngressRecord::FailureOccurrence { .. }
                    | IngressRecord::Terminal {
                        reason: TerminalReason::SourceFailure { .. },
                        ..
                    }
            ) {
            consumer.prepare_failure_recovery(&transfer)
        } else {
            None
        };
        let record = (*transfer.record).clone();
        let rejection = rejected_ingress(&record);
        let Some(record) = map_record(&members, record) else {
            return Err(PumpFailure {
                error: pump_failure("source record references an unknown membership"),
                rejected: rejection,
            });
        };
        let is_observation = matches!(
            &record,
            MappedRecord::Value { .. }
                | MappedRecord::Loss { .. }
                | MappedRecord::FailureOccurrence { .. }
        );
        // A production fanout failure is represented by the retained
        // descriptor, not a copied ordinary collection record.  Trying to
        // admit the original error first can make an arbitrarily large source
        // head permanently `RecordTooLarge` before central ever receives its
        // recovery offer. Ordered-source groups do not fan out and continue to
        // use the normal collection admission path.
        let descriptor_recovery = fanout
            && failure_recoveries.is_some()
            && matches!(
                &record,
                MappedRecord::FailureOccurrence { .. }
                    | MappedRecord::Terminal {
                        reason: TerminalReason::SourceFailure { .. },
                        ..
                    }
            );
        let already_admitted = if is_observation {
            let admitted = members
                .admitted_occurrence
                .lock()
                .expect("peer ingress progress");
            match admitted.as_ref() {
                Some(previous) if Arc::ptr_eq(previous, &transfer.record) => true,
                Some(_) => {
                    return Err(
                        pump_failure("source advanced past an unacknowledged occurrence").into(),
                    );
                }
                None => false,
            }
        } else {
            false
        };
        if !descriptor_recovery && !already_admitted {
            if let Err(error) = admit_to_collections(&registry, &source, &record, &members) {
                if !error.retryable {
                    return Err(PumpFailure {
                        error: error.error,
                        rejected: rejected_record(&record),
                    });
                }
                let wake = registry
                    .lock()
                    .expect("ordered notification groups")
                    .pending_source_wake(&source);
                let Some(wake) = wake else {
                    return Err(PumpFailure {
                        error: error.error,
                        rejected: rejected_record(&record),
                    });
                };
                loop {
                    if let Some((request, acknowledged)) = pause.as_mut() {
                        if *request.borrow_and_update() {
                            acknowledged.send_replace(true);
                            while *request.borrow_and_update() {
                                if request.changed().await.is_err() {
                                    return Err(pump_failure("source pause owner closed").into());
                                }
                            }
                            acknowledged.send_replace(false);
                        }
                    }
                    let notified = wake.notified();
                    let result = registry
                        .lock()
                        .expect("ordered notification groups")
                        .retry_pending_source(&source);
                    match result {
                        Ok(_) => break,
                        Err(crate::ordered_notification_groups::RegistryError::Collection(
                            ubm_core::ordered_notifications::Error::RecordTooLarge
                            | ubm_core::ordered_notifications::Error::PendingCapacity,
                        )) => {
                            if let Some((request, _)) = pause.as_mut() {
                                tokio::select! {
                                    _ = notified => {},
                                    changed = request.changed() => if changed.is_err() {
                                        return Err(pump_failure("source pause owner closed").into());
                                    },
                                }
                            } else {
                                notified.await;
                            }
                        }
                        Err(failure) => {
                            return Err(PumpFailure {
                                error: pump_failure(format!(
                                    "ordered source retry failed: {failure:?}"
                                )),
                                rejected: rejected_record(&record),
                            });
                        }
                    }
                }
            }
        }
        if is_observation && !already_admitted {
            *members
                .admitted_occurrence
                .lock()
                .expect("peer ingress progress") = Some(Arc::clone(&transfer.record));
        }
        if !fanout
            && matches!(
                &record,
                MappedRecord::Value { .. }
                    | MappedRecord::Loss { .. }
                    | MappedRecord::FailureOccurrence { .. }
            )
        {
            if !consumer.ack_transfer(&transfer) {
                return Err(pump_failure("source transfer acknowledgement rejected").into());
            }
            *members
                .admitted_occurrence
                .lock()
                .expect("peer ingress progress") = None;
            continue;
        }
        let mut completed_fence = None;
        let mut transfer_acknowledged = false;
        match record {
            MappedRecord::Marker { .. } => {}
            MappedRecord::FailureOccurrence {
                members: snapshot,
                error,
                ..
            } => {
                // Test sources do not install a radio store.  Production does;
                // only that path can offer a descriptor to central.
                if let Some(store) = failure_recoveries.as_ref() {
                    let recovery = retained_failure.ok_or_else(|| PumpFailure {
                        error: pump_failure(
                            "peer notification failure head cannot be retained for recovery",
                        ),
                        rejected: None,
                    })?;
                    let offer = store
                        .offer(
                            generation,
                            native_group,
                            recovery,
                            failure_occurrence_teardown(&error),
                        )
                        .map_err(PumpFailure::from)?;
                    let descriptor = offer.descriptor();
                    let completed = store
                        .acknowledgement_receiver(&descriptor)
                        .map_err(PumpFailure::from)?;
                    let (ack, admitted) = crate::boundary::PeerSourceFailureAck::new();
                    sender
                        .send(RadioEvent::PeerNotificationFailureRecoveryOffered {
                            descriptor,
                            members: snapshot,
                            teardown: offer.teardown(),
                            ack,
                        })
                        .await
                        .map_err(|_| pump_failure("peer notification event channel closed"))?;
                    admitted.await.map_err(|_| {
                        pump_failure("central did not admit peer notification failure recovery")
                    })?;
                    // Central admission only proves the exact source-time routes
                    // have staged the descriptor.  The original FIFO head remains
                    // retained until the recipient reads every page and calls the
                    // boundary ACK; waiting here prevents a later callback from
                    // overtaking that source occurrence.
                    PeerFailureRecoveryStore::wait_for_ack(completed)
                        .await
                        .map_err(PumpFailure::from)?;
                    offer.disarm();
                    transfer_acknowledged = true;
                }
            }
            MappedRecord::Value {
                membership: _,
                ordinal,
                target,
                provenance,
                value,
            } => {
                let len = value.value.len() as u64;
                let reserved = crate::btleplug_backend::ingress_try_reserve(&queued_bytes, len);
                let event = if reserved {
                    let provenance = match provenance {
                        CallbackProvenance::Notification => {
                            ubm_core::central::NotificationProvenance::Notification
                        }
                        CallbackProvenance::ReadOrNotification => {
                            ubm_core::central::NotificationProvenance::ReadOrNotification
                        }
                        CallbackProvenance::Unknown => {
                            ubm_core::central::NotificationProvenance::Unknown
                        }
                    };
                    RadioEvent::NotificationWithProvenance {
                        peer_id: target.peer_id,
                        service_uuid: target.service_uuid,
                        service_occurrence: target.service_occurrence,
                        characteristic_uuid: target.characteristic_uuid,
                        characteristic_occurrence: target.characteristic_occurrence,
                        epoch: target.epoch,
                        value: value.value,
                        provenance,
                        source_generation: generation,
                        source_ordinal: ordinal,
                    }
                } else {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    RadioEvent::NotificationsLost {
                        peer_id: target.peer_id,
                        service_uuid: target.service_uuid,
                        service_occurrence: target.service_occurrence,
                        characteristic_uuid: target.characteristic_uuid,
                        characteristic_occurrence: target.characteristic_occurrence,
                        epoch: target.epoch,
                        lost: 1,
                    }
                };
                if sender.send(event).await.is_err() {
                    if reserved {
                        crate::btleplug_backend::ingress_release(&queued_bytes, len);
                    }
                    return Err(pump_failure("peer notification event channel closed").into());
                }
            }
            MappedRecord::Loss {
                memberships: _,
                members,
                skipped_records,
            } => {
                sender
                    .send(RadioEvent::PeerNotificationsLost {
                        peer_id: peer_id.clone(),
                        generation,
                        members,
                        lost: Some(skipped_records),
                    })
                    .await
                    .map_err(|_| pump_failure("peer notification event channel closed"))?;
            }
            MappedRecord::Fence { membership, .. } => {
                let (ack, received) = PeerSourceFenceAck::new();
                sender
                    .send(RadioEvent::PeerNotificationSourceFence { ack })
                    .await
                    .map_err(|_| pump_failure("peer notification event channel closed"))?;
                received
                    .await
                    .map_err(|_| pump_failure("central notification fence was not acknowledged"))?;
                completed_fence = Some(membership);
            }
            MappedRecord::Barrier { id } => {
                // Secure the downstream slot first, then consume the native
                // barrier before publishing its acknowledgement. The receiver
                // may immediately use that acknowledgement to close the source.
                let permit = sender
                    .reserve()
                    .await
                    .map_err(|_| pump_failure("peer notification event channel closed"))?;
                let ack = barriers
                    .lock()
                    .expect("peer barriers")
                    .remove(&id)
                    .ok_or_else(|| pump_failure("source barrier has no waiting owner"))?;
                if !consumer.ack_transfer(&transfer) {
                    return Err(pump_failure("source transfer acknowledgement rejected").into());
                }
                transfer_acknowledged = true;
                permit.send(RadioEvent::PeerNotificationSourceFence { ack });
            }
            MappedRecord::Terminal {
                memberships: _,
                members: snapshot,
                reason,
            } => {
                if descriptor_recovery {
                    let store = failure_recoveries
                        .as_ref()
                        .expect("descriptor recovery store");
                    let recovery = retained_failure.ok_or_else(|| PumpFailure {
                        error: pump_failure(
                            "peer notification terminal failure head cannot be retained for recovery",
                        ),
                        rejected: None,
                    })?;
                    let offer = store
                        .offer(
                            generation,
                            native_group,
                            recovery,
                            match &reason {
                                TerminalReason::SourceFailure { error, .. } => {
                                    failure_occurrence_teardown(error)
                                }
                                _ => unreachable!("only source failures retain recovery"),
                            },
                        )
                        .map_err(PumpFailure::from)?;
                    let descriptor = offer.descriptor();
                    let completed = store
                        .acknowledgement_receiver(&descriptor)
                        .map_err(PumpFailure::from)?;
                    let (ack, admitted) = crate::boundary::PeerSourceFailureAck::new();
                    sender
                        .send(RadioEvent::PeerNotificationFailureRecoveryOffered {
                            descriptor,
                            members: snapshot,
                            teardown: offer.teardown(),
                            ack,
                        })
                        .await
                        .map_err(|_| pump_failure("peer notification event channel closed"))?;
                    admitted.await.map_err(|_| {
                        pump_failure(
                            "central did not admit peer notification terminal failure recovery",
                        )
                    })?;
                    PeerFailureRecoveryStore::wait_for_ack(completed)
                        .await
                        .map_err(PumpFailure::from)?;
                    offer.disarm();
                    if !consumer.ack_transfer(&transfer) {
                        return Err(pump_failure("source transfer acknowledgement rejected").into());
                    }
                    members.close();
                    barriers.lock().expect("peer barriers").clear();
                    return Ok(());
                }
                let (error, rejected_loss, tail_uncertain) = match reason {
                    TerminalReason::Closed { tail_uncertain } => (
                        pump_failure("peer notification source closed"),
                        None,
                        tail_uncertain,
                    ),
                    TerminalReason::Overflow {
                        rejected,
                        tail_uncertain,
                    } => {
                        let loss = match rejected {
                            RejectedRecord::Value { .. } => Some(1),
                            RejectedRecord::Loss { skipped_records } => Some(skipped_records),
                            _ => None,
                        };
                        (
                            pump_failure("peer notification source overflowed"),
                            loss,
                            tail_uncertain,
                        )
                    }
                    TerminalReason::OrdinalExhausted { tail_uncertain, .. } => (
                        pump_failure("peer notification source ordinal exhausted"),
                        Some(1),
                        tail_uncertain,
                    ),
                    TerminalReason::MarkerOrdinalExhausted { tail_uncertain, .. } => (
                        pump_failure("peer notification source ordinal exhausted"),
                        None,
                        tail_uncertain,
                    ),
                    TerminalReason::SourceFailure {
                        error,
                        rejected_loss,
                        tail_uncertain,
                        ..
                    } => {
                        let native = btleplug::Error::Platform(error.as_arc().as_ref().clone());
                        (
                            pump_failure("peer notification source failed").with_os(&native),
                            rejected_loss,
                            tail_uncertain,
                        )
                    }
                };
                if fanout && (!snapshot.is_empty() || tail_uncertain) {
                    sender
                        .send(RadioEvent::PeerNotificationSourceFailed {
                            peer_id: peer_id.clone(),
                            generation,
                            members: snapshot,
                            error,
                            rejected_loss,
                            tail_uncertain,
                        })
                        .await
                        .map_err(|_| pump_failure("peer notification event channel closed"))?;
                }
                // Replacement may reuse this native generation. Do not report
                // the group drained until central has applied the terminal.
                let (ack, received) = PeerSourceFenceAck::new();
                sender
                    .send(RadioEvent::PeerNotificationSourceFence { ack })
                    .await
                    .map_err(|_| pump_failure("peer notification event channel closed"))?;
                received
                    .await
                    .map_err(|_| pump_failure("central notification fence was not acknowledged"))?;
                if !consumer.ack_transfer(&transfer) {
                    return Err(pump_failure("source transfer acknowledgement rejected").into());
                }
                members.close();
                barriers.lock().expect("peer barriers").clear();
                return Ok(());
            }
        }
        if !transfer_acknowledged && !consumer.ack_transfer(&transfer) {
            return Err(pump_failure("source transfer acknowledgement rejected").into());
        }
        if is_observation {
            *members
                .admitted_occurrence
                .lock()
                .expect("peer ingress progress") = None;
        }
        if let Some(membership) = completed_fence {
            members.fence(membership);
        }
    }
    Err(pump_failure("peer notification source ended without terminal record").into())
}

#[path = "peer_notification_owner.rs"]
mod owner;
pub(crate) use owner::{Group, MemberRenewal, OrderedSourceOwner, PeerGroupState, open_group};
#[cfg(test)]
pub(crate) use owner::{OpenGenerationError, SourceGeneration};

#[cfg(test)]
#[path = "peer_notification_ingress_tests.rs"]
mod tests;
