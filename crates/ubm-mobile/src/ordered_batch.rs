// crates/ubm-mobile/src/ordered_batch.rs
//! One installed source lease's retained batch consumer.

use crate::host::HostInner;
use crate::radio::{
    OrderedEnd, OrderedRecord, OrderedRegistration, OrderedRejected, RadioCompletion, RadioRequest,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::sync::Mutex;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use ubm_core::ordered_notifications::{
    EndReason, Membership, OrderedCollection, RejectedLoss, SourceRejection,
};
use ubm_desktop::central::{OrderedIngressReceipt, OrderedIngressRecord};
use ubm_desktop::ordered_notification_groups::{SourceKey, failure_from_desktop};
use ubm_desktop::{DesktopError, PlatformDetail};

struct Pending {
    id: u64,
    records: Vec<OrderedRecord>,
    offset: usize,
}

/// Owns a single explicitly installed collection. Keep this object through
/// cancellation and cleanup: it retains the admitted offset until native ack.
pub struct OrderedBatchConsumer {
    host: Weak<HostInner>,
    source: SourceKey,
    collection_id: u64,
    empty_setup: bool,
    pending: Mutex<Option<Pending>>,
    consumed_fences: StdMutex<Vec<OrderedRegistration>>,
    consumed_barriers: StdMutex<Vec<(u64, bool)>>,
    fence_notify: tokio::sync::Notify,
    barrier_notify: tokio::sync::Notify,
    terminal_acked: AtomicBool,
    failed: AtomicBool,
    source_final: AtomicBool,
    seal_confirmed: AtomicBool,
    close_confirmed: AtomicBool,
    routing_paused: AtomicBool,
    routing_resume: tokio::sync::Notify,
    close_intent: StdMutex<Option<EndReason>>,
    unreported_failure: StdMutex<Option<(DesktopError, DesktopError)>>,
}

fn failure(detail: &str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::ProtocolViolation,
        BleErrorDomain::Gatt,
        "gatt.ordered.ingress",
    )
    .with_detail(detail.to_owned())
}
/// A retained record that does not fit is an admission refusal, not evidence
/// that the native source failed. Its batch remains owned for exact retry.
pub(crate) fn is_admission_refusal(error: &DesktopError) -> bool {
    error.code() == BleErrorCode::ArgumentInvalid
        && error.domain() == BleErrorDomain::Gatt
        && matches!(
            error.operation(),
            "gatt.ordered.ingress.record-too-large"
                | "gatt.ordered.ingress.failure-unrepresentable"
        )
}
fn membership(member: OrderedRegistration) -> Membership {
    Membership {
        member: member.source_member,
        epoch: member.epoch,
    }
}
fn convert(
    record: OrderedRecord,
    close_intent: Option<&EndReason>,
    platform: crate::radio::MobilePlatform,
) -> Result<OrderedIngressRecord, DesktopError> {
    // Bound variable-length native evidence before duplicating it in the
    // central queue. The caller retains the original batch and does not ACK
    // this record when it cannot fit; no platform detail is truncated.
    fn failure_size(failure: &crate::radio::PlatformFailure) -> usize {
        failure
            .detail
            .len()
            .saturating_add(failure.native_domain.as_ref().map_or(0, String::len))
            .saturating_add(failure.native_name.as_ref().map_or(0, String::len))
    }
    let bytes = match &record {
        OrderedRecord::Value { value, .. } => value.len(),
        OrderedRecord::BridgeFailure { code, detail } => code.len().saturating_add(detail.len()),
        OrderedRecord::SubsequentSourceFailure { failure } => failure_size(failure),
        OrderedRecord::SourceFailed {
            failure,
            native_domain,
            native_code,
            ..
        } => failure_size(failure)
            .saturating_add(native_domain.as_ref().map_or(0, String::len))
            .saturating_add(native_code.as_ref().map_or(0, String::len)),
        OrderedRecord::Terminal {
            native_domain,
            native_code,
            ..
        } => native_domain
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(native_code.as_ref().map_or(0, String::len)),
        _ => 0,
    };
    if bytes > ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY as usize {
        return Err(DesktopError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Gatt,
            "gatt.ordered.ingress.record-too-large",
        )
        .with_detail("native ordered record exceeds the maximum stream byte capacity"));
    }
    let (record, source_failure) = match record {
        OrderedRecord::SourceFailed {
            uncertain_tail,
            failure,
            native_domain,
            native_code,
            failure_member,
            rejected,
        } => (
            OrderedRecord::Terminal {
                reason: OrderedEnd::SourceFailed,
                uncertain_tail,
                native_domain,
                native_code,
                failure_member,
                rejected,
            },
            Some(failure),
        ),
        other => (other, None),
    };
    let converted = match record {
        OrderedRecord::Value {
            member,
            ordinal,
            value,
            delivery,
            provenance,
        } => OrderedIngressRecord::Value {
            member_id: member.member_id,
            membership: membership(member),
            ordinal,
            value,
            delivery: match delivery {
                ubm_desktop::ObservedDelivery::Notification => {
                    ubm_core::ordered_notifications::ObservedDelivery::Notification
                }
                ubm_desktop::ObservedDelivery::Indication => {
                    ubm_core::ordered_notifications::ObservedDelivery::Indication
                }
                ubm_desktop::ObservedDelivery::Unknown => {
                    ubm_core::ordered_notifications::ObservedDelivery::Unknown
                }
            },
            provenance,
        },
        OrderedRecord::Loss {
            scope,
            unit,
            count,
            byte_count,
        } => OrderedIngressRecord::Loss {
            scope,
            unit,
            count,
            bytes: byte_count,
        },
        OrderedRecord::Barrier {
            barrier_id,
            uncertain_tail,
        } => OrderedIngressRecord::Barrier {
            id: barrier_id,
            uncertain_tail,
        },
        OrderedRecord::Fence {
            member,
            uncertain_tail,
        } => OrderedIngressRecord::Fence {
            member_id: member.member_id,
            membership: membership(member),
            uncertain_tail,
        },
        OrderedRecord::Marker { marker_id, ordinal } => {
            OrderedIngressRecord::Marker { marker_id, ordinal }
        }
        OrderedRecord::Terminal {
            reason,
            uncertain_tail,
            native_domain,
            native_code,
            failure_member,
            rejected,
        } => {
            let (rejection, rejected_loss, rejected_member, unmapped) = match rejected {
                None => (None, None, None, false),
                Some(OrderedRejected::Value { member, byte_count }) => (
                    Some(SourceRejection::Value { bytes: byte_count }),
                    None,
                    Some(membership(member)),
                    false,
                ),
                Some(OrderedRejected::Fence { member }) => (
                    Some(SourceRejection::Fence {
                        membership: membership(member),
                    }),
                    None,
                    Some(membership(member)),
                    false,
                ),
                Some(OrderedRejected::Loss {
                    scope,
                    unit,
                    count,
                    byte_count,
                }) => (
                    None,
                    Some(RejectedLoss {
                        scope,
                        unit,
                        count,
                        bytes: byte_count,
                    }),
                    None,
                    false,
                ),
                Some(OrderedRejected::Barrier { barrier_id }) => (
                    Some(SourceRejection::Barrier { id: barrier_id }),
                    None,
                    None,
                    true,
                ),
                Some(OrderedRejected::Marker { marker_id }) => (
                    Some(SourceRejection::Marker { marker_id }),
                    None,
                    None,
                    false,
                ),
                Some(OrderedRejected::Unmapped) => (None, None, None, true),
            };
            let cause = match reason {
                OrderedEnd::Closed => close_intent.cloned().unwrap_or(EndReason::Closed),
                OrderedEnd::Overflow => EndReason::SourceOverflow,
                OrderedEnd::OrdinalExhausted => EndReason::SourceOrdinalExhausted,
                OrderedEnd::ConnectionLost
                | OrderedEnd::ServiceChanged
                | OrderedEnd::SourceReplaced
                | OrderedEnd::Shutdown
                | OrderedEnd::AdapterLost => EndReason::Invalidated,
                OrderedEnd::SourceFailed | OrderedEnd::WakeFailed => {
                    let mut error = source_failure
                        .map(|failure| {
                            let mapped = failure.to_notification_source_error(platform);
                            // The ordinary operation mapper may supply an operation-name
                            // platform code. A source terminal must not invent a native cause.
                            if failure.gatt_status.is_none()
                                && failure.native_name.is_none()
                                && failure.native_domain.is_none()
                                && failure.native_code.is_none()
                            {
                                let mut without_platform = DesktopError::new(
                                    mapped.code(),
                                    mapped.domain(),
                                    mapped.operation(),
                                );
                                if let Some(detail) = mapped.detail() {
                                    without_platform = without_platform.with_detail(detail);
                                }
                                without_platform
                            } else {
                                mapped
                            }
                        })
                        .unwrap_or_else(|| {
                            DesktopError::new(
                                BleErrorCode::PlatformFailure,
                                BleErrorDomain::Gatt,
                                "gatt.ordered.source",
                            )
                        });
                    if let (Some(domain), Some(code)) = (native_domain, native_code) {
                        let mut detail = error
                            .platform()
                            .cloned()
                            .unwrap_or_else(|| PlatformDetail::new(&domain, &code));
                        detail.domain = domain;
                        detail.code = code;
                        error = error.with_platform(detail);
                    }
                    EndReason::SourceFailed(failure_from_desktop(&error))
                }
            };
            OrderedIngressRecord::Terminal {
                reason: cause,
                rejection,
                failure_member: failure_member.map(membership).or(rejected_member),
                rejected_loss,
                uncertain_tail: uncertain_tail || unmapped,
            }
        }
        OrderedRecord::SourceFailed { .. } => unreachable!("source failure normalized above"),
        OrderedRecord::SubsequentSourceFailure { failure } => {
            let mapped = failure.to_notification_source_error(platform);
            let error = if let (crate::radio::MobilePlatform::Android, Some(status)) =
                (platform, failure.gatt_status)
            {
                let mut detail = mapped
                    .platform()
                    .cloned()
                    .unwrap_or_else(|| PlatformDetail::new("android", status.to_string()));
                detail.domain = "android".into();
                detail.code = status.to_string();
                mapped.with_platform(detail)
            } else if failure.gatt_status.is_none()
                && failure.native_name.is_none()
                && failure.native_domain.is_none()
                && failure.native_code.is_none()
            {
                let mut error =
                    DesktopError::new(mapped.code(), mapped.domain(), mapped.operation());
                if let Some(detail) = mapped.detail() {
                    error = error.with_detail(detail);
                }
                error
            } else {
                mapped
            };
            OrderedIngressRecord::BridgeFailure {
                failure: failure_from_desktop(&error),
            }
        }
        OrderedRecord::FailureObservationsOmitted => {
            OrderedIngressRecord::FailureObservationsOmitted
        }
        OrderedRecord::BridgeFailure { code, detail } => {
            let error =
                failure(&detail).with_platform(PlatformDetail::new("native-ordered-bridge", code));
            OrderedIngressRecord::BridgeFailure {
                failure: failure_from_desktop(&error),
            }
        }
    };
    // Validate against the negotiated legacy reader before the native batch
    // can be acknowledged. A queue byte limit alone is not a wire limit.
    let failure = match &converted {
        OrderedIngressRecord::BridgeFailure { failure } => Some(failure),
        OrderedIngressRecord::Terminal { reason, .. } => match reason {
            EndReason::SetupFailed(failure)
            | EndReason::SourceFailed(failure)
            | EndReason::HostReset(failure) => Some(failure),
            _ => None,
        },
        _ => None,
    };
    if let Some(failure) = failure {
        if !crate::ordered_encoding::legacy_failure_encodable(failure) {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "gatt.ordered.ingress.failure-unrepresentable",
            )
            .with_detail(
                "native failure cannot be represented by the selected mobile wire revision",
            ));
        }
    }
    Ok(converted)
}
impl OrderedBatchConsumer {
    /// Change the installed queue while serialized with its retained native batch.
    pub(crate) async fn increase_byte_capacity(
        &self,
        session: u64,
        max_bytes: usize,
    ) -> Result<usize, DesktopError> {
        let _pending = self.pending.lock().await;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| failure("ordered host is closed"))?;
        host.ordered_owners
            .with_current(self.collection_id, &self.source.peer_id, session, || {
                // A worker woken here cannot consume until the pending lock is
                // released, after the atomic central change. Failure to schedule
                // therefore cannot leave an unreported capacity change behind.
                host.ordered_wakes
                    .wake(self.source.native_group, self.source.generation.clone())
                    .map_err(|error| {
                        failure(&format!("ordered recovery wake failed: {error:?}"))
                    })?;
                let previous = if self.empty_setup {
                    host.central.increase_empty_ordered_byte_capacity(
                        &self.source,
                        self.collection_id,
                        max_bytes,
                    )?
                } else {
                    host.central.increase_mobile_ordered_byte_capacity(
                        self.collection_id,
                        &self.source,
                        max_bytes,
                    )?
                };
                Ok(previous)
            })
    }

    pub(crate) async fn install(
        host: Arc<HostInner>,
        source: SourceKey,
        owner: OrderedCollection,
        admission: &ubm_desktop::GattAdmission,
    ) -> Result<Self, DesktopError> {
        let collection_id = owner.group_id;
        host.central
            .install_mobile_ordered_ingress(source.clone(), owner, admission)
            .await?;
        Ok(Self {
            host: Arc::downgrade(&host),
            source,
            collection_id,
            empty_setup: false,
            pending: Mutex::new(None),
            consumed_fences: StdMutex::new(Vec::new()),
            consumed_barriers: StdMutex::new(Vec::new()),
            fence_notify: tokio::sync::Notify::new(),
            barrier_notify: tokio::sync::Notify::new(),
            terminal_acked: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            source_final: AtomicBool::new(false),
            seal_confirmed: AtomicBool::new(false),
            close_confirmed: AtomicBool::new(false),
            routing_paused: AtomicBool::new(false),
            routing_resume: tokio::sync::Notify::new(),
            close_intent: StdMutex::new(None),
            unreported_failure: StdMutex::new(None),
        })
    }
    pub(crate) fn attach(
        host: &Arc<HostInner>,
        source: SourceKey,
        collection_id: u64,
        empty_setup: bool,
    ) -> Self {
        Self {
            host: Arc::downgrade(host),
            source,
            collection_id,
            empty_setup,
            pending: Mutex::new(None),
            consumed_fences: StdMutex::new(Vec::new()),
            consumed_barriers: StdMutex::new(Vec::new()),
            fence_notify: tokio::sync::Notify::new(),
            barrier_notify: tokio::sync::Notify::new(),
            terminal_acked: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            source_final: AtomicBool::new(false),
            seal_confirmed: AtomicBool::new(false),
            close_confirmed: AtomicBool::new(false),
            routing_paused: AtomicBool::new(false),
            routing_resume: tokio::sync::Notify::new(),
            close_intent: StdMutex::new(None),
            unreported_failure: StdMutex::new(None),
        }
    }
    /// The receipt remains until the source owner completes its rebind or cleanup.
    pub fn has_consumed_fence(&self, member: OrderedRegistration) -> bool {
        crate::foreign::lock(&self.consumed_fences).contains(&member)
    }
    pub fn release_consumed_fence(&self, member: OrderedRegistration) -> bool {
        let mut fences = crate::foreign::lock(&self.consumed_fences);
        if let Some(index) = fences.iter().position(|fence| *fence == member) {
            fences.remove(index);
            true
        } else {
            false
        }
    }
    /// Retain the requested close reason while native drains its staged prefix.
    pub fn set_close_intent(&self, reason: EndReason) -> Result<(), DesktopError> {
        let mut intent = crate::foreign::lock(&self.close_intent);
        if intent.as_ref().is_some_and(|current| current != &reason) {
            return Err(failure("ordered source close reason changed"));
        }
        *intent = Some(reason);
        Ok(())
    }
    /// Pause at an admitted batch boundary. The pause survives caller cancellation
    /// and is cleared only after exact native registration and central rebind.
    pub async fn pause_routing(&self) {
        let _batch = self.pending.lock().await;
        self.routing_paused.store(true, Ordering::SeqCst);
    }
    /// Call only after the exact new membership was installed centrally.
    pub fn resume_routing(&self) {
        self.routing_paused.store(false, Ordering::SeqCst);
        self.routing_resume.notify_waiters();
        self.routing_resume.notify_one();
    }
    pub fn routing_paused(&self) -> bool {
        self.routing_paused.load(Ordering::SeqCst)
    }
    pub fn ended(&self) -> bool {
        self.terminal_acked.load(Ordering::SeqCst) || self.failed.load(Ordering::SeqCst)
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
    pub fn terminal_acked(&self) -> bool {
        self.terminal_acked.load(Ordering::SeqCst)
    }
    /// Retain a scoped source failure while leaving the unacknowledged batch owned.
    pub(crate) fn record_failure(&self, error: &DesktopError) -> Result<(), DesktopError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| failure("ordered host is closed"))?;
        let cause = failure_from_desktop(error);
        if self.empty_setup {
            host.central
                .fail_empty_ordered_ingress(&self.source, self.collection_id, cause)
        } else {
            host.central
                .admit_mobile_ordered_record(
                    self.collection_id,
                    &self.source,
                    OrderedIngressRecord::BridgeFailure { failure: cause },
                )
                .map(|_| ())
        }
    }
    /// Retain one unreported failure when the central's bounded failure slot
    /// is occupied. The source stays failed and its native batch unacknowledged.
    pub(crate) fn retain_unreported_failure(&self, source: DesktopError, report: DesktopError) {
        *crate::foreign::lock(&self.unreported_failure) = Some((source, report));
    }
    pub fn unreported_failure(&self) -> Option<(DesktopError, DesktopError)> {
        crate::foreign::lock(&self.unreported_failure).clone()
    }
    /// Retry after the earlier central failure has been consumed; clear only
    /// when the exact retained failure reaches the central receipt.
    pub fn retry_failure_report(&self) -> Result<(), DesktopError> {
        let mut pending = crate::foreign::lock(&self.unreported_failure);
        let Some((source, _)) = pending.as_ref() else {
            return Ok(());
        };
        match self.record_failure(source) {
            Ok(()) => {
                *pending = None;
                Ok(())
            }
            Err(error) => {
                let source = source.clone();
                *pending = Some((source, error.clone()));
                Err(error)
            }
        }
    }
    pub(crate) fn mark_failed(&self) {
        self.failed.store(true, Ordering::SeqCst);
        self.routing_resume.notify_waiters();
        self.routing_resume.notify_one();
        self.barrier_notify.notify_waiters();
        self.barrier_notify.notify_one();
        self.fence_notify.notify_waiters();
        self.fence_notify.notify_one();
    }
    pub(crate) fn mark_failed_on_drain_error(&self, error: &DesktopError) {
        if !is_admission_refusal(error) {
            self.mark_failed();
        }
    }
    /// Returns native tail uncertainty only after the barrier batch ACK succeeds.
    pub fn take_consumed_barrier(&self, id: u64) -> Option<bool> {
        let mut barriers = crate::foreign::lock(&self.consumed_barriers);
        barriers
            .iter()
            .position(|(current, _)| *current == id)
            .map(|index| barriers.remove(index).1)
    }
    pub async fn barrier_notified(&self) {
        self.barrier_notify.notified().await;
    }
    pub async fn fence_notified(&self) {
        self.fence_notify.notified().await;
    }
    /// After native confirms an idempotent source seal, drain and ACK all
    /// retained controls before publishing source finality. A canceled call
    /// retains batch progress and can retry the same native seal.
    pub(crate) async fn finish_sealed_drain(&self) -> Result<bool, DesktopError> {
        if self.source_final.load(Ordering::SeqCst) {
            return Ok(true);
        }
        if self.failed() {
            return Err(failure("ordered source failed before final drain"));
        }
        if !self.drain_to_empty().await? {
            return Ok(false);
        }
        if self.failed() {
            return Err(failure("ordered source failed during final drain"));
        }
        self.source_final.store(true, Ordering::SeqCst);
        Ok(true)
    }
    pub(crate) fn seal_confirmed(&self) -> bool {
        self.seal_confirmed.load(Ordering::SeqCst)
    }
    pub(crate) fn confirm_seal(&self) {
        self.seal_confirmed.store(true, Ordering::SeqCst);
    }
    pub(crate) fn close_confirmed(&self) -> bool {
        self.close_confirmed.load(Ordering::SeqCst)
    }
    pub(crate) fn confirm_close(&self) {
        self.close_confirmed.store(true, Ordering::SeqCst);
    }
    pub(crate) fn source_final(&self) -> bool {
        self.source_final.load(Ordering::SeqCst)
    }
    /// Drain through the source's empty/rearm result. A cancellation or ack
    /// timeout retains both the batch ID and admitted offset for exact retry.
    pub async fn drain_to_empty(&self) -> Result<bool, DesktopError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| failure("ordered host is closed"))?;
        let mut pending = loop {
            while self.routing_paused.load(Ordering::SeqCst) {
                if self.failed() {
                    return Err(failure("ordered source failed while routing was paused"));
                }
                self.routing_resume.notified().await;
            }
            let guard = self.pending.lock().await;
            if !self.routing_paused.load(Ordering::SeqCst) {
                break guard;
            }
        };
        loop {
            if self.failed() {
                return Err(failure("ordered source is failed"));
            }
            if pending.is_none() {
                let answer = host
                    .radio
                    .call(|id| RadioRequest::OrderedDrain {
                        id,
                        generation: self.source.generation.clone(),
                        group_id: self.source.native_group,
                    })
                    .await?;
                match answer {
                    RadioCompletion::OrderedRecords {
                        batch_id: None,
                        records,
                    } if records.is_empty() => return Ok(true),
                    RadioCompletion::OrderedRecords {
                        batch_id: Some(id),
                        records,
                    } if !records.is_empty() => {
                        *pending = Some(Pending {
                            id,
                            records,
                            offset: 0,
                        })
                    }
                    _ => return Err(failure("malformed ordered batch completion")),
                }
            }
            let Some(batch) = pending.as_mut() else {
                return Err(failure("missing retained batch"));
            };
            while batch.offset < batch.records.len() {
                if self.failed() {
                    return Err(failure("ordered source failed before record admission"));
                }
                let intent = crate::foreign::lock(&self.close_intent).clone();
                let record = convert(
                    batch.records[batch.offset].clone(),
                    intent.as_ref(),
                    host.platform,
                )?;
                if self.empty_setup {
                    let receipt = host.central.try_admit_empty_ordered_record(
                        &self.source,
                        self.collection_id,
                        record,
                    )?;
                    if receipt == OrderedIngressReceipt::PendingCapacity {
                        return Ok(false);
                    }
                } else {
                    let receipt = host.central.try_admit_mobile_ordered_record(
                        self.collection_id,
                        &self.source,
                        record,
                    )?;
                    if receipt == OrderedIngressReceipt::PendingCapacity {
                        return Ok(false);
                    }
                }
                batch.offset += 1;
            }
            let fenced: Vec<OrderedRegistration> = batch
                .records
                .iter()
                .filter_map(|record| match record {
                    OrderedRecord::Fence { member, .. } => Some(*member),
                    _ => None,
                })
                .collect();
            let barriers: Vec<(u64, bool)> = batch
                .records
                .iter()
                .filter_map(|record| match record {
                    OrderedRecord::Barrier {
                        barrier_id,
                        uncertain_tail,
                    } => Some((*barrier_id, *uncertain_tail)),
                    _ => None,
                })
                .collect();
            if crate::foreign::lock(&self.consumed_barriers)
                .len()
                .saturating_add(barriers.len())
                > 1024
            {
                return Err(failure("ordered barrier receipt capacity exceeded"));
            }
            if crate::foreign::lock(&self.consumed_fences)
                .len()
                .saturating_add(fenced.len())
                > 1024
            {
                return Err(failure("ordered fence receipt capacity exceeded"));
            }
            let batch_id = batch.id;
            let answer = host
                .radio
                .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                    id,
                    generation: self.source.generation.clone(),
                    group_id: self.source.native_group,
                    batch_id,
                })
                .await?;
            if !matches!(answer, RadioCompletion::Unit) {
                return Err(failure("malformed ordered batch acknowledgment"));
            }
            if batch.records.iter().any(|record| {
                matches!(
                    record,
                    OrderedRecord::Terminal { .. } | OrderedRecord::SourceFailed { .. }
                )
            }) {
                self.terminal_acked.store(true, Ordering::SeqCst);
                self.barrier_notify.notify_waiters();
                self.barrier_notify.notify_one();
                self.fence_notify.notify_waiters();
                self.fence_notify.notify_one();
            }
            if !barriers.is_empty() {
                crate::foreign::lock(&self.consumed_barriers).extend(barriers);
                self.barrier_notify.notify_one();
            }
            if !fenced.is_empty() {
                crate::foreign::lock(&self.consumed_fences).extend(fenced);
                self.fence_notify.notify_one();
            }
            *pending = None;
            // A wake can race an unacknowledged batch. Always re-drain until
            // native reports empty, regardless of whether another wake arrived.
        }
    }
}

#[cfg(test)]
#[path = "ordered_pump_tests.rs"]
mod pump_tests;

#[cfg(test)]
#[path = "ordered_empty_tests.rs"]
mod empty_tests;

#[cfg(test)]
#[path = "ordered_renewal_lifecycle_tests.rs"]
mod renewal_lifecycle_tests;

#[cfg(test)]
#[path = "ordered_late_open_tests.rs"]
mod late_open_tests;

#[cfg(test)]
#[path = "ordered_graph_tests.rs"]
mod graph_tests;

#[cfg(test)]
#[path = "ordered_batch_tests.rs"]
mod tests;
