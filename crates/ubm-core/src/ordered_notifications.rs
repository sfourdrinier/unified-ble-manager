// crates/ubm-core/src/ordered_notifications.rs
//! Portable ownership and bounded staging for one native ordered collection.
//!
//! The native source owns callback ordering and fence admission. This state
//! machine consumes only records already admitted by that source; it never
//! constructs an ordinal from independently delivered characteristic streams.

use std::collections::{BTreeMap, HashSet, VecDeque};

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

/// A typed native error metadata value retained without string matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureValue {
    Int(i64),
    Text(String),
    Bool(bool),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    Closed,
    SetupFailed(Failure),
    SourceFailed(Failure),
    Invalidated,
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
    OrdinalExhausted { bytes: u64 },
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Record(Record),
    Terminal(Terminal),
    Empty,
    Closed,
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
    controls: usize,
    max_items: usize,
    max_bytes: usize,
    max_controls: usize,
    last_ordinal: Option<u64>,
    terminal: Option<Terminal>,
    terminal_taken: bool,
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
            controls: 0,
            max_items,
            max_bytes,
            max_controls,
            last_ordinal: None,
            terminal: None,
            terminal_taken: false,
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
        });
        Ok(())
    }

    /// Submit only a record already admitted under this group's source lock.
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
            });
            return Ok(());
        }
        self.last_ordinal = Some(ordinal);
        if self.value_items >= self.max_items
            || value.len() > self.max_bytes.saturating_sub(self.value_bytes)
        {
            self.end(Terminal {
                reason: EndReason::CapacityExceeded,
                tail_uncertain: true,
                rejected_values: Some(1),
                rejected_loss: None,
                source_rejection: None,
                source_failure_member: None,
            });
            return Ok(());
        }
        self.value_items += 1;
        self.value_bytes += value.len();
        self.records.push_back(Record::Value {
            ordinal,
            member,
            value: value.to_vec(),
            delivery,
            provenance,
        });
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
        if let LossScope::Member(id) = scope
            && !self.members.iter().any(|member| member.member.id == id)
        {
            return Err(Error::UnknownMember);
        }
        if self.controls == self.max_controls {
            self.end(Terminal {
                reason: EndReason::CapacityExceeded,
                tail_uncertain: true,
                rejected_values: None,
                rejected_loss: Some(RejectedLoss {
                    scope,
                    unit,
                    count,
                    bytes,
                }),
                source_rejection: None,
                source_failure_member: None,
            });
            return Ok(());
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
        if !matches!(self.phase, Phase::Staging | Phase::Active) {
            return Err(Error::InvalidPhase);
        }
        self.end(Terminal {
            reason,
            tail_uncertain,
            rejected_values: None,
            rejected_loss: None,
            source_rejection,
            source_failure_member,
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
            && self
                .members
                .iter()
                .all(|member| member.acquisition == Acquisition::Released && !member.enable_pending)
    }

    pub fn poll(&mut self) -> Poll {
        if matches!(self.phase, Phase::AwaitingSource | Phase::Staging) {
            return Poll::Empty;
        }
        if let Some(record) = self.records.pop_front() {
            match &record {
                Record::Value { value, .. } => {
                    self.value_items -= 1;
                    self.value_bytes -= value.len();
                }
                Record::Loss { .. } => self.controls -= 1,
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
        self.terminal = Some(terminal);
    }
}

#[cfg(test)]
#[path = "ordered_notifications_tests.rs"]
mod tests;
