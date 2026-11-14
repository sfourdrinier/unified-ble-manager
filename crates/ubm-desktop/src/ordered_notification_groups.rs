// crates/ubm-desktop/src/ordered_notification_groups.rs
//! Host registry for ordered collections fed directly from a peer source FIFO.
//!
//! The source pump owns serialization: it must call these methods in source
//! order before per-characteristic fanout, and install/remove exact membership
//! routes under the native source's register/retire fence. This registry does
//! not open a second platform stream or own physical CCCD subscriptions.
//! When a recipient cannot admit an observation, its exact occurrence claim
//! remains bounded here and the source transfer stays unacknowledged until
//! all captured recipients accept it. Retrying never revisits completed owners.

use std::collections::{HashMap, HashSet};

use ubm_core::central::NotificationProvenance;
use ubm_core::ordered_notifications::{
    EndReason, Error as CollectionError, LossScope, LossUnit, Member, Membership, ObservedDelivery,
    OccurrenceLease, OrderedCollection, Phase, Poll, Record, RetainedRecord, SourceGeneration,
    SourceRejection,
};

/// Preserve the canonical desktop identity and typed platform detail when an
/// ordered collection records a source or setup failure.
pub fn failure_from_desktop(
    error: &crate::errors::DesktopError,
) -> ubm_core::ordered_notifications::Failure {
    use ubm_core::ordered_notifications::{Failure, FailurePlatformDetail};
    // Convert iteratively: platform cleanup trees may originate outside the
    // collection's depth budget and must not exhaust the thread stack before
    // the collection can refuse them.
    enum Work<'a> {
        Failure(&'a crate::errors::DesktopError),
        Detail(&'a crate::errors::PlatformDetail),
        FinishFailure(Failure, usize, usize),
        FinishDetail(FailurePlatformDetail, usize, usize),
    }
    enum Converted {
        Failure(Failure),
        Detail(FailurePlatformDetail),
    }
    let mut work = vec![Work::Failure(error)];
    let mut finished = Vec::new();
    while let Some(item) = work.pop() {
        match item {
            Work::Failure(error) => {
                let platform = error.platform();
                let failure = Failure {
                    code: error.code_str().to_owned(),
                    domain: error.domain().as_str().to_owned(),
                    operation: error.operation().to_owned(),
                    detail: error.detail().map(str::to_owned),
                    platform_domain: platform.map(|detail| detail.domain.clone()),
                    platform_code: platform.map(|detail| detail.code.clone()),
                    platform_message: platform.and_then(|detail| detail.message.clone()),
                    platform_metadata: platform
                        .map(|detail| {
                            detail
                                .metadata
                                .iter()
                                .map(|(key, value)| (key.clone(), failure_value(value)))
                                .collect()
                        })
                        .unwrap_or_default(),
                    platform_metadata_entries: platform
                        .and_then(|detail| failure_entries(&detail.metadata_entries)),
                    platform_cleanup: Vec::new(),
                    platform_cleanup_failures: Vec::new(),
                };
                let details = platform.map_or(&[][..], |value| value.cleanup.as_slice());
                let causes = platform.map_or(&[][..], |value| value.cleanup_failures.as_slice());
                work.push(Work::FinishFailure(failure, details.len(), causes.len()));
                work.extend(causes.iter().rev().map(Work::Failure));
                work.extend(details.iter().rev().map(Work::Detail));
            }
            Work::Detail(detail) => {
                let converted = FailurePlatformDetail {
                    domain: detail.domain.clone(),
                    code: detail.code.clone(),
                    message: detail.message.clone(),
                    metadata: detail
                        .metadata
                        .iter()
                        .map(|(key, value)| (key.clone(), failure_value(value)))
                        .collect(),
                    metadata_entries: failure_entries(&detail.metadata_entries),
                    cleanup: Vec::new(),
                    cleanup_failures: Vec::new(),
                };
                work.push(Work::FinishDetail(
                    converted,
                    detail.cleanup.len(),
                    detail.cleanup_failures.len(),
                ));
                work.extend(detail.cleanup_failures.iter().rev().map(Work::Failure));
                work.extend(detail.cleanup.iter().rev().map(Work::Detail));
            }
            Work::FinishFailure(mut failure, detail_count, cause_count) => {
                let (details, causes) = take_children(&mut finished, detail_count, cause_count);
                failure.platform_cleanup = details;
                failure.platform_cleanup_failures = causes;
                finished.push(Converted::Failure(failure));
            }
            Work::FinishDetail(mut detail, detail_count, cause_count) => {
                let (details, causes) = take_children(&mut finished, detail_count, cause_count);
                detail.cleanup = details;
                detail.cleanup_failures = causes;
                finished.push(Converted::Detail(detail));
            }
        }
    }
    fn take_children(
        finished: &mut Vec<Converted>,
        detail_count: usize,
        cause_count: usize,
    ) -> (Vec<FailurePlatformDetail>, Vec<Failure>) {
        let mut causes = Vec::with_capacity(cause_count);
        for _ in 0..cause_count {
            let Some(Converted::Failure(cause)) = finished.pop() else {
                unreachable!("failure conversion stack")
            };
            causes.push(cause);
        }
        causes.reverse();
        let mut details = Vec::with_capacity(detail_count);
        for _ in 0..detail_count {
            let Some(Converted::Detail(detail)) = finished.pop() else {
                unreachable!("detail conversion stack")
            };
            details.push(detail);
        }
        details.reverse();
        (details, causes)
    }
    match finished.pop() {
        Some(Converted::Failure(value)) => value,
        _ => unreachable!("failure conversion root"),
    }
}

fn failure_value(
    value: &crate::errors::PlatformValue,
) -> ubm_core::ordered_notifications::FailureValue {
    use crate::errors::PlatformValue;
    use ubm_core::ordered_notifications::FailureValue;
    match value {
        PlatformValue::Int(value) => FailureValue::Int(*value),
        PlatformValue::Text(value) => FailureValue::Text(value.clone()),
        PlatformValue::Bool(value) => FailureValue::Bool(*value),
    }
}

fn failure_entries(
    entries: &Option<Vec<crate::errors::PlatformMetadataEntry>>,
) -> Option<Vec<ubm_core::ordered_notifications::FailureMetadataEntry>> {
    entries.as_ref().map(|entries| {
        entries
            .iter()
            .map(
                |entry| ubm_core::ordered_notifications::FailureMetadataEntry {
                    key: entry.key.clone(),
                    value: failure_value(&entry.value),
                },
            )
            .collect()
    })
}

/// Host identity for one actual native FIFO. A reconnect or a new native
/// group must use a distinct key, even if its ordinals restart at zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceKey {
    pub peer_id: String,
    pub generation: SourceGeneration,
    pub native_group: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    Capacity,
    DuplicateCollection,
    UnknownCollection,
    UnknownSourceMembership,
    SourceMismatch,
    CleanupPending,
    Collection(CollectionError),
}

impl From<CollectionError> for RegistryError {
    fn from(error: CollectionError) -> Self {
        Self::Collection(error)
    }
}

#[derive(Debug)]
struct Entry {
    source: SourceKey,
    owner: OrderedCollection,
    wake: Option<std::sync::Arc<tokio::sync::Notify>>,
}

/// The caller serializes access with native membership registration and FIFO
/// consumption. The fixed group bound prevents an idle consumer from growing
/// the host registry without limit.
#[derive(Debug, Default)]
pub struct OrderedGroupRegistry {
    entries: HashMap<u64, Entry>,
    routes: HashMap<(SourceKey, Membership), HashSet<u64>>,
    pending_terminals: HashMap<SourceKey, PendingTerminal>,
    pending_values: HashMap<SourceKey, PendingValue>,
    pending_losses: HashMap<SourceKey, PendingLoss>,
    pending_failures: HashMap<SourceKey, PendingFailure>,
}

#[derive(Debug)]
struct PendingFailure {
    failure: std::sync::Arc<ubm_core::ordered_notifications::Failure>,
    failure_member: Option<Membership>,
    tail_uncertain: bool,
    remaining: HashSet<u64>,
    wake: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct PendingValue {
    membership: Membership,
    ordinal: u64,
    value: Vec<u8>,
    delivery: ObservedDelivery,
    provenance: NotificationProvenance,
    remaining: HashMap<u64, Option<OccurrenceLease>>,
    wake: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct PendingLoss {
    members: Vec<Membership>,
    skipped_records: Option<u64>,
    remaining: HashMap<u64, Option<OccurrenceLease>>,
    wake: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
struct PendingTerminal {
    reason: EndReason,
    rejection: Option<SourceRejection>,
    failure_member: Option<Membership>,
    tail_uncertain: bool,
    remaining: HashSet<u64>,
    wake: std::sync::Arc<tokio::sync::Notify>,
}

fn collection_admission_failure(
    owner: &mut OrderedCollection,
    error: CollectionError,
    rejection: Option<SourceRejection>,
) -> Result<(), RegistryError> {
    let failure = crate::errors::DesktopError::new(
        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
        ubm_core::contracts::BleErrorDomain::Gatt,
        "gatt.notification.collection",
    )
    .with_detail(format!("ordered collection admission failed: {error:?}"));
    owner.source_terminal(
        EndReason::SourceFailed(failure_from_desktop(&failure)),
        rejection,
        None,
        true,
    )?;
    Ok(())
}

impl OrderedGroupRegistry {
    pub const MAX_COLLECTIONS: usize = 64;

    /// Install the logical route before dispatching any member enable. The
    /// caller must hold the same source-owner critical section that installs
    /// the native membership, so already-queued callbacks cannot be relabelled.
    pub fn install(
        &mut self,
        collection_id: u64,
        source: SourceKey,
        mut owner: OrderedCollection,
    ) -> Result<(), RegistryError> {
        if self.entries.contains_key(&collection_id) {
            return Err(RegistryError::DuplicateCollection);
        }
        if self.entries.len() >= Self::MAX_COLLECTIONS {
            return Err(RegistryError::Capacity);
        }
        if source.peer_id.is_empty()
            || source.generation != owner.source_generation
            || collection_id != owner.group_id
        {
            return Err(RegistryError::SourceMismatch);
        }
        owner.source_installed()?;
        for member in owner.members() {
            self.routes
                .entry((source.clone(), member.member.source))
                .or_default()
                .insert(collection_id);
        }
        self.entries.insert(
            collection_id,
            Entry {
                source,
                owner,
                wake: None,
            },
        );
        Ok(())
    }

    pub fn owns_source(&self, collection_id: u64, source: &SourceKey) -> bool {
        self.entries
            .get(&collection_id)
            .is_some_and(|entry| &entry.source == source)
    }

    /// Attach a bounded coalesced wake to one exact source and make any
    /// already-staged data observable to a newly attached consumer.
    pub fn set_wake(
        &mut self,
        collection_id: u64,
        source: &SourceKey,
        wake: std::sync::Arc<tokio::sync::Notify>,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        if &entry.source != source {
            return Err(RegistryError::SourceMismatch);
        }
        entry.wake = Some(wake.clone());
        wake.notify_one();
        Ok(())
    }

    /// Read collection status without producing a readiness signal.
    pub fn with_collection_silent<T>(
        &mut self,
        collection_id: u64,
        read: impl FnOnce(&mut OrderedCollection) -> Result<T, CollectionError>,
    ) -> Result<T, RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        read(&mut entry.owner).map_err(RegistryError::from)
    }

    /// Run one acquisition/cleanup transition on the unique collection owner.
    pub fn with_collection<T>(
        &mut self,
        collection_id: u64,
        update: impl FnOnce(&mut OrderedCollection) -> Result<T, CollectionError>,
    ) -> Result<T, RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        let result = update(&mut entry.owner).map_err(RegistryError::from);
        if let Some(wake) = &entry.wake {
            wake.notify_one();
        }
        if let Some(pending) = self.pending_terminals.get(&entry.source) {
            pending.wake.notify_one();
        }
        if let Some(pending) = self.pending_values.get(&entry.source) {
            pending.wake.notify_one();
        }
        if let Some(pending) = self.pending_losses.get(&entry.source) {
            pending.wake.notify_one();
        }
        if let Some(pending) = self.pending_failures.get(&entry.source) {
            pending.wake.notify_one();
        }
        result
    }

    /// Admit one native FIFO value without assigning a replacement ordinal.
    pub fn admit_value(
        &mut self,
        source: &SourceKey,
        membership: Membership,
        ordinal: u64,
        value: &[u8],
        delivery: ObservedDelivery,
        provenance: NotificationProvenance,
    ) -> Result<usize, RegistryError> {
        if self.pending_losses.contains_key(source) || self.pending_failures.contains_key(source) {
            return Err(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        if let Some(pending) = self.pending_values.get(source) {
            if pending.membership != membership
                || pending.ordinal != ordinal
                || pending.value != value
                || pending.delivery != delivery
                || pending.provenance != provenance
            {
                return Err(RegistryError::Collection(CollectionError::PendingCapacity));
            }
            return self.retry_source_value(source);
        }
        let Some(collections) = self.routes.get(&(source.clone(), membership)).cloned() else {
            return Ok(0);
        };
        let mut admitted = 0;
        let mut first_error = None;
        let mut remaining = HashMap::new();
        for id in collections {
            let Some(entry) = self.entries.get_mut(&id) else {
                first_error.get_or_insert(RegistryError::UnknownCollection);
                continue;
            };
            if entry.owner.phase() == Phase::Closing || entry.owner.phase() == Phase::Terminal {
                continue;
            }
            if let Some(wake) = &entry.wake {
                wake.notify_one();
            }
            match entry.owner.admit_value(
                &source.generation,
                id,
                membership,
                ordinal,
                value,
                delivery,
                provenance,
            ) {
                Ok(()) => admitted += 1,
                Err(CollectionError::PendingCapacity) => {
                    let record = RetainedRecord::Value {
                        membership,
                        ordinal,
                        value: value.to_vec(),
                        delivery,
                        provenance,
                    };
                    match entry
                        .owner
                        .retain_occurrence(&source.generation, id, record)
                    {
                        Ok(lease) => {
                            remaining.insert(id, Some(lease));
                        }
                        Err((CollectionError::PendingCapacity, _)) => {
                            remaining.insert(id, None);
                        }
                        Err((error, _)) => {
                            first_error.get_or_insert(RegistryError::Collection(error));
                        }
                    }
                }
                Err(error) => {
                    let bytes = u64::try_from(value.len()).map_err(|_| RegistryError::Capacity)?;
                    if let Err(error) = collection_admission_failure(
                        &mut entry.owner,
                        error,
                        Some(SourceRejection::Value { bytes }),
                    ) {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        if !remaining.is_empty() {
            self.pending_values.insert(
                source.clone(),
                PendingValue {
                    membership,
                    ordinal,
                    value: value.to_vec(),
                    delivery,
                    provenance,
                    remaining,
                    wake: std::sync::Arc::new(tokio::sync::Notify::new()),
                },
            );
            first_error.get_or_insert(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        first_error.map_or(Ok(admitted), Err)
    }

    pub(crate) fn retry_source_value(
        &mut self,
        source: &SourceKey,
    ) -> Result<usize, RegistryError> {
        let Some(mut pending) = self.pending_values.remove(source) else {
            return Ok(0);
        };
        let (admitted, error) =
            self.retry_claims(source, &mut pending.remaining, || RetainedRecord::Value {
                membership: pending.membership,
                ordinal: pending.ordinal,
                value: pending.value.clone(),
                delivery: pending.delivery,
                provenance: pending.provenance,
            });
        if pending.remaining.is_empty() {
            return Ok(admitted);
        }
        self.pending_values.insert(source.clone(), pending);
        Err(error.unwrap_or(RegistryError::Collection(CollectionError::PendingCapacity)))
    }

    fn retry_claims(
        &mut self,
        source: &SourceKey,
        remaining: &mut HashMap<u64, Option<OccurrenceLease>>,
        record: impl Fn() -> RetainedRecord,
    ) -> (usize, Option<RegistryError>) {
        fn record_error(slot: &mut Option<RegistryError>, next: RegistryError) {
            let pressure = RegistryError::Collection(CollectionError::PendingCapacity);
            if slot.is_none() || (*slot == Some(pressure) && next != pressure) {
                *slot = Some(next);
            }
        }
        let mut admitted = 0;
        let mut error = None;
        let mut completed = Vec::new();
        for (&id, lease) in remaining.iter_mut() {
            let Some(entry) = self.entries.get_mut(&id) else {
                record_error(&mut error, RegistryError::UnknownCollection);
                continue;
            };
            if lease.is_none() {
                match entry
                    .owner
                    .retain_occurrence(&source.generation, id, record())
                {
                    Ok(claim) => *lease = Some(claim),
                    Err((failure, _)) => {
                        record_error(&mut error, RegistryError::Collection(failure));
                        continue;
                    }
                }
            }
            match entry.owner.retry_occurrence(
                &source.generation,
                id,
                lease.as_mut().expect("retained occurrence"),
            ) {
                Ok(()) => {
                    admitted += 1;
                    completed.push(id);
                    if let Some(wake) = &entry.wake {
                        wake.notify_one();
                    }
                }
                Err(failure) => {
                    record_error(&mut error, RegistryError::Collection(failure));
                }
            }
        }
        for id in completed {
            remaining.remove(&id);
        }
        (admitted, error)
    }

    /// Reserve or release downstream capacity for one exact source marker.
    pub fn reserve_marker(
        &mut self,
        collection_id: u64,
        source: &SourceKey,
        marker_id: u64,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        if &entry.source != source {
            return Err(RegistryError::SourceMismatch);
        }
        entry
            .owner
            .reserve_marker(&source.generation, collection_id, marker_id)
            .map_err(RegistryError::from)
    }
    pub fn marker_reserved(
        &self,
        collection_id: u64,
        source: &SourceKey,
        marker_id: u64,
    ) -> Result<bool, RegistryError> {
        let entry = self
            .entries
            .get(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        if &entry.source != source {
            return Err(RegistryError::SourceMismatch);
        }
        Ok(entry.owner.marker_reserved(marker_id))
    }
    pub fn release_marker(
        &mut self,
        collection_id: u64,
        source: &SourceKey,
        marker_id: u64,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        if &entry.source != source {
            return Err(RegistryError::SourceMismatch);
        }
        entry
            .owner
            .release_marker(marker_id)
            .map_err(RegistryError::from)
    }

    /// Admit a source-owned marker once to the exact native group.
    pub fn admit_marker(
        &mut self,
        source: &SourceKey,
        marker_id: u64,
        ordinal: u64,
    ) -> Result<usize, RegistryError> {
        if self.pending_failures.contains_key(source) {
            return Err(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        let mut admitted = 0;
        for (&id, entry) in &mut self.entries {
            if &entry.source == source
                && !matches!(entry.owner.phase(), Phase::Closing | Phase::Terminal)
            {
                entry
                    .owner
                    .admit_marker(&source.generation, id, marker_id, ordinal)?;
                if let Some(wake) = &entry.wake {
                    wake.notify_one();
                }
                admitted += 1;
            }
        }
        Ok(admitted)
    }

    /// One source-wide gap is delivered once to each collection represented
    /// by the immutable source-time membership snapshot, never once per member.
    pub fn admit_source_loss(
        &mut self,
        source: &SourceKey,
        members: &[Membership],
        skipped_records: Option<u64>,
    ) -> Result<usize, RegistryError> {
        if self.pending_values.contains_key(source) || self.pending_failures.contains_key(source) {
            return Err(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        if let Some(pending) = self.pending_losses.get(source) {
            if pending.members != members || pending.skipped_records != skipped_records {
                return Err(RegistryError::Collection(CollectionError::PendingCapacity));
            }
            return self.retry_source_loss(source);
        }
        let collections = self.collections_for(source, members);
        let mut admitted = 0;
        let mut first_error = None;
        let mut remaining = HashMap::new();
        for id in collections {
            let Some(entry) = self.entries.get_mut(&id) else {
                first_error.get_or_insert(RegistryError::UnknownCollection);
                continue;
            };
            if entry.owner.phase() == Phase::Closing || entry.owner.phase() == Phase::Terminal {
                continue;
            }
            if let Some(wake) = &entry.wake {
                wake.notify_one();
            }
            match entry.owner.admit_loss(
                &source.generation,
                id,
                LossScope::Unknown,
                LossUnit::SourceRecords,
                skipped_records,
                None,
            ) {
                Ok(()) => admitted += 1,
                Err(CollectionError::PendingCapacity) => {
                    let record = RetainedRecord::Loss {
                        scope: LossScope::Unknown,
                        unit: LossUnit::SourceRecords,
                        count: skipped_records,
                        bytes: None,
                    };
                    match entry
                        .owner
                        .retain_occurrence(&source.generation, id, record)
                    {
                        Ok(lease) => {
                            remaining.insert(id, Some(lease));
                        }
                        Err((CollectionError::PendingCapacity, _)) => {
                            remaining.insert(id, None);
                        }
                        Err((error, _)) => {
                            first_error.get_or_insert(RegistryError::Collection(error));
                        }
                    }
                }
                Err(error) => {
                    let rejection = skipped_records
                        .map(|skipped_records| SourceRejection::Loss { skipped_records });
                    if let Err(error) =
                        collection_admission_failure(&mut entry.owner, error, rejection)
                    {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        if !remaining.is_empty() {
            self.pending_losses.insert(
                source.clone(),
                PendingLoss {
                    members: members.to_vec(),
                    skipped_records,
                    remaining,
                    wake: std::sync::Arc::new(tokio::sync::Notify::new()),
                },
            );
            first_error.get_or_insert(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        first_error.map_or(Ok(admitted), Err)
    }

    fn retry_source_loss(&mut self, source: &SourceKey) -> Result<usize, RegistryError> {
        let Some(mut pending) = self.pending_losses.remove(source) else {
            return Ok(0);
        };
        let (admitted, error) =
            self.retry_claims(source, &mut pending.remaining, || RetainedRecord::Loss {
                scope: LossScope::Unknown,
                unit: LossUnit::SourceRecords,
                count: pending.skipped_records,
                bytes: None,
            });
        if pending.remaining.is_empty() {
            return Ok(admitted);
        }
        self.pending_losses.insert(source.clone(), pending);
        Err(error.unwrap_or(RegistryError::Collection(CollectionError::PendingCapacity)))
    }

    /// Retain one nonterminal failure for exactly its source-time recipients.
    /// The source retains its shared evidence and retries via
    /// `retry_pending_source`, acknowledging only after admission succeeds.
    pub fn admit_source_failure(
        &mut self,
        source: &SourceKey,
        members: &[Membership],
        failure: &std::sync::Arc<ubm_core::ordered_notifications::Failure>,
        failure_member: Option<Membership>,
        tail_uncertain: bool,
    ) -> Result<usize, RegistryError> {
        if self.pending_values.contains_key(source)
            || self.pending_losses.contains_key(source)
            || self.pending_terminals.contains_key(source)
            || self.pending_failures.contains_key(source)
        {
            return Err(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        let remaining = self.collections_for(source, members);
        if remaining.is_empty() {
            return Ok(0);
        }
        self.pending_failures.insert(
            source.clone(),
            PendingFailure {
                failure: failure.clone(),
                failure_member,
                tail_uncertain,
                remaining,
                wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            },
        );
        self.retry_source_failure(source)
    }

    fn retry_source_failure(&mut self, source: &SourceKey) -> Result<usize, RegistryError> {
        let Some(mut pending) = self.pending_failures.remove(source) else {
            return Ok(0);
        };
        let mut admitted = 0;
        let mut error = None;
        for id in pending.remaining.clone() {
            let Some(entry) = self.entries.get_mut(&id) else {
                error.get_or_insert(RegistryError::UnknownCollection);
                continue;
            };
            let result = entry
                .owner
                .validate_failure_evidence(&pending.failure)
                .and_then(|()| {
                    entry.owner.admit_failure_record(
                        &source.generation,
                        id,
                        Record::FailureObservation {
                            failure: Some(pending.failure.as_ref().clone()),
                            source_rejection: None,
                            source_failure_member: pending.failure_member,
                            rejected_loss: None,
                            tail_uncertain: pending.tail_uncertain,
                        },
                    )
                });
            match result {
                Ok(()) => {
                    pending.remaining.remove(&id);
                    admitted += 1;
                    if let Some(wake) = &entry.wake {
                        wake.notify_one();
                    }
                }
                Err(failure) => {
                    error.get_or_insert(RegistryError::Collection(failure));
                }
            }
        }
        if !pending.remaining.is_empty() {
            self.pending_failures.insert(source.clone(), pending);
        }
        error.map_or(Ok(admitted), Err)
    }

    /// A source terminal applies only to collections in its source-time
    /// membership snapshot. Rejected source records remain source-wide.
    pub fn source_terminal(
        &mut self,
        source: &SourceKey,
        members: &[Membership],
        reason: EndReason,
        rejection: Option<SourceRejection>,
        failure_member: Option<Membership>,
        tail_uncertain: bool,
    ) -> Result<usize, RegistryError> {
        if self.pending_terminals.contains_key(source)
            || self.pending_failures.contains_key(source)
            || self.pending_values.contains_key(source)
            || self.pending_losses.contains_key(source)
        {
            return Err(RegistryError::Collection(CollectionError::PendingCapacity));
        }
        let remaining = self.collections_for(source, members);
        if remaining.is_empty() {
            return Ok(0);
        }
        self.pending_terminals.insert(
            source.clone(),
            PendingTerminal {
                reason,
                rejection,
                failure_member,
                tail_uncertain,
                remaining,
                wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            },
        );
        self.retry_source_terminal(source)
    }

    /// Retry the same source occurrence against only its outstanding owners.
    /// The original reason and collection IDs captured on admission stay
    /// owned here while any collection refuses admission.
    pub fn retry_source_terminal(&mut self, source: &SourceKey) -> Result<usize, RegistryError> {
        let Some(mut pending) = self.pending_terminals.remove(source) else {
            return Ok(0);
        };
        let mut ended = 0;
        let mut first_error = None;
        for id in pending.remaining.clone() {
            let Some(entry) = self.entries.get_mut(&id) else {
                first_error.get_or_insert(RegistryError::UnknownCollection);
                continue;
            };
            // A terminal must never be cloned until the recipient has checked
            // both its allocation budget and structural depth.
            let validation = match &pending.reason {
                EndReason::SetupFailed(failure)
                | EndReason::SourceFailed(failure)
                | EndReason::HostReset(failure) => entry.owner.validate_failure_evidence(failure),
                _ => Ok(()),
            };
            let result = validation.and_then(|()| {
                if entry.owner.phase() == Phase::Closing
                    && let EndReason::SourceFailed(failure) = &pending.reason
                {
                    return entry.owner.admit_failure_record(
                        &source.generation,
                        id,
                        Record::FailureObservation {
                            failure: Some(failure.clone()),
                            source_rejection: pending.rejection,
                            source_failure_member: pending.failure_member,
                            rejected_loss: None,
                            tail_uncertain: pending.tail_uncertain,
                        },
                    );
                }
                entry.owner.source_terminal(
                    pending.reason.clone(),
                    pending.rejection,
                    pending.failure_member,
                    pending.tail_uncertain,
                )
            });
            match result {
                Ok(()) => {
                    pending.remaining.remove(&id);
                    ended += 1;
                    if let Some(wake) = &entry.wake {
                        wake.notify_one();
                    }
                }
                Err(error) => {
                    first_error.get_or_insert(RegistryError::Collection(error));
                }
            }
        }
        if !pending.remaining.is_empty() {
            self.pending_terminals.insert(source.clone(), pending);
        }
        first_error.map_or(Ok(ended), Err)
    }

    pub(crate) fn pending_source_wake(
        &self,
        source: &SourceKey,
    ) -> Option<std::sync::Arc<tokio::sync::Notify>> {
        self.pending_values
            .get(source)
            .map(|pending| pending.wake.clone())
            .or_else(|| {
                self.pending_losses
                    .get(source)
                    .map(|pending| pending.wake.clone())
            })
            .or_else(|| {
                self.pending_failures
                    .get(source)
                    .map(|pending| pending.wake.clone())
            })
            .or_else(|| {
                self.pending_terminals
                    .get(source)
                    .map(|pending| pending.wake.clone())
            })
    }

    pub(crate) fn retry_pending_source(
        &mut self,
        source: &SourceKey,
    ) -> Result<usize, RegistryError> {
        if self.pending_values.contains_key(source) {
            self.retry_source_value(source)
        } else if self.pending_losses.contains_key(source) {
            self.retry_source_loss(source)
        } else if self.pending_failures.contains_key(source) {
            self.retry_source_failure(source)
        } else {
            self.retry_source_terminal(source)
        }
    }

    /// Consume a native membership fence only after every earlier source
    /// record has passed this registry. Unrelated collection routes survive.
    pub fn retire_membership(&mut self, source: &SourceKey, membership: Membership) -> bool {
        self.routes.remove(&(source.clone(), membership)).is_some()
    }

    /// Install the new route only after the source pump consumed the old fence.
    /// The caller holds the source registration lock through this transition.
    pub fn rebind_member(
        &mut self,
        collection_id: u64,
        source: &SourceKey,
        id: u64,
        old: Membership,
        new: Membership,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        if &entry.source != source {
            return Err(RegistryError::SourceMismatch);
        }
        if self
            .routes
            .get(&(source.clone(), old))
            .is_some_and(|ids| ids.contains(&collection_id))
        {
            return Err(RegistryError::UnknownSourceMembership);
        }
        entry.owner.rebind_member(id, old, new)?;
        self.routes
            .entry((source.clone(), new))
            .or_default()
            .insert(collection_id);
        Ok(())
    }

    pub fn poll(&mut self, collection_id: u64) -> Result<Poll, RegistryError> {
        let entry = self
            .entries
            .get_mut(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        let pending = self
            .pending_terminals
            .get(&entry.source)
            .filter(|pending| pending.remaining.contains(&collection_id));
        let unsecured = self
            .pending_values
            .get(&entry.source)
            .is_some_and(|pending| {
                pending
                    .remaining
                    .get(&collection_id)
                    .is_some_and(Option::is_none)
            })
            || self
                .pending_losses
                .get(&entry.source)
                .is_some_and(|pending| {
                    pending
                        .remaining
                        .get(&collection_id)
                        .is_some_and(Option::is_none)
                });
        let failure_pending = self
            .pending_failures
            .get(&entry.source)
            .is_some_and(|pending| pending.remaining.contains(&collection_id));
        if (pending.is_some() || unsecured || failure_pending)
            && entry.owner.phase() == Phase::Closing
            && !entry.owner.has_pending_records()
        {
            return Ok(Poll::Empty);
        }
        let result = entry.owner.poll();
        if let Some(pending) = pending {
            pending.wake.notify_one();
        }
        if let Some(pending) = self.pending_values.get(&entry.source) {
            pending.wake.notify_one();
        }
        if let Some(pending) = self.pending_losses.get(&entry.source) {
            pending.wake.notify_one();
        }
        if let Some(pending) = self.pending_failures.get(&entry.source) {
            pending.wake.notify_one();
        }
        Ok(result)
    }

    /// Removing an entry never discards outstanding physical cleanup debt or
    /// unconsumed values/terminal; the host closes and drains it first.
    pub fn remove(&mut self, collection_id: u64) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get(&collection_id)
            .ok_or(RegistryError::UnknownCollection)?;
        if self
            .pending_terminals
            .values()
            .any(|pending| pending.remaining.contains(&collection_id))
            || self
                .pending_values
                .values()
                .any(|pending| pending.remaining.contains_key(&collection_id))
            || self
                .pending_losses
                .values()
                .any(|pending| pending.remaining.contains_key(&collection_id))
            || self
                .pending_failures
                .values()
                .any(|pending| pending.remaining.contains(&collection_id))
            || !entry.owner.cleanup_complete()
            || entry.owner.phase() != Phase::Terminal
            || entry.owner.has_pending_records()
        {
            return Err(RegistryError::CleanupPending);
        }
        let source = entry.source.clone();
        let members: Vec<Member> = entry
            .owner
            .members()
            .iter()
            .map(|state| state.member)
            .collect();
        self.entries.remove(&collection_id);
        for member in members {
            let key = (source.clone(), member.source);
            if let Some(ids) = self.routes.get_mut(&key) {
                ids.remove(&collection_id);
                if ids.is_empty() {
                    self.routes.remove(&key);
                }
            }
        }
        Ok(())
    }

    fn collections_for(&self, source: &SourceKey, members: &[Membership]) -> HashSet<u64> {
        members
            .iter()
            .filter_map(|member| self.routes.get(&(source.clone(), *member)))
            .flat_map(|ids| ids.iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_core::ordered_notifications::{DeliveryMode, Record};

    fn member(id: u64, epoch: u64) -> Member {
        Member {
            id,
            source: Membership { member: 5, epoch },
            delivery: DeliveryMode::Notification,
        }
    }

    fn collection(id: u64, members: Vec<Member>) -> OrderedCollection {
        OrderedCollection::new(
            "conn".into(),
            "db".into(),
            SourceGeneration::Counter(4),
            id,
            members,
            8,
            1024,
            4,
        )
        .unwrap()
    }

    fn source() -> SourceKey {
        SourceKey {
            peer_id: "peer".into(),
            generation: SourceGeneration::Counter(4),
            native_group: 17,
        }
    }

    #[test]
    fn capacity_pressure_does_not_turn_an_owed_value_into_a_source_failure() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let shared = member(10, 1);
        let small = OrderedCollection::new(
            "conn".into(),
            "db".into(),
            source.generation.clone(),
            1,
            vec![shared],
            1,
            1,
            4,
        )
        .unwrap();
        registry.install(1, source.clone(), small).unwrap();
        registry
            .with_collection(1, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        registry
            .admit_value(
                &source,
                shared.source,
                1,
                &[42],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown,
            )
            .unwrap();
        assert_eq!(
            registry.admit_value(
                &source,
                shared.source,
                2,
                &[42],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Err(RegistryError::Collection(CollectionError::PendingCapacity)),
        );
        assert!(matches!(
            registry.poll(1).unwrap(),
            Poll::Record(Record::Value { ordinal: 1, .. })
        ));
        assert!(matches!(registry.poll(1).unwrap(), Poll::Empty));
    }

    #[test]
    fn pressure_retry_delivers_once_to_each_original_recipient() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let shared = member(10, 1);
        for (id, bytes) in [(1, 1), (2, 8)] {
            let owner = OrderedCollection::new(
                "conn".into(),
                "db".into(),
                source.generation.clone(),
                id,
                vec![shared],
                4,
                bytes,
                4,
            )
            .unwrap();
            registry.install(id, source.clone(), owner).unwrap();
            registry
                .with_collection(id, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(id, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
        let send = |registry: &mut OrderedGroupRegistry| {
            registry.admit_value(
                &source,
                shared.source,
                7,
                &[1, 2],
                ObservedDelivery::Unknown,
                NotificationProvenance::Notification,
            )
        };
        assert_eq!(
            send(&mut registry),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        assert!(matches!(
            registry.poll(2).unwrap(),
            Poll::Record(Record::Value { ordinal: 7, .. })
        ));
        assert_eq!(
            send(&mut registry),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        assert!(matches!(registry.poll(2).unwrap(), Poll::Empty));
        registry
            .install(3, source.clone(), collection(3, vec![shared]))
            .unwrap();
        registry
            .with_collection(3, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(3, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        registry
            .with_collection(1, |owner| {
                owner.increase_byte_capacity(&source.generation, 1, 8)
            })
            .unwrap();
        send(&mut registry).unwrap();
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::Value { ordinal: 7, value, .. }) if value == [1, 2])
        );
        assert!(matches!(registry.poll(2).unwrap(), Poll::Empty));
        assert!(matches!(registry.poll(3).unwrap(), Poll::Empty));
        registry
            .admit_value(
                &source,
                shared.source,
                8,
                &[1, 2],
                ObservedDelivery::Unknown,
                NotificationProvenance::Notification,
            )
            .unwrap();
        for id in [1, 2, 3] {
            assert!(
                matches!(registry.poll(id).unwrap(), Poll::Record(Record::Value { ordinal: 8, value, .. }) if value == [1, 2])
            );
            assert!(matches!(registry.poll(id).unwrap(), Poll::Empty));
        }
    }

    #[test]
    fn retained_pressure_claim_survives_collection_close() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let shared = member(10, 1);
        let owner = OrderedCollection::new(
            "conn".into(),
            "db".into(),
            source.generation.clone(),
            1,
            vec![shared],
            4,
            1,
            4,
        )
        .unwrap();
        registry.install(1, source.clone(), owner).unwrap();
        registry
            .with_collection(1, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        assert_eq!(
            registry.admit_value(
                &source,
                shared.source,
                4,
                &[1, 2],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        registry
            .with_collection(1, |owner| owner.close(EndReason::Closed, false))
            .unwrap();
        assert!(matches!(registry.poll(1).unwrap(), Poll::Empty));
        registry
            .with_collection(1, |owner| {
                owner.increase_byte_capacity(&source.generation, 1, 4)
            })
            .unwrap();
        registry.retry_source_value(&source).unwrap();
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::Value { ordinal: 4, value, .. }) if value == [1, 2])
        );
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Terminal(terminal) if terminal.reason == EndReason::Closed)
        );
    }

    #[test]
    fn loss_pressure_retains_the_original_source_observation() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let shared = member(10, 1);
        let owner = OrderedCollection::new(
            "conn".into(),
            "db".into(),
            source.generation.clone(),
            1,
            vec![shared],
            4,
            1024,
            1,
        )
        .unwrap();
        registry.install(1, source.clone(), owner).unwrap();
        registry
            .with_collection(1, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        registry
            .admit_source_loss(&source, &[shared.source], Some(2))
            .unwrap();
        assert_eq!(
            registry.admit_source_loss(&source, &[shared.source], Some(3)),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        assert!(matches!(
            registry.poll(1).unwrap(),
            Poll::Record(Record::Loss { count: Some(2), .. })
        ));
        registry
            .admit_source_loss(&source, &[shared.source], Some(3))
            .unwrap();
        assert!(matches!(
            registry.poll(1).unwrap(),
            Poll::Record(Record::Loss { count: Some(3), .. })
        ));
        assert!(matches!(registry.poll(1).unwrap(), Poll::Empty));
    }

    #[test]
    fn removal_keeps_late_evidence_after_terminal_and_cleanup() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let member = member(10, 1);
        registry
            .install(1, source.clone(), collection(1, vec![member]))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.close(EndReason::Closed, false))
            .unwrap();
        assert!(matches!(registry.poll(1).unwrap(), Poll::Terminal(_)));
        registry
            .with_collection(1, |owner| owner.begin_release(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.release_result(10, Ok(())))
            .unwrap();
        registry
            .with_collection(1, |owner| {
                owner.admit_failure_record(
                    &source.generation,
                    1,
                    Record::FailureObservationsOmitted { count: None },
                )
            })
            .unwrap();
        assert_eq!(registry.remove(1), Err(RegistryError::CleanupPending));
        assert!(matches!(
            registry.poll(1).unwrap(),
            Poll::Record(Record::FailureObservationsOmitted { count: None })
        ));
        registry.remove(1).unwrap();
    }

    #[test]
    fn counter_and_opaque_generations_are_distinct_source_identities() {
        let mut registry = OrderedGroupRegistry::default();
        let counter = source();
        let opaque = SourceKey {
            generation: SourceGeneration::Opaque("4".into()),
            ..counter.clone()
        };
        let shared = member(10, 1);
        registry
            .install(1, counter.clone(), collection(1, vec![shared]))
            .unwrap();
        let mut opaque_owner = collection(2, vec![shared]);
        opaque_owner.source_generation = opaque.generation.clone();
        registry.install(2, opaque.clone(), opaque_owner).unwrap();
        for id in [1, 2] {
            registry
                .with_collection(id, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(id, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
        assert_eq!(
            registry.admit_value(
                &counter,
                shared.source,
                1,
                &[7],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Ok(1)
        );
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::Value { value, .. }) if value == [7])
        );
        assert!(matches!(registry.poll(2).unwrap(), Poll::Empty));
        assert_eq!(
            registry.admit_value(
                &opaque,
                shared.source,
                2,
                &[8],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Ok(1)
        );
        assert!(
            matches!(registry.poll(2).unwrap(), Poll::Record(Record::Value { value, .. }) if value == [8])
        );
    }

    #[test]
    fn one_collection_admission_failure_does_not_skip_another() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let shared = member(10, 1);
        for id in [1, 2] {
            registry
                .install(id, source.clone(), collection(id, vec![shared]))
                .unwrap();
            registry
                .with_collection(id, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(id, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
        registry
            .with_collection(1, |owner| {
                owner.source_generation = SourceGeneration::Counter(999);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            registry.admit_value(
                &source,
                shared.source,
                7,
                &[42],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Ok(1)
        );
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Terminal(terminal) if matches!(terminal.reason, EndReason::SourceFailed(_)) && terminal.source_rejection == Some(SourceRejection::Value { bytes: 1 }))
        );
        assert!(
            matches!(registry.poll(2).unwrap(), Poll::Record(Record::Value { ordinal: 7, value, .. }) if value == [42])
        );
    }

    #[test]
    fn rebind_after_fence_preserves_fifo_and_rejects_stale_membership() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let old = member(10, 1);
        let new = Membership {
            member: old.source.member,
            epoch: old.source.epoch + 1,
        };
        registry
            .install(1, source.clone(), collection(1, vec![old]))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        assert!(
            registry
                .rebind_member(1, &source, 10, old.source, new)
                .is_err()
        );
        assert_eq!(
            registry.admit_value(
                &source,
                old.source,
                1,
                &[1],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Ok(1)
        );
        assert!(registry.retire_membership(&source, old.source));
        registry
            .rebind_member(1, &source, 10, old.source, new)
            .unwrap();
        assert_eq!(
            registry.admit_value(
                &source,
                old.source,
                2,
                &[9],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Ok(0)
        );
        assert_eq!(
            registry.admit_value(
                &source,
                new,
                3,
                &[2],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown
            ),
            Ok(1)
        );
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::Value { ordinal: 1, member: 10, membership, value, .. }) if membership == old.source && value == [1])
        );
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::Value { ordinal: 3, member: 10, membership, value, .. }) if membership == new && value == [2])
        );
    }

    #[test]
    fn exact_membership_and_source_snapshot_preserve_group_order_and_uncertainty() {
        let mut registry = OrderedGroupRegistry::default();
        let source = source();
        let first = member(10, 1);
        let replacement = member(20, 2);
        registry
            .install(1, source.clone(), collection(1, vec![first]))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .admit_value(
                &source,
                first.source,
                3,
                &[1],
                ObservedDelivery::Unknown,
                NotificationProvenance::ReadOrNotification,
            )
            .unwrap();
        assert_eq!(
            registry.poll(1).unwrap(),
            Poll::Empty,
            "setup value stays staged"
        );
        registry
            .with_collection(1, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        registry
            .install(2, source.clone(), collection(2, vec![replacement]))
            .unwrap();
        registry
            .with_collection(2, |owner| owner.begin_enable(20))
            .unwrap();
        registry
            .with_collection(2, |owner| owner.enable_result(20, Some(true)))
            .unwrap();
        assert_eq!(
            registry
                .admit_source_loss(&source, &[first.source, first.source], Some(u64::MAX))
                .unwrap(),
            1
        );
        registry
            .admit_value(
                &source,
                replacement.source,
                4,
                &[2],
                ObservedDelivery::Notification,
                NotificationProvenance::Notification,
            )
            .unwrap();
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::Value { ordinal: 3, value, .. }) if value == [1])
        );
        assert!(matches!(
            registry.poll(1).unwrap(),
            Poll::Record(Record::Loss {
                unit: LossUnit::SourceRecords,
                count: Some(u64::MAX),
                ..
            })
        ));
        assert!(
            matches!(registry.poll(2).unwrap(), Poll::Record(Record::Value { ordinal: 4, value, .. }) if value == [2])
        );
        assert_eq!(
            registry.poll(2).unwrap(),
            Poll::Empty,
            "old membership gap must not affect replacement"
        );
        let reason = EndReason::SourceOverflow;
        let rejection = Some(SourceRejection::Loss { skipped_records: 9 });
        assert_eq!(
            registry
                .source_terminal(
                    &source,
                    &[first.source],
                    reason.clone(),
                    rejection,
                    None,
                    true
                )
                .unwrap(),
            1
        );
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Terminal(terminal) if terminal.reason == reason && terminal.source_rejection == rejection && terminal.tail_uncertain)
        );
        assert_eq!(registry.poll(2).unwrap(), Poll::Empty);
        assert_eq!(registry.remove(1), Err(RegistryError::CleanupPending));
        registry
            .with_collection(1, |owner| owner.begin_release(10))
            .unwrap();
        registry
            .with_collection(1, |owner| owner.release_result(10, Ok(())))
            .unwrap();
        registry.remove(1).unwrap();
        assert_eq!(
            registry
                .admit_value(
                    &source,
                    first.source,
                    5,
                    &[3],
                    ObservedDelivery::Unknown,
                    NotificationProvenance::Unknown
                )
                .unwrap(),
            0
        );
    }
    #[test]
    fn source_failure_occurrence_waits_for_each_original_recipient_without_ending_source() {
        let mut registry = OrderedGroupRegistry::default();
        let key = source();
        let shared = member(10, 3);
        for (id, controls) in [(1, 1), (2, 4)] {
            let owner = OrderedCollection::new(
                "conn".into(),
                "db".into(),
                key.generation.clone(),
                id,
                vec![shared],
                8,
                1024,
                controls,
            )
            .unwrap();
            registry.install(id, key.clone(), owner).unwrap();
            registry
                .with_collection(id, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(id, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
        registry
            .admit_source_loss(&key, &[shared.source], Some(1))
            .unwrap();
        assert!(matches!(
            registry.poll(2).unwrap(),
            Poll::Record(Record::Loss { .. })
        ));
        let failure = std::sync::Arc::new(failure_from_desktop(
            &crate::errors::DesktopError::read_failed("original"),
        ));
        assert_eq!(
            registry.admit_source_failure(
                &key,
                &[shared.source],
                &failure,
                Some(shared.source),
                true
            ),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        assert_eq!(
            registry.admit_marker(&key, 99, 2),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        assert_eq!(
            registry.source_terminal(&key, &[shared.source], EndReason::Closed, None, None, false),
            Err(RegistryError::Collection(CollectionError::PendingCapacity))
        );
        assert!(
            matches!(registry.poll(2).unwrap(), Poll::Record(Record::FailureObservation { failure: Some(found), .. }) if found == *failure)
        );
        registry
            .install(3, key.clone(), collection(3, vec![shared]))
            .unwrap();
        assert!(matches!(
            registry.poll(1).unwrap(),
            Poll::Record(Record::Loss { .. })
        ));
        assert_eq!(registry.retry_pending_source(&key), Ok(1));
        assert!(
            matches!(registry.poll(1).unwrap(), Poll::Record(Record::FailureObservation { failure: Some(found), source_failure_member: Some(found_member), .. }) if found == *failure && found_member == shared.source)
        );
        assert_eq!(registry.poll(2).unwrap(), Poll::Empty);
        assert_eq!(registry.poll(3).unwrap(), Poll::Empty);
        registry
            .admit_value(
                &key,
                shared.source,
                1,
                &[7],
                ObservedDelivery::Unknown,
                NotificationProvenance::Unknown,
            )
            .unwrap();
        for id in [1, 2] {
            assert!(matches!(
                registry.poll(id).unwrap(),
                Poll::Record(Record::Value { ordinal: 1, .. })
            ));
        }
    }

    #[test]
    fn identical_source_failures_remain_distinct_observations() {
        let mut registry = OrderedGroupRegistry::default();
        let key = source();
        let membership = member(10, 3).source;
        for id in [1, 2] {
            registry
                .install(id, key.clone(), collection(id, vec![member(10, 3)]))
                .unwrap();
        }
        let reason = EndReason::SourceFailed(failure_from_desktop(
            &crate::errors::DesktopError::read_failed("same"),
        ));
        assert_eq!(
            registry.source_terminal(&key, &[membership], reason.clone(), None, None, true),
            Ok(2)
        );
        assert_eq!(
            registry.source_terminal(&key, &[membership], reason.clone(), None, None, true),
            Ok(2)
        );
        for id in [1, 2] {
            assert!(
                matches!(registry.poll(id).unwrap(), Poll::Record(Record::FailureObservation { failure: Some(failure), .. }) if matches!(&reason, EndReason::SourceFailed(expected) if &failure == expected))
            );
            assert!(
                matches!(registry.poll(id).unwrap(), Poll::Terminal(terminal) if terminal.reason == reason)
            );
            assert!(matches!(registry.poll(id).unwrap(), Poll::Closed));
        }
    }

    #[test]
    fn deep_terminal_is_rejected_before_recursive_clone() {
        use ubm_core::ordered_notifications::{Failure, failure_allocation_cost};
        let mut failure = Failure {
            code: "leaf".into(),
            domain: String::new(),
            operation: String::new(),
            detail: None,
            platform_domain: None,
            platform_code: None,
            platform_message: None,
            platform_metadata: Default::default(),
            platform_metadata_entries: None,
            platform_cleanup: Vec::new(),
            platform_cleanup_failures: Vec::new(),
        };
        for _ in 0..20_000 {
            failure = Failure {
                code: String::new(),
                domain: String::new(),
                operation: String::new(),
                detail: None,
                platform_domain: None,
                platform_code: None,
                platform_message: None,
                platform_metadata: Default::default(),
                platform_metadata_entries: None,
                platform_cleanup: Vec::new(),
                platform_cleanup_failures: vec![failure],
            };
        }
        assert_eq!(failure_allocation_cost(&failure), usize::MAX);
        let mut registry = OrderedGroupRegistry::default();
        let key = source();
        registry
            .install(1, key.clone(), collection(1, vec![member(10, 3)]))
            .unwrap();
        let result = registry.source_terminal(
            &key,
            &[member(10, 3).source],
            EndReason::SourceFailed(failure),
            None,
            None,
            true,
        );
        assert_eq!(
            result,
            Err(RegistryError::Collection(CollectionError::RecordTooLarge))
        );
        assert!(matches!(registry.poll(1).unwrap(), Poll::Empty));
    }
}

#[cfg(test)]
mod error_mapping_tests {
    use super::failure_from_desktop;
    use crate::errors::{DesktopError, PlatformDetail, PlatformValue};
    use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
    use ubm_core::ordered_notifications::FailureValue;

    #[test]
    fn source_failure_preserves_typed_platform_identity() {
        let source = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Gatt,
            "gatt.notification.source",
        )
        .with_detail("source closed")
        .with_platform(
            PlatformDetail::new("native", "42")
                .with_message("refused")
                .with_metadata("count", PlatformValue::Int(7))
                .with_metadata("label", PlatformValue::Text("sample".into()))
                .with_metadata("retry", PlatformValue::Bool(false)),
        );
        let mapped = failure_from_desktop(&source);
        assert_eq!(mapped.code, source.code_str());
        assert_eq!(mapped.domain, source.domain().as_str());
        assert_eq!(mapped.operation, "gatt.notification.source");
        assert_eq!(mapped.detail.as_deref(), Some("source closed"));
        assert_eq!(mapped.platform_domain.as_deref(), Some("native"));
        assert_eq!(mapped.platform_code.as_deref(), Some("42"));
        assert_eq!(mapped.platform_message.as_deref(), Some("refused"));
        assert_eq!(
            mapped.platform_metadata.get("count"),
            Some(&FailureValue::Int(7))
        );
        assert_eq!(
            mapped.platform_metadata.get("label"),
            Some(&FailureValue::Text("sample".into()))
        );
        assert_eq!(
            mapped.platform_metadata.get("retry"),
            Some(&FailureValue::Bool(false))
        );
    }

    #[test]
    fn conversion_preserves_cleanup_tree_order_and_identity() {
        let mut first = PlatformDetail::new("native", "first");
        first.cleanup.push(PlatformDetail::new("nested", "child"));
        first
            .cleanup_failures
            .push(DesktopError::read_failed("detail cause"));
        let mut platform = PlatformDetail::new("native", "root");
        platform.cleanup = vec![first, PlatformDetail::new("native", "second")];
        platform.cleanup_failures = vec![
            DesktopError::read_failed("first cause"),
            DesktopError::subscribe_failed("second cause"),
        ];
        let source = DesktopError::read_failed("primary").with_platform(platform);
        let value = failure_from_desktop(&source);
        assert_eq!(
            value
                .platform_cleanup
                .iter()
                .map(|item| item.code.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(value.platform_cleanup[0].cleanup[0].code, "child");
        assert_eq!(
            value.platform_cleanup[0].cleanup_failures[0]
                .detail
                .as_deref(),
            Some("detail cause")
        );
        assert_eq!(
            value
                .platform_cleanup_failures
                .iter()
                .map(|item| item.detail.as_deref())
                .collect::<Vec<_>>(),
            [Some("first cause"), Some("second cause")]
        );
    }

    #[test]
    fn deep_desktop_failure_conversion_preserves_the_complete_tree() {
        const CHILD: &str = "UBM_522_DEEP_CONVERSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("ordered_notification_groups::error_mapping_tests::deep_desktop_failure_conversion_preserves_the_complete_tree")
                .arg("--nocapture").env(CHILD, "1").status().unwrap();
            assert!(status.success(), "deep conversion child failed: {status}");
            return;
        }
        let mut error =
            DesktopError::new(BleErrorCode::PlatformFailure, BleErrorDomain::Gatt, "leaf");
        for _ in 0..20_000 {
            let mut platform = PlatformDetail::new("native", "nested");
            platform.cleanup_failures.push(error);
            error = DesktopError::new(BleErrorCode::PlatformFailure, BleErrorDomain::Gatt, "outer")
                .with_platform(platform);
        }
        let converted = failure_from_desktop(&error);
        // DesktopError's independently owned recursive drop is outside this module.
        std::mem::forget(error);
        let mut node = &converted;
        for _ in 0..20_000 {
            node = &node.platform_cleanup_failures[0];
        }
        assert_eq!(node.operation, "leaf");
    }
}
