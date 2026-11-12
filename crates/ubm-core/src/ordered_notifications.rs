// crates/ubm-core/src/ordered_notifications.rs
//! Portable ownership and bounded staging for one native ordered collection.
//!
//! The native source owns callback ordering and fence admission. This state
//! machine consumes only records already admitted by that source; it never
//! constructs an ordinal from independently delivered characteristic streams.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::central::NotificationProvenance;
use crate::contracts::{MAX_STREAM_BYTE_CAPACITY, MAX_STREAM_ITEM_CAPACITY};

/// Tagged native source identity; UUID-like identities are retained verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceGeneration {
    Counter(u64),
    Opaque(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Membership {
    pub member: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Notification,
    Indication,
    Either,
}

/// Delivery observed by the host; an unreported mode stays unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedDelivery {
    Notification,
    Indication,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub id: u64,
    pub source: Membership,
    pub delivery: DeliveryMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossScope {
    Member(u64),
    Group,
    Unknown,
}

/// Source-record gaps are not exact counts of values for any member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossUnit {
    Values,
    SourceRecords,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Value {
        ordinal: u64,
        member: u64,
        /// Source-time native membership, retained across later rebinds.
        membership: Membership,
        value: Vec<u8>,
        delivery: ObservedDelivery,
        provenance: NotificationProvenance,
    },
    /// A source-admitted command boundary, not proof of radio causality.
    Marker { marker_id: u64, ordinal: u64 },
    Loss {
        scope: LossScope,
        unit: LossUnit,
        count: Option<u64>,
        bytes: Option<u64>,
    },
    /// One occurrence; identical reports remain separate records. At least one
    /// field must carry evidence or `tail_uncertain` must be true.
    FailureObservation {
        failure: Option<Failure>,
        source_rejection: Option<SourceRejection>,
        source_failure_member: Option<Membership>,
        rejected_loss: Option<RejectedLoss>,
        tail_uncertain: bool,
    },
    /// A known omission; `None` means a positive but unknown count and a
    /// present count must be nonzero.
    FailureObservationsOmitted { count: Option<u64> },
}

/// A typed native error metadata value retained without string matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureValue {
    Int(i64),
    Text(String),
    Bool(bool),
}

/// An exact platform-supplied entry, including duplicate or colliding names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureMetadataEntry {
    pub key: String,
    pub value: FailureValue,
}

/// An independent platform cleanup detail, separate from the primary cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailurePlatformDetail {
    pub domain: String,
    pub code: String,
    pub message: Option<String>,
    pub metadata: BTreeMap<String, FailureValue>,
    pub metadata_entries: Option<Vec<FailureMetadataEntry>>,
    pub cleanup: Vec<FailurePlatformDetail>,
    pub cleanup_failures: Vec<Failure>,
}

/// Normalized failure identity retained for setup and source terminals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub domain: String,
    pub operation: String,
    pub detail: Option<String>,
    pub platform_domain: Option<String>,
    pub platform_code: Option<String>,
    pub platform_message: Option<String>,
    pub platform_metadata: BTreeMap<String, FailureValue>,
    pub platform_metadata_entries: Option<Vec<FailureMetadataEntry>>,
    pub platform_cleanup: Vec<FailurePlatformDetail>,
    pub platform_cleanup_failures: Vec<Failure>,
}

// Rejected input can be arbitrarily deep, even before it reaches an admission
// budget. Detach children before dropping each node to keep destruction off
// the call stack, including mixed failure/detail trees.
enum FailureNode {
    Failure(Failure),
    Detail(FailurePlatformDetail),
}

fn drop_failure_nodes(mut nodes: Vec<FailureNode>) {
    while let Some(node) = nodes.pop() {
        match node {
            FailureNode::Failure(mut failure) => {
                nodes.extend(
                    std::mem::take(&mut failure.platform_cleanup)
                        .into_iter()
                        .map(FailureNode::Detail),
                );
                nodes.extend(
                    std::mem::take(&mut failure.platform_cleanup_failures)
                        .into_iter()
                        .map(FailureNode::Failure),
                );
            }
            FailureNode::Detail(mut detail) => {
                nodes.extend(
                    std::mem::take(&mut detail.cleanup)
                        .into_iter()
                        .map(FailureNode::Detail),
                );
                nodes.extend(
                    std::mem::take(&mut detail.cleanup_failures)
                        .into_iter()
                        .map(FailureNode::Failure),
                );
            }
        }
    }
}

impl Drop for Failure {
    fn drop(&mut self) {
        let mut nodes = Vec::new();
        nodes.extend(
            std::mem::take(&mut self.platform_cleanup)
                .into_iter()
                .map(FailureNode::Detail),
        );
        nodes.extend(
            std::mem::take(&mut self.platform_cleanup_failures)
                .into_iter()
                .map(FailureNode::Failure),
        );
        drop_failure_nodes(nodes);
    }
}

impl Drop for FailurePlatformDetail {
    fn drop(&mut self) {
        let mut nodes = Vec::new();
        nodes.extend(
            std::mem::take(&mut self.cleanup)
                .into_iter()
                .map(FailureNode::Detail),
        );
        nodes.extend(
            std::mem::take(&mut self.cleanup_failures)
                .into_iter()
                .map(FailureNode::Failure),
        );
        drop_failure_nodes(nodes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    Closed,
    SetupFailed(Failure),
    SourceFailed(Failure),
    Invalidated,
    ServiceChanged,
    ConnectionLost,
    OwnerReleased,
    HostReset(Failure),
    CapacityExceeded,
    OrdinalInvalid,
    SourceOverflow,
    SourceOrdinalExhausted,
}

/// A source loss control rejected because the reserved control lane filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RejectedLoss {
    pub scope: LossScope,
    pub unit: LossUnit,
    pub count: Option<u64>,
    pub bytes: Option<u64>,
}

/// A rejected record belongs to the shared native source, not necessarily
/// to this collection or one of its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceRejection {
    Value { bytes: u64 },
    Loss { skipped_records: u64 },
    Fence { membership: Membership },
    Barrier { id: u64 },
    Marker { marker_id: u64 },
    OrdinalExhausted { bytes: u64 },
}

/// Bounded evidence for failures observed after a source terminal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LateFailureSummary {
    pub first: Option<Failure>,
    pub additional_count: u64,
    /// The counter itself exceeded its representable range.
    pub count_overflowed: bool,
    /// Native overflow omitted an unknown number of observations.
    pub omitted_unknown: bool,
    /// A separate native barrier reported an uncertain tail.
    pub tail_uncertain: bool,
}

impl LateFailureSummary {
    pub fn observe(&mut self, failure: Failure) {
        if self.first.is_none() {
            self.first = Some(failure);
        } else if let Some(count) = self.additional_count.checked_add(1) {
            self.additional_count = count;
        } else {
            self.count_overflowed = true;
        }
    }

    pub fn merge(&mut self, other: Self) {
        if let Some(first) = other.first {
            self.observe(first);
        }
        match self.additional_count.checked_add(other.additional_count) {
            Some(count) => self.additional_count = count,
            None => {
                self.additional_count = u64::MAX;
                self.count_overflowed = true;
            }
        }
        self.count_overflowed |= other.count_overflowed;
        self.omitted_unknown |= other.omitted_unknown;
        self.tail_uncertain |= other.tail_uncertain;
    }
}

/// Separate from the bounded FIFO: an exhausted queue cannot lose its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub reason: EndReason,
    /// The native/platform tail or an unadmitted control cannot be quantified.
    pub tail_uncertain: bool,
    /// A value rejected at this collection's own admission, when known.
    pub rejected_values: Option<u64>,
    pub rejected_loss: Option<RejectedLoss>,
    pub source_rejection: Option<SourceRejection>,
    pub source_failure_member: Option<Membership>,
    /// Failure admitted after the native terminal and before source seal.
    pub secondary_failure: Option<Failure>,
    /// Further separately observed failures; exact unless the counter overflowed.
    pub additional_failure_count: u64,
    /// The observed count is a lower bound when this flag is set.
    pub failure_count_overflowed: bool,
    /// Native overflow omitted an unknown number of failure observations.
    pub failure_observations_omitted: bool,
}

impl Terminal {
    /// Whether the shared delivery layer can separate source evidence from
    /// this terminal without merging unrelated rejected-loss facts.
    #[must_use]
    pub fn has_separable_observation(&self) -> bool {
        (self.source_rejection.is_some()
            || self.source_failure_member.is_some()
            || self.rejected_loss.is_some()
            || self.rejected_values.is_some())
            && !(self.rejected_values.is_some() && self.rejected_loss.is_some())
    }

    pub fn merge_late_failures(&mut self, summary: LateFailureSummary) {
        let mut retained = LateFailureSummary {
            first: self.secondary_failure.take(),
            additional_count: self.additional_failure_count,
            count_overflowed: self.failure_count_overflowed,
            omitted_unknown: self.failure_observations_omitted,
            tail_uncertain: self.tail_uncertain,
        };
        retained.merge(summary);
        self.tail_uncertain = retained.tail_uncertain;
        self.secondary_failure = retained.first;
        self.additional_failure_count = retained.additional_count;
        self.failure_count_overflowed = retained.count_overflowed;
        self.failure_observations_omitted = retained.omitted_unknown;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    AwaitingSource,
    Staging,
    Active,
    Closing,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquisition {
    Pending,
    Enabling,
    Acquired,
    Indeterminate,
    Releasing,
    Released,
    ReleaseFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberState {
    pub member: Member,
    pub acquisition: Acquisition,
    pub cleanup_error: Option<String>,
    /// An outstanding enable may still report a late success after cancellation.
    pub enable_pending: bool,
    /// A late enable may have taken effect after the current release dispatch.
    pub enabled_during_release: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    InvalidPhase,
    UnknownMember,
    WrongSource,
    DuplicateResult,
    MarkerCreditUnavailable,
    PendingCapacity,
    RecordTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Record(Record),
    Terminal(Terminal),
    Empty,
    Closed,
}

/// An occurrence retained by the source in its original FIFO position.
#[derive(Debug)]
pub enum RetainedRecord {
    Value {
        membership: Membership,
        ordinal: u64,
        value: Vec<u8>,
        delivery: ObservedDelivery,
        provenance: NotificationProvenance,
    },
    Loss {
        scope: LossScope,
        unit: LossUnit,
        count: Option<u64>,
        bytes: Option<u64>,
    },
}

/// An unforgeable, single-use claim on one collection instance. The producer
/// retains this object until it is admitted or explicitly recovered by cancel.
#[derive(Debug)]
#[must_use = "the source must retry or explicitly recover its retained occurrence"]
pub struct OccurrenceLease {
    owner: Arc<()>,
    id: u64,
    record: Option<RetainedRecord>,
}

/// One group's lifetime. Methods are driven by one host owner, which must
/// serialize source admission with member install and retirement fences.
#[derive(Debug)]
pub struct OrderedCollection {
    pub peer_generation: String,
    pub database_generation: String,
    pub source_generation: SourceGeneration,
    pub group_id: u64,
    phase: Phase,
    members: Vec<MemberState>,
    records: VecDeque<Record>,
    value_items: usize,
    value_bytes: usize,
    observation_bytes: usize,
    controls: usize,
    reserved_marker: Option<u64>,
    max_items: usize,
    max_bytes: usize,
    max_controls: usize,
    last_ordinal: Option<u64>,
    terminal: Option<Terminal>,
    terminal_taken: bool,
    // For a terminal with separable evidence, count the queued records that
    // precede it. Later failures cannot overtake its split observation.
    records_before_terminal: Option<usize>,
    occurrence_owner: Arc<()>,
    next_occurrence: u64,
    retained_occurrences: VecDeque<u64>,
    preclose_records: Option<usize>,
}

impl OrderedCollection {
    pub fn new(
        peer_generation: String,
        database_generation: String,
        source_generation: SourceGeneration,
        group_id: u64,
        members: Vec<Member>,
        max_items: usize,
        max_bytes: usize,
        max_controls: usize,
    ) -> Result<Self, Error> {
        let mut ids = HashSet::new();
        let mut source_members = HashSet::new();
        if peer_generation.is_empty()
            || database_generation.is_empty()
            || matches!(&source_generation, SourceGeneration::Opaque(value) if value.is_empty())
            || members.is_empty()
            || u64::try_from(members.len()).map_or(true, |value| value > MAX_STREAM_ITEM_CAPACITY)
            || max_items == 0
            || max_bytes == 0
            || max_controls == 0
            || u64::try_from(max_items).map_or(true, |value| value > MAX_STREAM_ITEM_CAPACITY)
            || u64::try_from(max_controls).map_or(true, |value| value > MAX_STREAM_ITEM_CAPACITY)
            || u64::try_from(max_bytes).map_or(true, |value| value > MAX_STREAM_BYTE_CAPACITY)
            || members
                .iter()
                .any(|member| !ids.insert(member.id) || !source_members.insert(member.source))
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            peer_generation,
            database_generation,
            source_generation,
            group_id,
            phase: Phase::AwaitingSource,
            members: members
                .into_iter()
                .map(|member| MemberState {
                    member,
                    acquisition: Acquisition::Pending,
                    cleanup_error: None,
                    enable_pending: false,
                    enabled_during_release: false,
                })
                .collect(),
            records: VecDeque::new(),
            value_items: 0,
            value_bytes: 0,
            observation_bytes: 0,
            controls: 0,
            reserved_marker: None,
            max_items,
            max_bytes,
            max_controls,
            last_ordinal: None,
            terminal: None,
            terminal_taken: false,
            records_before_terminal: None,
            occurrence_owner: Arc::new(()),
            next_occurrence: 0,
            retained_occurrences: VecDeque::new(),
            preclose_records: None,
        })
    }

    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    #[must_use]
    pub fn members(&self) -> &[MemberState] {
        &self.members
    }

    /// Capture an exact source occurrence while admission is still open. The
    /// caller serializes this with the source FIFO and close fence. On refusal
    /// the original record is returned to its owner unchanged.
    pub fn retain_occurrence(
        &mut self,
        source: &SourceGeneration,
        group_id: u64,
        record: RetainedRecord,
    ) -> Result<OccurrenceLease, (Error, RetainedRecord)> {
        let validate = (|| {
            self.require_source(source, group_id)?;
            match &record {
                RetainedRecord::Value {
                    membership, value, ..
                } => {
                    if !self
                        .members
                        .iter()
                        .any(|member| member.member.source == *membership)
                    {
                        return Err(Error::UnknownMember);
                    }
                    if u64::try_from(value.len())
                        .map_or(true, |size| size > MAX_STREAM_BYTE_CAPACITY)
                    {
                        return Err(Error::RecordTooLarge);
                    }
                }
                RetainedRecord::Loss {
                    scope: LossScope::Member(id),
                    ..
                } if !self.members.iter().any(|member| member.member.id == *id) => {
                    return Err(Error::UnknownMember);
                }
                RetainedRecord::Loss { .. } => {}
            }
            if self.retained_occurrences.len() >= self.max_items.saturating_add(self.max_controls) {
                return Err(Error::PendingCapacity);
            }
            self.next_occurrence
                .checked_add(1)
                .ok_or(Error::InvalidConfiguration)
        })();
        let next = match validate {
            Ok(next) => next,
            Err(error) => return Err((error, record)),
        };
        self.next_occurrence = next;
        self.retained_occurrences.push_back(next);
        Ok(OccurrenceLease {
            owner: self.occurrence_owner.clone(),
            id: next,
            record: Some(record),
        })
    }

    /// Retry only the oldest retained claim; a capacity refusal changes nothing.
    pub fn retry_occurrence(
        &mut self,
        source: &SourceGeneration,
        group_id: u64,
        lease: &mut OccurrenceLease,
    ) -> Result<(), Error> {
        self.check_occurrence(source, group_id, lease)?;
        if self.retained_occurrences.front() != Some(&lease.id) {
            return Err(Error::PendingCapacity);
        }
        let record = lease.record.as_ref().ok_or(Error::InvalidPhase)?;
        match record {
            RetainedRecord::Value { ordinal, value, .. } => {
                if self.last_ordinal.is_some_and(|last| *ordinal <= last) {
                    return Err(Error::InvalidConfiguration);
                }
                if self.value_items >= self.max_items
                    || value.len()
                        > self
                            .max_bytes
                            .saturating_sub(self.value_bytes.saturating_add(self.observation_bytes))
                {
                    return Err(Error::PendingCapacity);
                }
            }
            RetainedRecord::Loss { .. } => {
                if self.controls + usize::from(self.reserved_marker.is_some()) >= self.max_controls
                {
                    return Err(Error::PendingCapacity);
                }
            }
        }
        let record = match lease.record.take().ok_or(Error::InvalidPhase)? {
            RetainedRecord::Value {
                membership,
                ordinal,
                value,
                delivery,
                provenance,
            } => {
                let member = self
                    .members
                    .iter()
                    .find(|member| member.member.source == membership)
                    .ok_or(Error::UnknownMember)?
                    .member
                    .id;
                self.last_ordinal = Some(ordinal);
                self.value_items += 1;
                self.value_bytes += value.len();
                Record::Value {
                    ordinal,
                    member,
                    membership,
                    value,
                    delivery,
                    provenance,
                }
            }
            RetainedRecord::Loss {
                scope,
                unit,
                count,
                bytes,
            } => {
                self.controls += 1;
                Record::Loss {
                    scope,
                    unit,
                    count,
                    bytes,
                }
            }
        };
        self.retained_occurrences.pop_front();
        if let Some(index) = &mut self.preclose_records {
            self.records.insert(*index, record);
            *index += 1;
            if let Some(count) = &mut self.records_before_terminal {
                *count += 1;
            }
        } else {
            self.records.push_back(record);
        }
        Ok(())
    }

    /// Recover a claim without admitting or discarding its occurrence. This
    /// is not a source acknowledgement: the caller remains responsible for
    /// the returned record and its ordering with any later source records.
    pub fn cancel_occurrence(
        &mut self,
        source: &SourceGeneration,
        group_id: u64,
        lease: &mut OccurrenceLease,
    ) -> Result<RetainedRecord, Error> {
        self.check_occurrence(source, group_id, lease)?;
        let position = self
            .retained_occurrences
            .iter()
            .position(|id| *id == lease.id)
            .ok_or(Error::InvalidPhase)?;
        self.retained_occurrences.remove(position);
        lease.record.take().ok_or(Error::InvalidPhase)
    }

    fn check_occurrence(
        &self,
        source: &SourceGeneration,
        group_id: u64,
        lease: &OccurrenceLease,
    ) -> Result<(), Error> {
        if &self.source_generation != source
            || self.group_id != group_id
            || !Arc::ptr_eq(&self.occurrence_owner, &lease.owner)
        {
            return Err(Error::WrongSource);
        }
        if lease.record.is_none()
            || !matches!(self.phase, Phase::Staging | Phase::Active | Phase::Closing)
        {
            return Err(Error::InvalidPhase);
        }
        Ok(())
    }

    /// Increase this source's existing allocation budget without changing its
    /// queue, terminal, or cleanup state. Repeating the same limit is safe.
    pub fn increase_byte_capacity(
        &mut self,
        generation: &SourceGeneration,
        group_id: u64,
        max_bytes: usize,
    ) -> Result<usize, Error> {
        if generation != &self.source_generation || group_id != self.group_id {
            return Err(Error::WrongSource);
        }
        if max_bytes < self.max_bytes
            || u64::try_from(max_bytes).map_or(true, |value| value > MAX_STREAM_BYTE_CAPACITY)
        {
            return Err(Error::InvalidConfiguration);
        }
        let previous = self.max_bytes;
        self.max_bytes = max_bytes;
        Ok(previous)
    }

    /// Check failure evidence before cloning or reserving its terminal slot.
    pub fn validate_failure_evidence(&self, failure: &Failure) -> Result<(), Error> {
        if failure_allocation_cost(failure) > self.max_bytes {
            Err(Error::RecordTooLarge)
        } else {
            Ok(())
        }
    }

    /// Preflight a retained native value without consuming its ordinal.
    pub fn validate_individual_value(
        &self,
        generation: &SourceGeneration,
        group_id: u64,
        bytes: usize,
    ) -> Result<(), Error> {
        self.require_source(generation, group_id)?;
        if bytes > self.max_bytes {
            Err(Error::RecordTooLarge)
        } else {
            Ok(())
        }
    }

    /// Rebind a logical member only after the source owner has consumed the
    /// old membership's FIFO fence and serialized the new registration.
    pub fn rebind_member(
        &mut self,
        id: u64,
        old: Membership,
        new: Membership,
    ) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Staging | Phase::Active)
            || !self.retained_occurrences.is_empty()
        {
            return Err(Error::InvalidPhase);
        }
        if old == new
            || self
                .members
                .iter()
                .any(|member| member.member.source == new)
        {
            return Err(Error::InvalidConfiguration);
        }
        let member = self
            .members
            .iter_mut()
            .find(|member| member.member.id == id)
            .ok_or(Error::UnknownMember)?;
        if member.member.source != old {
            return Err(Error::UnknownMember);
        }
        member.member.source = new;
        Ok(())
    }

    /// Must succeed before requesting the first physical member enable.
    pub fn source_installed(&mut self) -> Result<(), Error> {
        if self.phase != Phase::AwaitingSource {
            return Err(Error::InvalidPhase);
        }
        self.phase = Phase::Staging;
        Ok(())
    }

    pub fn begin_enable(&mut self, id: u64) -> Result<(), Error> {
        if self.phase != Phase::Staging {
            return Err(Error::InvalidPhase);
        }
        if self
            .members
            .iter()
            .any(|member| member.acquisition == Acquisition::Enabling)
        {
            return Err(Error::InvalidPhase);
        }
        let member = self.member_mut(id)?;
        if member.acquisition != Acquisition::Pending {
            return Err(Error::InvalidPhase);
        }
        member.acquisition = Acquisition::Enabling;
        member.enable_pending = true;
        Ok(())
    }

    /// `None` means the platform result is indeterminate and remains owned.
    /// Late success after setup cancellation also remains a cleanup obligation.
    pub fn enable_result(&mut self, id: u64, success: Option<bool>) -> Result<(), Error> {
        let member = self.member_mut(id)?;
        if !member.enable_pending {
            return Err(Error::DuplicateResult);
        }
        if let Some(succeeded) = success {
            member.enable_pending = false;
            match (member.acquisition, succeeded) {
                (Acquisition::Enabling | Acquisition::Indeterminate, true) => {
                    member.acquisition = Acquisition::Acquired
                }
                (Acquisition::Enabling | Acquisition::Indeterminate, false) => {
                    member.acquisition = Acquisition::Released
                }
                (Acquisition::Released, true) => member.acquisition = Acquisition::Acquired,
                (Acquisition::Releasing, true) => member.enabled_during_release = true,
                (
                    Acquisition::Releasing | Acquisition::ReleaseFailed | Acquisition::Released,
                    _,
                ) => {}
                _ => return Err(Error::InvalidPhase),
            }
        } else if member.acquisition == Acquisition::Enabling {
            member.acquisition = Acquisition::Indeterminate;
        }
        if self.phase == Phase::Staging
            && self
                .members
                .iter()
                .all(|member| member.acquisition == Acquisition::Acquired && !member.enable_pending)
        {
            self.phase = Phase::Active;
        }
        Ok(())
    }

    /// Record the failed setup result without discarding admitted observations.
    pub fn fail_enable(
        &mut self,
        id: u64,
        outcome: Option<bool>,
        failure: Failure,
        tail_uncertain: bool,
    ) -> Result<(), Error> {
        if self.phase != Phase::Staging {
            return Err(Error::InvalidPhase);
        }
        self.enable_result(id, outcome)?;
        self.end(Terminal {
            reason: EndReason::SetupFailed(failure),
            tail_uncertain,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: None,
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        });
        Ok(())
    }

    /// Submit a source-owned record. Pending capacity leaves the ordinal and
    /// collection untouched; the source must retain and retry the occurrence.
    pub fn admit_value(
        &mut self,
        source_generation: &SourceGeneration,
        group_id: u64,
        membership: Membership,
        ordinal: u64,
        value: &[u8],
        delivery: ObservedDelivery,
        provenance: NotificationProvenance,
    ) -> Result<(), Error> {
        self.require_source(source_generation, group_id)?;
        if !self.retained_occurrences.is_empty() {
            return Err(Error::PendingCapacity);
        }
        let member = self
            .members
            .iter()
            .find(|known| known.member.source == membership)
            .ok_or(Error::UnknownMember)?
            .member
            .id;
        if self.last_ordinal.is_some_and(|last| ordinal <= last) {
            self.end(Terminal {
                reason: EndReason::OrdinalInvalid,
                tail_uncertain: true,
                rejected_values: Some(1),
                rejected_loss: None,
                source_rejection: None,
                source_failure_member: None,
                secondary_failure: None,
                additional_failure_count: 0,
                failure_count_overflowed: false,
                failure_observations_omitted: false,
            });
            return Ok(());
        }
        if self.value_items >= self.max_items
            || value.len()
                > self
                    .max_bytes
                    .saturating_sub(self.value_bytes.saturating_add(self.observation_bytes))
        {
            return Err(Error::PendingCapacity);
        }
        self.last_ordinal = Some(ordinal);
        self.value_items += 1;
        self.value_bytes += value.len();
        self.records.push_back(Record::Value {
            ordinal,
            member,
            membership,
            value: value.to_vec(),
            delivery,
            provenance,
        });
        Ok(())
    }

    /// Reserve one exact downstream control slot before native marker dispatch.
    /// This is capacity only; the native FIFO still assigns the ordinal.
    pub fn reserve_marker(
        &mut self,
        source_generation: &SourceGeneration,
        group_id: u64,
        marker_id: u64,
    ) -> Result<(), Error> {
        self.require_source(source_generation, group_id)?;
        if self.reserved_marker == Some(marker_id) {
            return Ok(());
        }
        if !self.retained_occurrences.is_empty() {
            return Err(Error::PendingCapacity);
        }
        if self.reserved_marker.is_some() || self.controls >= self.max_controls {
            return Err(Error::MarkerCreditUnavailable);
        }
        self.reserved_marker = Some(marker_id);
        Ok(())
    }

    #[must_use]
    pub fn marker_reserved(&self, marker_id: u64) -> bool {
        self.reserved_marker == Some(marker_id)
    }

    /// Release only after authoritative native NotAdmitted or pre-dispatch cancel.
    pub fn release_marker(&mut self, marker_id: u64) -> Result<(), Error> {
        if self.reserved_marker != Some(marker_id) {
            return Err(Error::InvalidConfiguration);
        }
        self.reserved_marker = None;
        Ok(())
    }

    /// Admit a marker assigned by the same source ordinal owner as values.
    pub fn admit_marker(
        &mut self,
        source_generation: &SourceGeneration,
        group_id: u64,
        marker_id: u64,
        ordinal: u64,
    ) -> Result<(), Error> {
        self.require_source(source_generation, group_id)?;
        if !self.retained_occurrences.is_empty() {
            return Err(Error::PendingCapacity);
        }
        if self.reserved_marker != Some(marker_id) {
            return Err(Error::MarkerCreditUnavailable);
        }
        self.reserved_marker = None;
        if self.last_ordinal.is_some_and(|last| ordinal <= last) {
            self.end(Terminal {
                reason: EndReason::OrdinalInvalid,
                tail_uncertain: true,
                rejected_values: None,
                rejected_loss: None,
                source_rejection: Some(SourceRejection::Marker { marker_id }),
                source_failure_member: None,
                secondary_failure: None,
                additional_failure_count: 0,
                failure_count_overflowed: false,
                failure_observations_omitted: false,
            });
            return Ok(());
        }
        self.last_ordinal = Some(ordinal);
        if self.controls == self.max_controls {
            self.end(Terminal {
                reason: EndReason::CapacityExceeded,
                tail_uncertain: true,
                rejected_values: None,
                rejected_loss: None,
                source_rejection: Some(SourceRejection::Marker { marker_id }),
                source_failure_member: None,
                secondary_failure: None,
                additional_failure_count: 0,
                failure_count_overflowed: false,
                failure_observations_omitted: false,
            });
            return Ok(());
        }
        self.controls += 1;
        self.records
            .push_back(Record::Marker { marker_id, ordinal });
        Ok(())
    }

    pub fn admit_loss(
        &mut self,
        source_generation: &SourceGeneration,
        group_id: u64,
        scope: LossScope,
        unit: LossUnit,
        count: Option<u64>,
        bytes: Option<u64>,
    ) -> Result<(), Error> {
        self.require_source(source_generation, group_id)?;
        if !self.retained_occurrences.is_empty() {
            return Err(Error::PendingCapacity);
        }
        if let LossScope::Member(id) = scope
            && !self.members.iter().any(|member| member.member.id == id)
        {
            return Err(Error::UnknownMember);
        }
        if self.controls + usize::from(self.reserved_marker.is_some()) >= self.max_controls {
            return Err(Error::PendingCapacity);
        }
        self.controls += 1;
        self.records.push_back(Record::Loss {
            scope,
            unit,
            count,
            bytes,
        });
        Ok(())
    }

    /// Admit exact failure evidence in FIFO order, including after a source
    /// terminal has been taken for deferred delivery. The caller seals the
    /// source before publishing that terminal to its consumer.
    pub fn admit_failure_record(
        &mut self,
        source: &SourceGeneration,
        group_id: u64,
        record: Record,
    ) -> Result<(), Error> {
        if &self.source_generation != source || self.group_id != group_id {
            return Err(Error::WrongSource);
        }
        if self.phase == Phase::AwaitingSource {
            return Err(Error::InvalidPhase);
        }
        if matches!(self.phase, Phase::Staging | Phase::Active)
            && !self.retained_occurrences.is_empty()
        {
            return Err(Error::PendingCapacity);
        }
        let marker = match &record {
            Record::FailureObservation {
                failure,
                source_rejection,
                source_failure_member,
                rejected_loss,
                tail_uncertain,
            } => {
                if failure.is_none()
                    && source_rejection.is_none()
                    && source_failure_member.is_none()
                    && rejected_loss.is_none()
                    && !tail_uncertain
                {
                    return Err(Error::InvalidConfiguration);
                }
                match source_rejection {
                    Some(SourceRejection::Marker { marker_id })
                        if self.reserved_marker == Some(*marker_id) =>
                    {
                        Some(*marker_id)
                    }
                    _ => None,
                }
            }
            Record::FailureObservationsOmitted { count: Some(0) } => {
                return Err(Error::InvalidConfiguration);
            }
            Record::FailureObservationsOmitted { .. } => None,
            _ => return Err(Error::InvalidConfiguration),
        };
        let bytes = observation_size(&record);
        if bytes > self.max_bytes {
            return Err(Error::RecordTooLarge);
        }
        let reserved = usize::from(self.reserved_marker.is_some() && marker.is_none());
        if self.controls.saturating_add(reserved) >= self.max_controls
            || bytes
                > self
                    .max_bytes
                    .saturating_sub(self.value_bytes.saturating_add(self.observation_bytes))
        {
            return Err(Error::PendingCapacity);
        }
        if marker.is_some() {
            self.reserved_marker = None;
        }
        self.controls += 1;
        self.observation_bytes += bytes;
        // Clone variable data to avoid retaining excess caller allocation capacity.
        self.records.push_back(record.clone());
        Ok(())
    }

    /// Caller fences source admission before closing. Pending enables become
    /// indeterminate until a late platform result or a definitive owner fence.
    pub fn close(&mut self, reason: EndReason, tail_uncertain: bool) -> Result<(), Error> {
        if matches!(self.phase, Phase::Closing | Phase::Terminal) {
            return Err(Error::InvalidPhase);
        }
        self.end(Terminal {
            reason,
            tail_uncertain,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: None,
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        });
        Ok(())
    }

    /// Preserve a native source terminal and its source-wide rejected record.
    pub fn source_terminal(
        &mut self,
        reason: EndReason,
        source_rejection: Option<SourceRejection>,
        source_failure_member: Option<Membership>,
        tail_uncertain: bool,
    ) -> Result<(), Error> {
        self.source_terminal_with_rejected_loss(
            reason,
            source_rejection,
            source_failure_member,
            None,
            tail_uncertain,
        )
    }

    /// Preserve a rejected loss control, including unknown count or bytes.
    /// When both `source_rejection` and `rejected_loss` describe the same
    /// source record they are complementary evidence, not two loss events.
    pub fn source_terminal_with_rejected_loss(
        &mut self,
        reason: EndReason,
        source_rejection: Option<SourceRejection>,
        source_failure_member: Option<Membership>,
        rejected_loss: Option<RejectedLoss>,
        tail_uncertain: bool,
    ) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Staging | Phase::Active) {
            return Err(Error::InvalidPhase);
        }
        if let EndReason::SetupFailed(failure)
        | EndReason::SourceFailed(failure)
        | EndReason::HostReset(failure) = &reason
        {
            self.validate_failure_evidence(failure)?;
        }
        if let Some(RejectedLoss {
            scope: LossScope::Member(id),
            ..
        }) = rejected_loss
            && !self.members.iter().any(|member| member.member.id == id)
        {
            return Err(Error::UnknownMember);
        }
        if let Some(SourceRejection::Marker { marker_id }) = source_rejection {
            if self.reserved_marker != Some(marker_id) {
                return Err(Error::MarkerCreditUnavailable);
            }
            self.reserved_marker = None;
        }
        let failure_observations_omitted = false;
        self.end(Terminal {
            reason,
            tail_uncertain,
            rejected_values: None,
            rejected_loss,
            source_rejection,
            source_failure_member,
            secondary_failure: None,
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted,
        });
        Ok(())
    }

    /// List exact member claims still requiring an asynchronous cleanup.
    #[must_use]
    pub fn cleanup_candidates(&self) -> Vec<Member> {
        self.members
            .iter()
            .filter(|member| {
                matches!(
                    member.acquisition,
                    Acquisition::Acquired | Acquisition::Indeterminate | Acquisition::ReleaseFailed
                )
            })
            .map(|member| member.member)
            .collect()
    }

    pub fn begin_release(&mut self, id: u64) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Closing | Phase::Terminal) {
            return Err(Error::InvalidPhase);
        }
        let member = self.member_mut(id)?;
        if !matches!(
            member.acquisition,
            Acquisition::Acquired | Acquisition::Indeterminate | Acquisition::ReleaseFailed
        ) {
            return Err(Error::InvalidPhase);
        }
        member.acquisition = Acquisition::Releasing;
        member.enabled_during_release = false;
        Ok(())
    }

    pub fn release_result(&mut self, id: u64, result: Result<(), String>) -> Result<(), Error> {
        let member = self.member_mut(id)?;
        if member.acquisition != Acquisition::Releasing {
            return Err(Error::DuplicateResult);
        }
        match result {
            Ok(()) => {
                member.acquisition = if member.enabled_during_release {
                    Acquisition::Acquired
                } else {
                    Acquisition::Released
                };
                member.enabled_during_release = false;
                member.cleanup_error = None;
            }
            Err(error) => {
                member.acquisition = Acquisition::ReleaseFailed;
                member.cleanup_error = Some(error);
            }
        }
        Ok(())
    }

    /// Close completion is authoritative only after every member's claim is released.
    #[must_use]
    pub fn cleanup_complete(&self) -> bool {
        matches!(self.phase, Phase::Closing | Phase::Terminal)
            && self.reserved_marker.is_none()
            && self.retained_occurrences.is_empty()
            && self
                .members
                .iter()
                .all(|member| member.acquisition == Acquisition::Released && !member.enable_pending)
    }

    /// Whether source evidence still belongs to this collection.
    #[must_use]
    pub fn has_pending_records(&self) -> bool {
        !self.records.is_empty() || !self.retained_occurrences.is_empty()
    }

    /// Poll at the source boundary. A terminal with separable evidence keeps
    /// its position before later failures; a host must defer its publication
    /// until the source is sealed and those records are drained.
    pub fn poll(&mut self) -> Poll {
        if matches!(self.phase, Phase::AwaitingSource | Phase::Staging) {
            return Poll::Empty;
        }
        if self.preclose_records == Some(0) && !self.retained_occurrences.is_empty() {
            return Poll::Empty;
        }
        if !self.terminal_taken
            && self.records_before_terminal == Some(0)
            && let Some(terminal) = self.terminal.clone()
        {
            self.terminal_taken = true;
            self.phase = Phase::Terminal;
            return Poll::Terminal(terminal);
        }
        if let Some(record) = self.records.pop_front() {
            if let Some(count) = &mut self.preclose_records {
                *count = count.saturating_sub(1);
            }
            if !self.terminal_taken
                && let Some(count) = &mut self.records_before_terminal
            {
                *count -= 1;
            }
            match &record {
                Record::Value { value, .. } => {
                    self.value_items -= 1;
                    self.value_bytes -= value.len();
                }
                Record::Loss { .. } | Record::Marker { .. } => self.controls -= 1,
                Record::FailureObservation { .. } | Record::FailureObservationsOmitted { .. } => {
                    self.controls -= 1;
                    self.observation_bytes -= observation_size(&record);
                }
            }
            return Poll::Record(record);
        }
        if !self.terminal_taken
            && let Some(terminal) = self.terminal.clone()
        {
            self.terminal_taken = true;
            self.phase = Phase::Terminal;
            return Poll::Terminal(terminal);
        }
        if self.terminal_taken {
            Poll::Closed
        } else {
            Poll::Empty
        }
    }

    fn require_source(
        &self,
        source_generation: &SourceGeneration,
        group_id: u64,
    ) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Staging | Phase::Active) {
            return Err(Error::InvalidPhase);
        }
        if &self.source_generation != source_generation || self.group_id != group_id {
            return Err(Error::WrongSource);
        }
        Ok(())
    }

    fn member_mut(&mut self, id: u64) -> Result<&mut MemberState, Error> {
        self.members
            .iter_mut()
            .find(|member| member.member.id == id)
            .ok_or(Error::UnknownMember)
    }

    fn end(&mut self, terminal: Terminal) {
        if self.terminal.is_some() {
            return;
        }
        for member in &mut self.members {
            match member.acquisition {
                Acquisition::Pending => member.acquisition = Acquisition::Released,
                Acquisition::Enabling => member.acquisition = Acquisition::Indeterminate,
                _ => {}
            }
        }
        self.phase = Phase::Closing;
        self.preclose_records = Some(self.records.len());
        self.records_before_terminal = terminal
            .has_separable_observation()
            .then_some(self.records.len());
        self.terminal = Some(terminal);
    }
}

// Fixed record storage is bounded by the item/control count. The byte budget
// covers value bytes and the variable allocations owned by failure evidence.
fn observation_size(record: &Record) -> usize {
    let Record::FailureObservation {
        failure: Some(failure),
        ..
    } = record
    else {
        return 0;
    };
    failure_allocation_cost(failure)
}

/// Saturating budget charge for the variable allocations of cloned failure
/// evidence. Vector elements include their inline storage; map entries receive
/// a conservative node allowance as well as key/text bytes. This is a budget
/// accounting measure, not an exact measurement of allocator RSS. The root
/// failure itself is inline in a separately item-bounded record or terminal.
/// Evidence deeper than 128 nodes returns `usize::MAX`: its exact charge was
/// not calculated and it is too structurally deep for admission and cloning.
#[must_use]
pub fn failure_allocation_cost(failure: &Failure) -> usize {
    failure_allocation_cost_at_depth(failure, 0)
}

const MAX_FAILURE_DEPTH: usize = 128;

fn failure_allocation_cost_at_depth(failure: &Failure, depth: usize) -> usize {
    if depth >= MAX_FAILURE_DEPTH {
        return usize::MAX;
    }
    let mut bytes = failure
        .code
        .len()
        .saturating_add(failure.domain.len())
        .saturating_add(failure.operation.len());
    for value in [
        &failure.detail,
        &failure.platform_domain,
        &failure.platform_code,
        &failure.platform_message,
    ]
    .into_iter()
    .flatten()
    {
        bytes = bytes.saturating_add(value.len());
    }
    bytes = bytes.saturating_add(metadata_allocation_cost(&failure.platform_metadata));
    bytes = bytes.saturating_add(entries_allocation_cost(&failure.platform_metadata_entries));
    for detail in &failure.platform_cleanup {
        bytes = bytes
            .saturating_add(std::mem::size_of::<FailurePlatformDetail>())
            .saturating_add(platform_detail_allocation_cost(detail, depth + 1));
    }
    for cause in &failure.platform_cleanup_failures {
        bytes = bytes
            .saturating_add(std::mem::size_of::<Failure>())
            .saturating_add(failure_allocation_cost_at_depth(cause, depth + 1));
    }
    bytes
}

fn metadata_allocation_cost(metadata: &BTreeMap<String, FailureValue>) -> usize {
    // Charge each entry for a full B-tree node, including eleven key/value
    // slots, twelve child pointers and node bookkeeping. This deliberately
    // overcounts shared nodes and bounds even sparsely occupied maps.
    let node = std::mem::size_of::<(String, FailureValue)>()
        .saturating_mul(11)
        .saturating_add(std::mem::size_of::<usize>().saturating_mul(12))
        .saturating_add(64);
    metadata.iter().fold(0usize, |bytes, (key, value)| {
        bytes
            .saturating_add(node)
            .saturating_add(key.len())
            .saturating_add(value_text_len(value))
    })
}

fn entries_allocation_cost(entries: &Option<Vec<FailureMetadataEntry>>) -> usize {
    entries.as_ref().map_or(0, |entries| {
        entries.iter().fold(0usize, |bytes, entry| {
            bytes
                .saturating_add(std::mem::size_of::<FailureMetadataEntry>())
                .saturating_add(entry.key.len())
                .saturating_add(value_text_len(&entry.value))
        })
    })
}

fn value_text_len(value: &FailureValue) -> usize {
    match value {
        FailureValue::Text(text) => text.len(),
        _ => 0,
    }
}

fn platform_detail_allocation_cost(detail: &FailurePlatformDetail, depth: usize) -> usize {
    if depth >= MAX_FAILURE_DEPTH {
        return usize::MAX;
    }
    let mut bytes = detail
        .domain
        .len()
        .saturating_add(detail.code.len())
        .saturating_add(detail.message.as_ref().map_or(0, String::len))
        .saturating_add(metadata_allocation_cost(&detail.metadata))
        .saturating_add(entries_allocation_cost(&detail.metadata_entries));
    for child in &detail.cleanup {
        bytes = bytes
            .saturating_add(std::mem::size_of::<FailurePlatformDetail>())
            .saturating_add(platform_detail_allocation_cost(child, depth + 1));
    }
    for child in &detail.cleanup_failures {
        bytes = bytes
            .saturating_add(std::mem::size_of::<Failure>())
            .saturating_add(failure_allocation_cost_at_depth(child, depth + 1));
    }
    bytes
}

#[cfg(test)]
#[path = "ordered_notifications_tests.rs"]
mod tests;
