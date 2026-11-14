// crates/ubm-desktop/src/ordered_recording.rs
//! Serialized, retained-delivery to continuation-journal recording foundation.

use crate::DesktopError;
use crate::boundary::{DeliveryMode, RadioBoundary};
use crate::central::{
    DesktopCentral, OrderedClaimRequest, OrderedCleanupOutcome, OrderedDelivery,
    OrderedMemberAcquisition,
};
use crate::continuation_journal::{
    ContinuationJournal, JournalError, JournalQuota, JournalRegistry,
};
use crate::op_control::OpControl;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use ubm_core::ordered_notifications::{
    EndReason, Failure, FailureValue, LossScope, LossUnit, Membership, ObservedDelivery, Poll,
    Record, RejectedLoss, SourceGeneration, SourceRejection, Terminal,
};

/// Stable identity for one live producer. The host must generate a fresh,
/// collision-resistant incarnation for each new producer and retain it for retries.
#[derive(Clone)]
pub struct RecordingIdentity {
    pub recording_id: String,
    pub producer_incarnation: String,
    pub peer_id: String,
    pub collection_id: u64,
    pub source_generation: SourceGeneration,
}

#[derive(Debug, Clone)]
pub enum RecordingError {
    Source(DesktopError),
    Storage(JournalError),
    StorageCleanup(JournalError, DesktopError),
    InvalidDelivery,
    InvalidIdentity,
    WorkerFailed,
    MissingTerminal,
    EntropyUnavailable,
    ReceiptRetired,
    RecordingNotAccepting,
}

impl From<JournalError> for RecordingError {
    fn from(error: JournalError) -> Self {
        Self::Storage(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStep {
    Committed,
    TerminalCommitted,
    PendingSource,
    Empty,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingClose {
    Progress,
    PendingSource,
    PendingDrain,
    Finished,
}

#[derive(Debug, Clone)]
pub enum RecordingDriverStatus {
    Idle,
    Running,
    WaitingCapacity,
    WaitingSource,
    WaitingDrain,
    Closing,
    Failed(RecordingError),
    Finished,
}

/// Immutable public-member mapping for one ordered recording.
#[derive(Debug, Clone)]
pub struct OrderedRecordingMember {
    pub member_id: u64,
    pub public_member_id: String,
    pub selector: ubm_core::central::PathSelector,
    pub delivery: Option<DeliveryMode>,
    pub preference: Option<DeliveryMode>,
}
#[derive(Debug, Clone)]
pub struct OrderedRecordingDeclaration {
    pub peer_id: String,
    pub connection_generation: String,
    pub database_generation: String,
    pub members: Vec<OrderedRecordingMember>,
    pub max_items: usize,
    pub max_bytes: usize,
    pub max_controls: usize,
}
#[expect(
    clippy::result_large_err,
    reason = "The persisted recording error retains source and storage cleanup evidence; boxing would change these established validation results."
)]
impl OrderedRecordingDeclaration {
    fn validate(&self) -> Result<(), RecordingError> {
        let mut ids = std::collections::HashSet::new();
        let mut public_ids = std::collections::HashSet::new();
        if self.peer_id.is_empty()
            || self.connection_generation.is_empty()
            || self.database_generation.is_empty()
            || self.members.is_empty()
            || self.members.len() > 128
            || self.max_items == 0
            || self.max_items as u64 > ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
            || self.max_bytes == 0
            || self.max_bytes as u64 > ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY
            || self.max_controls == 0
            || self.max_controls as u64 > ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
        {
            return Err(RecordingError::InvalidIdentity);
        }
        for member in &self.members {
            if !ids.insert(member.member_id)
                || member.public_member_id.is_empty()
                || member.public_member_id.len() > 256
                || !public_ids.insert(&member.public_member_id)
                || member.selector.service_uuid.is_empty()
                || member.selector.service_occurrence.is_none()
                || member.selector.characteristic_occurrence.is_none()
                || member
                    .selector
                    .characteristic_uuid
                    .as_ref()
                    .is_none_or(String::is_empty)
                || member.selector.descriptor_uuid.is_some()
                || member.selector.descriptor_occurrence.is_some()
            {
                return Err(RecordingError::InvalidIdentity);
            }
        }
        Ok(())
    }
    /// Parse and strictly validate the canonical persisted context.
    pub fn from_context(value: &Value) -> Result<Self, RecordingError> {
        fn string(value: &Value) -> Result<String, RecordingError> {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(RecordingError::InvalidIdentity)
        }
        fn decimal(value: &Value) -> Result<u64, RecordingError> {
            let text = value.as_str().ok_or(RecordingError::InvalidIdentity)?;
            let parsed: u64 = text.parse().map_err(|_| RecordingError::InvalidIdentity)?;
            if parsed.to_string() != text {
                return Err(RecordingError::InvalidIdentity);
            }
            Ok(parsed)
        }
        fn optional_decimal(value: &Value) -> Result<Option<u64>, RecordingError> {
            if value.is_null() {
                Ok(None)
            } else {
                decimal(value).map(Some)
            }
        }
        fn mode(value: &Value) -> Result<Option<DeliveryMode>, RecordingError> {
            match value.as_str() {
                Some("notification") => Ok(Some(DeliveryMode::Notification)),
                Some("indication") => Ok(Some(DeliveryMode::Indication)),
                None if value.is_null() => Ok(None),
                _ => Err(RecordingError::InvalidIdentity),
            }
        }
        fn capacity(value: &Value) -> Result<usize, RecordingError> {
            usize::try_from(value.as_u64().ok_or(RecordingError::InvalidIdentity)?)
                .map_err(|_| RecordingError::InvalidIdentity)
        }
        if value["kind"] != "ordered-recording" || value["version"] != 1 {
            return Err(RecordingError::InvalidIdentity);
        }
        let mut members = Vec::new();
        for member in value["members"]
            .as_array()
            .ok_or(RecordingError::InvalidIdentity)?
        {
            let selector = &member["selector"];
            members.push(OrderedRecordingMember {
                member_id: decimal(&member["memberId"])?,
                public_member_id: string(&member["publicMemberId"])?,
                selector: ubm_core::central::PathSelector {
                    service_uuid: string(&selector["serviceUuid"])?,
                    service_occurrence: optional_decimal(&selector["serviceOccurrence"])?,
                    characteristic_uuid: Some(string(&selector["characteristicUuid"])?),
                    characteristic_occurrence: optional_decimal(
                        &selector["characteristicOccurrence"],
                    )?,
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                },
                delivery: mode(&member["delivery"])?,
                preference: mode(&member["preference"])?,
            });
        }
        let declaration = Self {
            peer_id: string(&value["peerId"])?,
            connection_generation: string(&value["connectionGeneration"])?,
            database_generation: string(&value["databaseGeneration"])?,
            members,
            max_items: capacity(&value["maxItems"])?,
            max_bytes: capacity(&value["maxBytes"])?,
            max_controls: capacity(&value["maxControls"])?,
        };
        if declaration.context()? != *value {
            return Err(RecordingError::InvalidIdentity);
        }
        Ok(declaration)
    }

    pub fn context(&self) -> Result<Value, RecordingError> {
        self.validate()?;
        let members: Vec<Value> = self.members.iter().map(|member| json!({"memberId":member.member_id.to_string(),"publicMemberId":member.public_member_id,"selector":{"serviceUuid":member.selector.service_uuid,"serviceOccurrence":member.selector.service_occurrence.map(|v|v.to_string()),"characteristicUuid":member.selector.characteristic_uuid,"characteristicOccurrence":member.selector.characteristic_occurrence.map(|v|v.to_string())},"delivery":member.delivery.map(DeliveryMode::as_str),"preference":member.preference.map(DeliveryMode::as_str)})).collect();
        Ok(
            json!({"kind":"ordered-recording","version":1,"peerId":self.peer_id,"connectionGeneration":self.connection_generation,"databaseGeneration":self.database_generation,"maxItems":self.max_items,"maxBytes":self.max_bytes,"maxControls":self.max_controls,"members":members}),
        )
    }
}

/// Parameters for the one source-open attempt, owned across caller cancellation.
pub(crate) struct RecordingOpen {
    pub connection_generation: String,
    pub database_generation: String,
    pub requests: Vec<OrderedClaimRequest>,
    pub max_items: usize,
    pub max_bytes: usize,
    pub max_controls: usize,
    pub control: OpControl,
}

#[derive(Default)]
struct StartReceiptState {
    started: bool,
    retired: bool,
    failed: bool,
    receipt: Option<Value>,
}

#[derive(Default)]
struct OwnerState {
    token: Option<u64>,
    marked: bool,
    open_attempted: bool,
    generation: Option<SourceGeneration>,
    bound: bool,
    pending: Option<OrderedDelivery>,
    pending_acked: bool,
    storage_stopping: bool,
    open_error: Option<DesktopError>,
    terminal_committed: bool,
    sealed: bool,
    group_acked: bool,
    group_forgotten: bool,
    connection_lease: Option<String>,
    lease_acquired: bool,
    lease_failure: Option<DesktopError>,
    host_cleanup_settled: bool,
    finished: bool,
}

/// Whether the process supervisor independently retains the declared connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingConnectionLeaseStatus {
    Pending,
    Retained { generation: String },
    NotRetained,
}

/// Keep this owner alive across cancelled waiters; it owns the reservation and
/// its source cleanup obligations until explicit settlement.
pub struct OrderedRecording<B: RadioBoundary> {
    central: Arc<DesktopCentral<B>>,
    journal: Arc<ContinuationJournal>,
    recording_id: String,
    producer: String,
    recovery_capability: StdMutex<Option<String>>,
    peer: String,
    collection: u64,
    state: Arc<Mutex<OwnerState>>,
    declaration: Option<OrderedRecordingDeclaration>,
    start_gate: Arc<Mutex<()>>,
    start_result: StdMutex<StartReceiptState>,
    reclaim_authority: StdMutex<RecorderReclaimAuthority<B>>,
    driver_started: AtomicBool,
    driver_close: AtomicBool,
    driver_wake: tokio::sync::Notify,
    driver_finished: tokio::sync::Notify,
    driver_status: StdMutex<RecordingDriverStatus>,
}

struct RecorderReclaimAuthority<B: RadioBoundary> {
    stopping: bool,
    owner: Option<Arc<crate::reclaim_reservation::ReclaimReservationOwner<B>>>,
}

#[expect(
    clippy::result_large_err,
    reason = "The internal recovery-capability path returns the same persisted recording error used by its callers."
)]
fn random_recovery_capability() -> Result<String, RecordingError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| RecordingError::EntropyUnavailable)?;
    let mut capability = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut capability, "{byte:02x}").map_err(|_| RecordingError::EntropyUnavailable)?;
    }
    Ok(capability)
}

#[expect(
    clippy::result_large_err,
    reason = "The owner preserves complete source and durable-cleanup errors across its established internal result methods."
)]
impl<B: RadioBoundary> OrderedRecording<B> {
    /// Reserve an unopened collection. The caller must retain this handle even
    /// after a failed or cancelled operation, until cleanup is settled.
    #[cfg(test)]
    pub(crate) fn reserve(
        central: Arc<DesktopCentral<B>>,
        journal: Arc<ContinuationJournal>,
        recording_id: String,
        producer: String,
        peer: String,
    ) -> Result<Arc<Self>, RecordingError> {
        Self::reserve_internal(
            central,
            journal,
            recording_id,
            producer,
            peer,
            None,
            random_recovery_capability()?,
        )
    }
    fn reserve_internal(
        central: Arc<DesktopCentral<B>>,
        journal: Arc<ContinuationJournal>,
        recording_id: String,
        producer: String,
        peer: String,
        declaration: Option<OrderedRecordingDeclaration>,
        recovery_capability: String,
    ) -> Result<Arc<Self>, RecordingError> {
        if recording_id != journal.recording_id()
            || producer.is_empty()
            || producer.len() > 256
            || peer.is_empty()
        {
            return Err(RecordingError::InvalidIdentity);
        }
        let _registration = central
            .begin_ordered_recording_registration()
            .map_err(RecordingError::Source)?;
        crate::continuation_journal::register_ordered_supervisor(&journal, &producer, || {
            let (collection, token) = central
                .reserve_ordered_recording_guarded(&peer)
                .map_err(RecordingError::Source)?;
            Ok(Arc::new(Self {
                central: Arc::clone(&central),
                journal: Arc::clone(&journal),
                declaration,
                recording_id,
                producer: producer.clone(),
                recovery_capability: StdMutex::new(Some(recovery_capability)),
                peer,
                collection,
                state: Arc::new(Mutex::new(OwnerState {
                    token: Some(token),
                    ..OwnerState::default()
                })),
                start_gate: Arc::new(Mutex::new(())),
                start_result: StdMutex::new(StartReceiptState::default()),
                reclaim_authority: StdMutex::new(RecorderReclaimAuthority {
                    stopping: false,
                    owner: None,
                }),
                driver_started: AtomicBool::new(false),
                driver_close: AtomicBool::new(false),
                driver_wake: tokio::sync::Notify::new(),
                driver_finished: tokio::sync::Notify::new(),
                driver_status: StdMutex::new(RecordingDriverStatus::Idle),
            }))
        })
    }
    /// Persist the exact declaration and reserve a supervised unopened group.
    /// OS entropy is acquired before journal or central effects.
    pub async fn prepare(
        central: Arc<DesktopCentral<B>>,
        registry: Arc<JournalRegistry>,
        recording_id: String,
        quota: JournalQuota,
        declaration: OrderedRecordingDeclaration,
    ) -> Result<Arc<Self>, RecordingError> {
        let context = declaration.context()?;
        let mut nonce = [0u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| RecordingError::EntropyUnavailable)?;
        let mut producer = String::with_capacity(64);
        for byte in nonce {
            use std::fmt::Write;
            write!(&mut producer, "{byte:02x}").map_err(|_| RecordingError::EntropyUnavailable)?;
        }
        let recovery_capability = random_recovery_capability()?;
        let open_registry = Arc::clone(&registry);
        let id = recording_id.clone();
        let journal = tokio::task::spawn_blocking(move || open_registry.open(&id, &context, quota))
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
            .map_err(RecordingError::Storage)?;
        if let Some(owner) = registry
            .ordered_owner(&recording_id, &central)
            .map_err(RecordingError::Storage)?
        {
            return Ok(owner);
        }
        let status_journal = Arc::clone(&journal);
        let accepting = tokio::task::spawn_blocking(move || status_journal.status())
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
            .map_err(RecordingError::Storage)?;
        if accepting["accepting"] != true {
            return Err(RecordingError::RecordingNotAccepting);
        }
        let peer = declaration.peer_id.clone();
        match Self::reserve_internal(
            Arc::clone(&central),
            journal,
            recording_id.clone(),
            producer,
            peer,
            Some(declaration),
            recovery_capability,
        ) {
            Err(RecordingError::Storage(error)) if error.kind == "storage.owner-conflict" => {
                registry
                    .ordered_owner(&recording_id, &central)
                    .map_err(RecordingError::Storage)?
                    .ok_or(RecordingError::Storage(error))
            }
            result => result,
        }
    }

    fn declared_options(&self, control: OpControl) -> Result<RecordingOpen, RecordingError> {
        let declaration = self
            .declaration
            .clone()
            .ok_or(RecordingError::InvalidIdentity)?;
        let requests = declaration
            .members
            .iter()
            .map(|member| OrderedClaimRequest {
                member_id: member.member_id,
                selector: member.selector.clone(),
                consumer: format!("ordered-{}-{}", self.collection, member.member_id),
                delivery: member.delivery,
                preference: member.preference,
            })
            .collect();
        Ok(RecordingOpen {
            connection_generation: declaration.connection_generation,
            database_generation: declaration.database_generation,
            requests,
            max_items: declaration.max_items,
            max_bytes: declaration.max_bytes,
            max_controls: declaration.max_controls,
            control,
        })
    }

    /// Return a setup receipt even on partial failure; the process supervisor
    /// retains the owner and setup observations flow to its journal driver.
    pub async fn start_receipt(
        self: &Arc<Self>,
        control: OpControl,
    ) -> Result<Value, RecordingError> {
        let options = self.declared_options(control)?;
        // Own the gate before publishing pending, so retirement cannot pass
        // a start whose worker has not yet been scheduled.
        let gate = Arc::clone(&self.start_gate).lock_owned().await;
        {
            let mut receipt = self
                .start_result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if receipt.retired {
                return Err(RecordingError::ReceiptRetired);
            }
            if receipt.failed && receipt.receipt.is_none() {
                return Err(RecordingError::WorkerFailed);
            }
            if let Some(cached) = &receipt.receipt {
                return Ok(cached.clone());
            }
            receipt.started = true;
        }
        let owner = Arc::clone(self);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _gate = gate;
            let cached = {
                let state = owner
                    .start_result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.retired {
                    Some(Err(RecordingError::ReceiptRetired))
                } else if state.failed {
                    Some(Err(RecordingError::WorkerFailed))
                } else {
                    state.receipt.as_ref().map(|receipt| Ok(receipt.clone()))
                }
            };
            if let Some(result) = cached {
                let _ = sender.send(result);
                return;
            }
            let worker_owner = Arc::clone(&owner);
            let worker = tokio::spawn(async move {
                let owner = worker_owner;
                let opened = owner.open(options).await;
                let (attempted, generation) = {
                    let state = owner.state.lock().await;
                    (state.open_attempted, state.generation.clone())
                };
                let generation = generation.map(|g| match g {
                    SourceGeneration::Counter(v) => json!({"kind":"counter","value":v.to_string()}),
                    SourceGeneration::Opaque(v) => json!({"kind":"opaque","value":v}),
                });
                let mut inspection_error = None;
                let members = match owner.central.ordered_batch_member_status(&owner.peer,owner.collection).await {
                Ok(values) => values.into_iter().map(|member| json!({"memberId":member.member_id.to_string(),"acquisition":match member.acquisition { OrderedMemberAcquisition::NotStarted=>"not-started",OrderedMemberAcquisition::Acquired=>"acquired",OrderedMemberAcquisition::Indeterminate=>"indeterminate" },"delivery":match member.effective_delivery { crate::boundary::ObservedDelivery::Notification=>"notification",crate::boundary::ObservedDelivery::Indication=>"indication",crate::boundary::ObservedDelivery::Unknown=>"unknown" },"cleanupReleased":member.cleanup_released,"cleanupError":member.cleanup_error.as_ref().map(|error|failure_v2(&crate::ordered_notification_groups::failure_from_desktop(error)))})).collect::<Vec<_>>(),
                Err(error) => { inspection_error = Some(stop_failure(&RecordingError::Source(error))); Vec::new() }
            };
                let (setup, fence) = match owner
                    .central
                    .retained_ordered_batch_errors(&owner.peer, owner.collection)
                    .await
                {
                    Ok(values) => values,
                    Err(error) => {
                        if inspection_error.is_none() {
                            inspection_error = Some(stop_failure(&RecordingError::Source(error)));
                        }
                        (None, None)
                    }
                };
                let driver = if attempted {
                    owner.start_driver().await.err()
                } else {
                    None
                };
                if let Some(error) = &driver {
                    owner.set_driver_status(RecordingDriverStatus::Failed(error.clone()));
                    owner.driver_finished.notify_waiters();
                }
                let state = if !attempted || driver.is_some() {
                    "failed"
                } else if opened.is_err() {
                    "setup-failed"
                } else {
                    "active"
                };
                let receipt = json!({"version":2,"recordingId":owner.recording_id,"collectionId":owner.collection.to_string(),"state":state,"sourceGeneration":generation,"members":members,"setupFailure":setup.as_ref().map(|error|failure_v2(&crate::ordered_notification_groups::failure_from_desktop(error))),"fenceFailure":fence.as_ref().map(|error|failure_v2(&crate::ordered_notification_groups::failure_from_desktop(error))),"error":opened.as_ref().err().map(stop_failure),"inspectionError":inspection_error,"driverError":driver.as_ref().map(stop_failure)});
                owner
                    .start_result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .receipt = Some(receipt.clone());
                Ok(receipt)
            });
            let result = match worker.await {
                Ok(result) => result,
                Err(_) => {
                    owner
                        .start_result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .failed = true;
                    Err(RecordingError::WorkerFailed)
                }
            };
            let _ = sender.send(result);
        });
        receiver.await.map_err(|_| RecordingError::WorkerFailed)?
    }

    /// Host-authorized status never waits for an in-flight source Open.
    pub fn start_status(&self) -> Value {
        let state = self
            .start_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let label = if state.retired {
            "retired"
        } else if state.receipt.is_some() {
            "ready"
        } else if state.failed {
            "failed"
        } else if state.started {
            "pending"
        } else {
            "not-started"
        };
        json!({"version":1,"state":label,"receipt":state.receipt})
    }
    /// Explicitly discard only the retained start receipt; never release the
    /// source, its supervisor, or journal records. Repeated retirement is safe.
    pub async fn retire_start_receipt(&self) -> Result<Value, RecordingError> {
        let _gate = self.start_gate.lock().await;
        let mut state = self
            .start_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.retired {
            return Ok(json!({"version":1,"retired":true}));
        }
        if !state.started || (state.receipt.is_none() && !state.failed) {
            return Err(RecordingError::InvalidIdentity);
        }
        state.receipt = None;
        state.retired = true;
        Ok(json!({"version":1,"retired":true}))
    }

    pub(crate) fn belongs_to(&self, central: &DesktopCentral<B>) -> bool {
        self.central.same_authority(central)
    }

    pub fn driver_status(&self) -> RecordingDriverStatus {
        self.driver_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_driver_status(&self, status: RecordingDriverStatus) {
        *self
            .driver_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }
    /// Start one host-retained driver after Open. Errors remain inspectable and
    /// require an explicit retry wake; capacity waits for reader acknowledgement.
    pub async fn start_driver(self: &Arc<Self>) -> Result<(), RecordingError> {
        let state = self.state.lock().await;
        if !state.open_attempted || state.finished {
            return Err(RecordingError::InvalidIdentity);
        }
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        if state.open_error.is_some() {
            self.driver_close.store(true, Ordering::SeqCst);
        }
        let source_wake = self
            .central
            .ordered_recording_wake(&self.peer, self.collection, token)
            .await
            .map_err(RecordingError::Source)?;
        if self.driver_started.swap(true, Ordering::SeqCst) {
            return Err(RecordingError::InvalidIdentity);
        }
        drop(state);
        self.set_driver_status(RecordingDriverStatus::Running);
        let owner = Arc::clone(self);
        tokio::spawn(async move {
            let worker = Arc::clone(&owner);
            if tokio::spawn(async move { worker.drive(source_wake).await })
                .await
                .is_err()
            {
                owner.revoke_reclaims();
                owner.driver_started.store(false, Ordering::SeqCst);
                owner
                    .set_driver_status(RecordingDriverStatus::Failed(RecordingError::WorkerFailed));
                owner.driver_finished.notify_waiters();
            }
        });
        Ok(())
    }
    /// Request source fence, durable drain and cleanup. Call `wait_driver` to
    /// observe completion or inspect `driver_status` for a retained failure.
    pub fn request_driver_close(&self) {
        self.revoke_reclaims();
        self.driver_close.store(true, Ordering::SeqCst);
        self.driver_wake.notify_one();
    }
    fn revoke_reclaims(&self) {
        let mut authority = self
            .reclaim_authority
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        authority.stopping = true;
        if let Some(owner) = &authority.owner {
            owner.revoke_and_cleanup();
        }
    }
    /// Host shutdown requests closure and returns a truthful terminal, pending,
    /// capacity or failure receipt. Pending work remains supervised for retry.
    pub async fn shutdown(self: &Arc<Self>) -> Result<RecordingDriverStatus, RecordingError> {
        // A start owns this gate through its setup receipt and driver install.
        // Shutdown cannot release its group between those two observations.
        let start = self.start_gate.lock().await;
        if !self.driver_started.load(Ordering::SeqCst) {
            let state = self.state.lock().await;
            let opened = state.open_attempted;
            drop(state);
            if !opened {
                return match self.close(OpControl::budget_ms(5000)).await? {
                    RecordingClose::Finished => Ok(RecordingDriverStatus::Finished),
                    RecordingClose::PendingSource => Ok(RecordingDriverStatus::WaitingSource),
                    RecordingClose::PendingDrain | RecordingClose::Progress => {
                        Ok(RecordingDriverStatus::WaitingDrain)
                    }
                };
            }
            self.start_driver().await?;
        }
        self.request_driver_close();
        drop(start);
        Ok(self.wait_driver().await)
    }
    /// Settle this exact process-owned recorder and preserve its host cleanup receipt.
    pub async fn shutdown_host_receipt(self: &Arc<Self>) -> Value {
        let recording = stop_receipt(self.shutdown().await);
        let host_cleanup =
            crate::continuation_journal::OrderedSupervisor::host_cleanup(Arc::clone(self)).await;
        json!({"recording":recording,"hostCleanup":host_cleanup})
    }
    pub fn retry_driver(&self) {
        self.driver_wake.notify_one();
    }
    pub async fn wait_driver(&self) -> RecordingDriverStatus {
        loop {
            let notified = self.driver_finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let status = self.driver_status();
            if matches!(
                status,
                RecordingDriverStatus::Finished
                    | RecordingDriverStatus::Failed(_)
                    | RecordingDriverStatus::WaitingCapacity
                    | RecordingDriverStatus::WaitingSource
                    | RecordingDriverStatus::WaitingDrain
                    | RecordingDriverStatus::Idle
            ) {
                return status;
            }
            notified.await;
        }
    }
    async fn drive(self: Arc<Self>, source_wake: Arc<tokio::sync::Notify>) {
        let space = self.journal.reader_space_wake();
        loop {
            // Register before polling so an edge arriving during work is retained.
            let source_ready = source_wake.notified();
            tokio::pin!(source_ready);
            source_ready.as_mut().enable();
            let space_ready = space.notified();
            tokio::pin!(space_ready);
            space_ready.as_mut().enable();
            let request_ready = self.driver_wake.notified();
            tokio::pin!(request_ready);
            request_ready.as_mut().enable();
            let closing = self.driver_close.load(Ordering::SeqCst);
            self.set_driver_status(if closing {
                RecordingDriverStatus::Closing
            } else {
                RecordingDriverStatus::Running
            });
            let outcome = if closing {
                match self.close(OpControl::budget_ms(5000)).await {
                    Ok(RecordingClose::Finished) => {
                        self.set_driver_status(RecordingDriverStatus::Finished);
                        self.driver_finished.notify_waiters();
                        return;
                    }
                    Ok(RecordingClose::Progress) => continue,
                    Ok(RecordingClose::PendingSource) => {
                        self.set_driver_status(RecordingDriverStatus::WaitingSource);
                        self.driver_finished.notify_waiters();
                        Ok(())
                    }
                    Ok(RecordingClose::PendingDrain) => {
                        self.set_driver_status(RecordingDriverStatus::WaitingDrain);
                        self.driver_finished.notify_waiters();
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            } else {
                match self.step().await {
                    Ok(RecordingStep::Committed) => continue,
                    Ok(RecordingStep::TerminalCommitted | RecordingStep::Closed) => {
                        self.revoke_reclaims();
                        self.driver_close.store(true, Ordering::SeqCst);
                        continue;
                    }
                    Ok(RecordingStep::PendingSource) => {
                        self.set_driver_status(RecordingDriverStatus::WaitingSource);
                        self.driver_finished.notify_waiters();
                        Ok(())
                    }
                    Ok(RecordingStep::Empty) => Ok(()),
                    Err(error) => Err(error),
                }
            };
            match outcome {
                Err(RecordingError::Storage(error)) if error.kind == "storage.full" => {
                    self.revoke_reclaims();
                    self.driver_close.store(true, Ordering::SeqCst);
                    self.set_driver_status(RecordingDriverStatus::WaitingCapacity);
                    self.driver_finished.notify_waiters();
                    tokio::select! { _ = &mut space_ready => {}, _ = &mut request_ready => {} }
                }
                Err(error) => {
                    self.revoke_reclaims();
                    if matches!(&error, RecordingError::StorageCleanup(storage, _) if storage.kind == "storage.full")
                    {
                        self.driver_close.store(true, Ordering::SeqCst);
                    }
                    self.set_driver_status(RecordingDriverStatus::Failed(error));
                    self.driver_finished.notify_waiters();
                    request_ready.await;
                }
                Ok(()) => {
                    tokio::select! { _ = &mut source_ready => {}, _ = &mut space_ready => {}, _ = &mut request_ready => {} }
                }
            }
        }
    }

    /// Inspect the next marker identity and whether a transaction is retained.
    pub async fn marker_next_id(&self) -> Result<(Option<u64>, bool), RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .ordered_marker_next_id(&self.peer, self.collection, Some(token))
            .await
            .map_err(RecordingError::Source)
    }

    /// Resolve the write target and exact native snapshot under recorder ownership.
    pub async fn prepare_marker_request(
        &self,
        marker_id: u64,
        selector: &ubm_core::central::PathSelector,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<crate::boundary::OrderedMarkerWriteRequest, RecordingError> {
        let token = {
            let state = self.state.lock().await;
            if state.finished || state.sealed {
                return Err(RecordingError::InvalidIdentity);
            }
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .prepare_ordered_marker_request(
                &self.peer,
                self.collection,
                Some(token),
                marker_id,
                selector,
                value,
                with_response,
            )
            .await
            .map_err(RecordingError::Source)
    }

    /// Recover the original native-resolved request by its exact marker ID.
    pub async fn retained_marker_request(
        &self,
        marker_id: u64,
    ) -> Result<crate::boundary::OrderedMarkerWriteRequest, RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .retained_ordered_marker_request(&self.peer, self.collection, Some(token), marker_id)
            .await
            .map_err(RecordingError::Source)
    }

    pub async fn marker_write_status_by_id(
        &self,
        marker_id: u64,
    ) -> Result<crate::boundary::OrderedMarkerWriteStatus, RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .ordered_marker_write_status_by_id(&self.peer, self.collection, Some(token), marker_id)
            .await
            .map_err(RecordingError::Source)
    }
    /// Request cancellation of the retained transaction; returned status is
    /// the native outcome, not an inference from the request.
    pub async fn cancel_marker_write_by_id(
        &self,
        marker_id: u64,
    ) -> Result<crate::boundary::OrderedMarkerWriteStatus, RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .cancel_ordered_marker_write_by_id(&self.peer, self.collection, Some(token), marker_id)
            .await
            .map_err(RecordingError::Source)
    }

    pub async fn forget_marker_write_by_id(&self, marker_id: u64) -> Result<(), RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .forget_ordered_marker_write_by_id(&self.peer, self.collection, Some(token), marker_id)
            .await
            .map_err(RecordingError::Source)
    }

    /// Start or replay one exact source marker/write transaction. This marker
    /// is a source-order boundary, not proof of radio causality.
    pub async fn start_marker_write(
        &self,
        request: crate::boundary::OrderedMarkerWriteRequest,
    ) -> Result<crate::boundary::OrderedMarkerWriteStatus, RecordingError> {
        let token = {
            let state = self.state.lock().await;
            if state.finished || state.sealed {
                return Err(RecordingError::InvalidIdentity);
            }
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .start_ordered_marker_write(&self.peer, self.collection, Some(token), request)
            .await
            .map_err(RecordingError::Source)
    }
    pub async fn marker_write_status(
        &self,
        request: &crate::boundary::OrderedMarkerWriteRequest,
    ) -> Result<crate::boundary::OrderedMarkerWriteStatus, RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .ordered_marker_write_status(&self.peer, self.collection, Some(token), request)
            .await
            .map_err(RecordingError::Source)
    }
    /// Retire only the exact terminal native result after journaled source
    /// evidence was acknowledged. Retry failures retain the transaction.
    pub async fn forget_marker_write(
        &self,
        request: &crate::boundary::OrderedMarkerWriteRequest,
    ) -> Result<(), RecordingError> {
        let token = {
            let state = self.state.lock().await;
            state.token.ok_or(RecordingError::InvalidIdentity)?
        };
        self.central
            .forget_ordered_marker_write(&self.peer, self.collection, Some(token), request)
            .await
            .map_err(RecordingError::Source)
    }

    pub fn collection_id(&self) -> u64 {
        self.collection
    }
    /// Return this recorder's shared marker reservation authority.
    pub async fn marker_reservation_owner(
        &self,
    ) -> Result<Arc<crate::marker_reservation::MarkerReservationOwner<B>>, RecordingError> {
        let state = self.state.lock().await;
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        self.central
            .marker_reservation_owner(&self.peer, self.collection, Some(token))
            .await
            .map_err(RecordingError::Source)
    }
    /// Return the shared reclaim reservation without exposing its recorder token.
    pub async fn reclaim_reservation_owner(
        &self,
        capability: &str,
    ) -> Result<Arc<crate::reclaim_reservation::ReclaimReservationOwner<B>>, RecordingError> {
        self.validate_recovery_capability(capability)?;
        let state = self.state.lock().await;
        if state.finished || state.group_acked {
            return Err(RecordingError::InvalidIdentity);
        }
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        drop(state);
        let owner = self
            .central
            .reclaim_reservation_owner(&self.peer, self.collection, token)
            .await
            .map_err(RecordingError::Source)?;
        self.validate_recovery_capability(capability)?;
        owner
            .bind_capability(capability)
            .map_err(RecordingError::Source)?;
        let mut authority = self
            .reclaim_authority
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if authority.stopping {
            owner.revoke_and_cleanup();
            return Err(RecordingError::InvalidIdentity);
        }
        authority.owner = Some(Arc::clone(&owner));
        Ok(owner)
    }

    /// Revalidate an exact start retry even after the supervisor has finished.
    pub fn matches_start(
        &self,
        declaration: &OrderedRecordingDeclaration,
        quota: JournalQuota,
    ) -> Result<bool, RecordingError> {
        let context = declaration.context()?;
        self.journal
            .matches_declaration(&context, quota)
            .map_err(RecordingError::Storage)
    }
    pub fn recording_id(&self) -> &str {
        &self.recording_id
    }
    pub fn peer_id(&self) -> &str {
        &self.peer
    }

    /// Call only after authenticating the original start session; this secret
    /// remains process-owned and must never be written to the journal.
    pub fn issue_recovery_capability(&self) -> Result<String, RecordingError> {
        self.recovery_capability
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(RecordingError::InvalidIdentity)
    }

    /// Authenticate every recovery operation, including receipt queries.
    pub fn validate_recovery_capability(&self, capability: &str) -> Result<(), RecordingError> {
        let secret = self
            .recovery_capability
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(expected) = secret.as_ref() else {
            return Err(RecordingError::InvalidIdentity);
        };
        let supplied = capability.as_bytes();
        if supplied.len() != expected.len() {
            return Err(RecordingError::InvalidIdentity);
        }
        let mismatch = expected
            .as_bytes()
            .iter()
            .zip(supplied)
            .fold(0u8, |value, (a, b)| value | (a ^ b));
        if mismatch != 0 {
            return Err(RecordingError::InvalidIdentity);
        }
        Ok(())
    }

    /// Pin the recorder's exact group and graph while an authenticated host
    /// transfers its session mapping to the caller's current connection lease.
    pub async fn acquire_reclaim(
        &self,
        capability: &str,
        caller_lease: &str,
    ) -> Result<crate::central::OrderedRecordingReclaimPermit, RecordingError> {
        self.validate_recovery_capability(capability)?;
        let state = self.state.lock().await;
        if state.finished || state.group_acked {
            return Err(RecordingError::InvalidIdentity);
        }
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        drop(state);
        self.central
            .acquire_ordered_recording_reclaim(&self.peer, self.collection, token, caller_lease)
            .await
            .map_err(RecordingError::Source)
    }

    /// A busy owner may be between admission and retention; do not infer failure
    /// or release the session lease until that operation becomes observable.
    pub fn connection_lease_status(&self) -> RecordingConnectionLeaseStatus {
        let Ok(state) = self.state.try_lock() else {
            return RecordingConnectionLeaseStatus::Pending;
        };
        if state.connection_lease.is_some() {
            return match &self.declaration {
                Some(declaration) => RecordingConnectionLeaseStatus::Retained {
                    generation: declaration.connection_generation.clone(),
                },
                None => RecordingConnectionLeaseStatus::Pending,
            };
        }
        if state.finished || state.lease_acquired || state.open_error.is_some() {
            RecordingConnectionLeaseStatus::NotRetained
        } else {
            RecordingConnectionLeaseStatus::Pending
        }
    }

    /// Claim the reservation and durably mark it live before attempting Open.
    /// An error after Open starts leaves the owner available for cleanup/recovery.
    pub(crate) async fn open(
        self: &Arc<Self>,
        options: RecordingOpen,
    ) -> Result<(), RecordingError> {
        let state = Arc::clone(&self.state).lock_owned().await;
        let owner = Arc::clone(self);
        tokio::spawn(async move { owner.open_owned(state, options).await })
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
    }
    async fn open_owned(
        &self,
        mut state: tokio::sync::OwnedMutexGuard<OwnerState>,
        mut options: RecordingOpen,
    ) -> Result<(), RecordingError> {
        if state.open_attempted || state.finished || state.group_acked {
            return Err(RecordingError::InvalidIdentity);
        }
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        if !state.marked {
            let journal = Arc::clone(&self.journal);
            let producer = self.producer.clone();
            let collection = self.collection.to_string();
            tokio::task::spawn_blocking(move || {
                crate::continuation_journal::begin_supervised_ordered(
                    &journal,
                    &producer,
                    &collection,
                )
            })
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
            .map_err(RecordingError::Storage)?;
            state.marked = true;
        }
        state.open_attempted = true;
        if self.declaration.is_some() {
            let source = options.control.connection_lease().unwrap_or("").to_owned();
            let lease = format!("ordered-recording-{}", self.producer);
            if let Err(error) = self
                .central
                .retain_connection_lease(
                    &self.peer,
                    &source,
                    &lease,
                    &options.connection_generation,
                )
                .await
            {
                state.open_error = Some(error.clone());
                return Err(RecordingError::Source(error));
            }
            state.connection_lease = Some(lease.clone());
            state.lease_acquired = true;
            options.control = options.control.with_connection_lease(lease);
        }
        let result = self
            .central
            .open_ordered_recording(
                &self.peer,
                self.collection,
                token,
                &options.connection_generation,
                &options.database_generation,
                &options.requests,
                options.max_items,
                options.max_bytes,
                options.max_controls,
                options.control,
            )
            .await;
        state.open_error = result.as_ref().err().cloned();
        state.generation = self
            .central
            .ordered_recording_source_generation(&self.peer, self.collection, token)
            .await
            .map_err(RecordingError::Source)?;
        if state.generation.is_some() {
            self.bind_generation(&mut state).await?;
        }
        result.map_err(RecordingError::Source)
    }
    async fn bind_generation(&self, state: &mut OwnerState) -> Result<(), RecordingError> {
        if state.bound {
            return Ok(());
        }
        let generation = state
            .generation
            .as_ref()
            .ok_or(RecordingError::InvalidIdentity)?;
        let (kind, value) = match generation {
            SourceGeneration::Counter(v) => ("counter", v.to_string()),
            SourceGeneration::Opaque(v) => ("opaque", v.clone()),
        };
        let journal = Arc::clone(&self.journal);
        let producer = self.producer.clone();
        tokio::task::spawn_blocking(move || {
            journal.bind_ordered_generation(&producer, kind, &value)
        })
        .await
        .map_err(|_| RecordingError::WorkerFailed)?
        .map_err(RecordingError::Storage)?;
        state.bound = true;
        Ok(())
    }

    /// Commit one retained record and then acknowledge exactly that delivery.
    pub async fn step(self: &Arc<Self>) -> Result<RecordingStep, RecordingError> {
        let state = Arc::clone(&self.state).lock_owned().await;
        let owner = Arc::clone(self);
        tokio::spawn(async move { owner.step_owned(state).await })
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
    }
    async fn step_owned(
        &self,
        mut state: tokio::sync::OwnedMutexGuard<OwnerState>,
    ) -> Result<RecordingStep, RecordingError> {
        if state.finished || state.sealed || state.group_acked {
            return Ok(RecordingStep::Closed);
        }
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        if state.storage_stopping
            && self
                .central
                .retry_ordered_recording_cleanup(
                    &self.peer,
                    self.collection,
                    token,
                    OpControl::budget_ms(5000),
                )
                .await
                .map_err(RecordingError::Source)?
                == OrderedCleanupOutcome::PendingSource
        {
            return Ok(RecordingStep::PendingSource);
        }
        if state.generation.is_none() {
            state.generation = self
                .central
                .ordered_recording_source_generation(&self.peer, self.collection, token)
                .await
                .map_err(RecordingError::Source)?;
        }
        self.bind_generation(&mut state).await?;
        if state.pending.is_none() {
            let delivery = self
                .central
                .poll_ordered_recording(&self.peer, self.collection, token)
                .await
                .map_err(RecordingError::Source)?;
            match (&delivery.delivery_id, &delivery.poll) {
                (None, Poll::Empty) => return Ok(RecordingStep::Empty),
                (None, Poll::Closed) => return Ok(RecordingStep::Closed),
                (Some(_), Poll::Record(_) | Poll::Terminal(_)) => state.pending = Some(delivery),
                _ => return Err(RecordingError::InvalidDelivery),
            }
        }
        let terminal = self.commit_pending(&mut state, token).await?;
        Ok(if terminal {
            RecordingStep::TerminalCommitted
        } else {
            RecordingStep::Committed
        })
    }
    async fn commit_pending(
        &self,
        state: &mut OwnerState,
        token: u64,
    ) -> Result<bool, RecordingError> {
        let identity = RecordingIdentity {
            recording_id: self.recording_id.clone(),
            producer_incarnation: self.producer.clone(),
            peer_id: self.peer.clone(),
            collection_id: self.collection,
            source_generation: state
                .generation
                .clone()
                .ok_or(RecordingError::InvalidIdentity)?,
        };
        let delivery = state
            .pending
            .as_ref()
            .ok_or(RecordingError::InvalidDelivery)?;
        let id = delivery
            .delivery_id
            .ok_or(RecordingError::InvalidDelivery)?;
        let terminal = matches!(delivery.poll, Poll::Terminal(_));
        if !state.pending_acked {
            match commit_then_ack(&self.journal, &identity, delivery, |id| async move {
                self.central
                    .acknowledge_ordered_recording(&self.peer, self.collection, token, id)
                    .await
            })
            .await
            {
                Err(RecordingError::Storage(error)) if error.kind == "storage.full" => {
                    state.storage_stopping = true;
                    return match self
                        .central
                        .retry_ordered_recording_cleanup(
                            &self.peer,
                            self.collection,
                            token,
                            OpControl::budget_ms(5000),
                        )
                        .await
                    {
                        Ok(_) => Err(RecordingError::Storage(error)),
                        Err(cleanup) => Err(RecordingError::StorageCleanup(error, cleanup)),
                    };
                }
                Err(error) => return Err(error),
                Ok(()) => state.pending_acked = true,
            }
        }
        let journal = Arc::clone(&self.journal);
        let producer = identity.producer_incarnation;
        let collection = identity.collection_id.to_string();
        let (kind, generation) = match identity.source_generation {
            SourceGeneration::Counter(value) => ("counter", value.to_string()),
            SourceGeneration::Opaque(value) => ("opaque", value),
        };
        let retirement = tokio::task::spawn_blocking(move || {
            journal.retire_ordered_identity(
                &producer,
                &collection,
                kind,
                &generation,
                &id.to_string(),
            )
        })
        .await
        .map_err(|_| RecordingError::WorkerFailed)?;
        if let Err(error) = retirement {
            if error.kind == "storage.full" {
                state.storage_stopping = true;
                return match self
                    .central
                    .retry_ordered_recording_cleanup(
                        &self.peer,
                        self.collection,
                        token,
                        OpControl::budget_ms(5000),
                    )
                    .await
                {
                    Ok(_) => Err(RecordingError::Storage(error)),
                    Err(cleanup) => Err(RecordingError::StorageCleanup(error, cleanup)),
                };
            }
            return Err(RecordingError::Storage(error));
        }
        state.pending = None;
        state.pending_acked = false;
        state.terminal_committed |= terminal;
        Ok(terminal)
    }

    /// Fence/retry cleanup and commit one admitted record per call. The caller
    /// continues until Finished; pending source and storage failures retain owner.
    pub async fn close(
        self: &Arc<Self>,
        control: OpControl,
    ) -> Result<RecordingClose, RecordingError> {
        self.revoke_reclaims();
        let state = Arc::clone(&self.state).lock_owned().await;
        let owner = Arc::clone(self);
        tokio::spawn(async move { owner.close_owned(state, control).await })
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
    }
    async fn close_owned(
        &self,
        mut state: tokio::sync::OwnedMutexGuard<OwnerState>,
        control: OpControl,
    ) -> Result<RecordingClose, RecordingError> {
        if state.finished {
            return Ok(RecordingClose::Finished);
        }
        let token = state.token.ok_or(RecordingError::InvalidIdentity)?;
        if state.sealed || state.group_acked {
            if !state.group_acked {
                self.central
                    .acknowledge_ordered_recording_batch(&self.peer, self.collection, token)
                    .await
                    .map_err(RecordingError::Source)?;
                state.group_acked = true;
            }
            return self.finish_cleanup(&mut state).await;
        }
        if state.generation.is_none() {
            state.generation = self
                .central
                .ordered_recording_source_generation(&self.peer, self.collection, token)
                .await
                .map_err(RecordingError::Source)?;
        }
        let cleanup = self
            .central
            .retry_ordered_recording_cleanup(&self.peer, self.collection, token, control)
            .await
            .map_err(RecordingError::Source)?;
        if cleanup == OrderedCleanupOutcome::PendingSource {
            return Ok(RecordingClose::PendingSource);
        }
        if !state.marked && !state.open_attempted && cleanup == OrderedCleanupOutcome::Released {
            self.central
                .acknowledge_ordered_recording_batch(&self.peer, self.collection, token)
                .await
                .map_err(RecordingError::Source)?;
            state.group_acked = true;
            return self.finish_cleanup(&mut state).await;
        }
        if state.generation.is_none() && cleanup == OrderedCleanupOutcome::Released {
            let observed = self
                .central
                .ordered_recording_source_generation(&self.peer, self.collection, token)
                .await
                .map_err(RecordingError::Source)?;
            if observed.is_none() && !state.terminal_committed {
                let source_error = state
                    .open_error
                    .as_ref()
                    .ok_or(RecordingError::MissingTerminal)?;
                let evidence =
                    try_v2_failure(source_error).ok_or(RecordingError::InvalidDelivery)?;
                let (setup_error, fence_error) = self
                    .central
                    .retained_ordered_batch_errors(&self.peer, self.collection)
                    .await
                    .map_err(RecordingError::Source)?;
                let members = self.central.ordered_batch_member_status(&self.peer,self.collection).await.map_err(RecordingError::Source)?.into_iter().map(|member| {
                    let cleanup = member.cleanup_error.as_ref().map(|error| try_v2_failure(error).ok_or(RecordingError::InvalidDelivery)).transpose()?;
                    Ok(json!({"member":member.member_id.to_string(),"acquisition":match member.acquisition { OrderedMemberAcquisition::NotStarted => "not-started", OrderedMemberAcquisition::Acquired => "acquired", OrderedMemberAcquisition::Indeterminate => "indeterminate" },"delivery":match member.effective_delivery { crate::boundary::ObservedDelivery::Notification=>"notification",crate::boundary::ObservedDelivery::Indication=>"indication",crate::boundary::ObservedDelivery::Unknown=>"unknown" },"cleanupReleased":member.cleanup_released,"cleanupError":cleanup}))
                }).collect::<Result<Vec<_>, RecordingError>>()?;
                let setup = setup_error
                    .as_ref()
                    .map(|error| try_v2_failure(error).ok_or(RecordingError::InvalidDelivery))
                    .transpose()?;
                let fence = fence_error
                    .as_ref()
                    .map(|error| try_v2_failure(error).ok_or(RecordingError::InvalidDelivery))
                    .transpose()?;
                let metadata = json!({"kind":"ordered","version":2,"recordingId":self.recording_id,"peerId":self.peer,"collectionId":self.collection.to_string(),"sourceGeneration":null,"deliveryId":null});
                let record = json!({"kind":"setup-terminal","sourceCreated":false,"tailUncertain":false,"failure":evidence,"setupFailure":setup,"fenceFailure":fence,"members":members});
                if serde_json::to_vec(&record).map_or(true, |bytes| bytes.len() > 1_048_576) {
                    return Err(RecordingError::InvalidDelivery);
                }
                let journal = Arc::clone(&self.journal);
                let producer = self.producer.clone();
                let collection = self.collection.to_string();
                tokio::task::spawn_blocking(move || {
                    journal.append_ordered(
                        &producer,
                        &collection,
                        "no-source",
                        "not-created",
                        "setup-terminal",
                        &metadata,
                        &record,
                    )
                })
                .await
                .map_err(|_| RecordingError::WorkerFailed)?
                .map_err(RecordingError::Storage)?;
                state.terminal_committed = true;
            } else {
                state.generation = observed;
            }
        }
        if !state.terminal_committed || state.pending.is_some() {
            if state.generation.is_none() {
                state.generation = self
                    .central
                    .ordered_recording_source_generation(&self.peer, self.collection, token)
                    .await
                    .map_err(RecordingError::Source)?;
            }
            self.bind_generation(&mut state).await?;
            if state.pending.is_none() {
                let delivery = self
                    .central
                    .poll_ordered_recording(&self.peer, self.collection, token)
                    .await
                    .map_err(RecordingError::Source)?;
                match (&delivery.delivery_id, &delivery.poll) {
                    (None, Poll::Empty) => return Ok(RecordingClose::PendingDrain),
                    (None, Poll::Closed) => return Err(RecordingError::MissingTerminal),
                    (Some(_), Poll::Record(_) | Poll::Terminal(_)) => {
                        state.pending = Some(delivery)
                    }
                    _ => return Err(RecordingError::InvalidDelivery),
                }
            }
            self.commit_pending(&mut state, token).await?;
            return Ok(RecordingClose::Progress);
        }
        if cleanup != OrderedCleanupOutcome::Released {
            return Ok(RecordingClose::PendingDrain);
        }
        if !state.sealed {
            let journal = Arc::clone(&self.journal);
            let producer = self.producer.clone();
            tokio::task::spawn_blocking(move || journal.seal_ordered(&producer))
                .await
                .map_err(|_| RecordingError::WorkerFailed)?
                .map_err(RecordingError::Storage)?;
            state.sealed = true;
        }
        self.central
            .acknowledge_ordered_recording_batch(&self.peer, self.collection, token)
            .await
            .map_err(RecordingError::Source)?;
        state.group_acked = true;
        self.finish_cleanup(&mut state).await
    }

    async fn finish_cleanup(
        &self,
        state: &mut OwnerState,
    ) -> Result<RecordingClose, RecordingError> {
        if !state.group_forgotten {
            self.central
                .forget_ordered_batch(&self.peer, self.collection)
                .await
                .map_err(RecordingError::Source)?;
            state.group_forgotten = true;
        }
        if let Some(lease) = state.connection_lease.as_ref() {
            if let Err(error) = self
                .central
                .release_connection_lease(&self.peer, lease, OpControl::budget_ms(5000))
                .await
            {
                state.lease_failure = Some(error.clone());
                return Err(RecordingError::Source(error));
            }
            state.connection_lease = None;
            state.lease_failure = None;
        }
        if state.marked {
            let journal = Arc::clone(&self.journal);
            let producer = self.producer.clone();
            let released = state.lease_acquired;
            tokio::task::spawn_blocking(move || {
                journal.settle_ordered_host_cleanup(&producer, released)
            })
            .await
            .map_err(|_| RecordingError::WorkerFailed)?
            .map_err(RecordingError::Storage)?;
            state.host_cleanup_settled = true;
        }
        crate::continuation_journal::release_ordered_supervisor(&self.journal, &self.producer)
            .map_err(RecordingError::Storage)?;
        *self
            .recovery_capability
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        state.finished = true;
        Ok(RecordingClose::Finished)
    }
}

impl<B: RadioBoundary> crate::continuation_journal::OrderedSupervisor for OrderedRecording<B> {
    fn shutdown(
        self: Arc<Self>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<RecordingDriverStatus, RecordingError>> + Send>,
    > {
        Box::pin(async move { OrderedRecording::shutdown(&self).await })
    }

    fn host_cleanup(
        self: Arc<Self>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> {
        Box::pin(async move {
            let state = self.state.lock().await;
            if let Some(error) = &state.lease_failure {
                json!({"state":"release-failed","failures":[{"resourceKind":"ordered-recording-connection-lease","error":normalized_cleanup_error(error)}]})
            } else if state.connection_lease.is_some()
                || (state.lease_acquired && !state.host_cleanup_settled)
            {
                json!({"state":"pending"})
            } else if state.lease_acquired {
                json!({"state":"released","failures":[]})
            } else {
                json!({"state":"not-applicable"})
            }
        })
    }
}

fn normalized_cleanup_error(error: &DesktopError) -> Value {
    let platform = error.platform().map(|platform| {
        let metadata: serde_json::Map<String, Value> = platform.metadata.iter().map(|(key, value)| {
            let value = match value {
                crate::PlatformValue::Int(value) if value.unsigned_abs() > 9_007_199_254_740_991 => json!(value.to_string()),
                crate::PlatformValue::Int(value) => json!(value),
                crate::PlatformValue::Text(value) => json!(value),
                crate::PlatformValue::Bool(value) => json!(value),
            };
            (key.clone(), value)
        }).collect();
        let mut value = json!({"domain":platform.domain,"code":platform.code,"safeMessage":platform.message.as_deref().or(error.detail()).unwrap_or_default(),"metadata":metadata});
        if let Some(entries) = &crate::ordered_notification_groups::failure_from_desktop(error).platform_metadata_entries {
            value["metadataEntries"] = entries_value(entries);
        }
        value
    });
    let commit = match error.commit() {
        Some(ubm_core::contracts::CommitState::NotDispatched) => Some("not-dispatched"),
        Some(ubm_core::contracts::CommitState::Unknown) => Some("uncertain"),
        _ => None,
    };
    json!({"code":error.code_str(),"domain":error.domain().as_str(),"operation":error.operation(),"platform":platform,"retryability":error.retryability().as_str(),"commit":commit})
}

fn storage_diagnostic(error: &JournalError) -> Value {
    json!({"kind":error.kind,"detail":error.detail,"operation":error.operation,"sqliteExtendedCode":error.sqlite_extended_code,"sqliteCode":error.sqlite_code})
}
fn stop_failure(error: &RecordingError) -> Value {
    match error {
        RecordingError::Source(source) => {
            json!({"kind":"source","source":failure_v2(&crate::ordered_notification_groups::failure_from_desktop(source))})
        }
        RecordingError::Storage(storage) => {
            json!({"kind":"storage","storage":storage_diagnostic(storage)})
        }
        RecordingError::StorageCleanup(storage, source) => {
            json!({"kind":"storage-cleanup","storage":storage_diagnostic(storage),"source":failure_v2(&crate::ordered_notification_groups::failure_from_desktop(source))})
        }
        RecordingError::InvalidDelivery => json!({"kind":"invalid-delivery"}),
        RecordingError::InvalidIdentity => json!({"kind":"invalid-identity"}),
        RecordingError::WorkerFailed => json!({"kind":"worker-failed"}),
        RecordingError::MissingTerminal => json!({"kind":"missing-terminal"}),
        RecordingError::EntropyUnavailable => json!({"kind":"entropy-unavailable"}),
        RecordingError::ReceiptRetired => json!({"kind":"receipt-retired"}),
        RecordingError::RecordingNotAccepting => json!({"kind":"recording-not-accepting"}),
    }
}
/// A post-reservation worker failure retains the exact owner; it does not
/// assert whether source Open reached the platform.
pub fn owner_failure_receipt(
    recording_id: &str,
    collection_id: u64,
    error: &RecordingError,
) -> Value {
    json!({"kind":"owner-failed","recordingId":recording_id,"collectionId":collection_id.to_string(),"error":stop_failure(error)})
}

/// A prepare failure has no group/source; storage may already have created an
/// empty journal, so its effect is explicit and never guessed away.
pub fn prepare_failure_receipt(recording_id: &str, error: &RecordingError) -> Value {
    let journal_effect = if matches!(
        error,
        RecordingError::InvalidIdentity | RecordingError::EntropyUnavailable
    ) {
        "none"
    } else {
        "unknown"
    };
    json!({"version":2,"state":"prepare-failed","recordingId":recording_id,"sourceStarted":false,"journalEffect":journal_effect,"error":stop_failure(error)})
}

pub fn stop_receipt(result: Result<RecordingDriverStatus, RecordingError>) -> Value {
    match result {
        Ok(RecordingDriverStatus::Finished) => json!({"version":2,"state":"finished","error":null}),
        Ok(RecordingDriverStatus::WaitingCapacity) => {
            json!({"version":2,"state":"waiting-capacity","error":null})
        }
        Ok(RecordingDriverStatus::WaitingSource) => {
            json!({"version":2,"state":"waiting-source","error":null})
        }
        Ok(RecordingDriverStatus::WaitingDrain) => {
            json!({"version":2,"state":"waiting-drain","error":null})
        }
        Ok(RecordingDriverStatus::Failed(error)) | Err(error) => {
            json!({"version":2,"state":"failed","error":stop_failure(&error)})
        }
        Ok(
            RecordingDriverStatus::Idle
            | RecordingDriverStatus::Running
            | RecordingDriverStatus::Closing,
        ) => json!({"version":2,"state":"failed","error":{"kind":"worker-failed"}}),
    }
}

// The source ACK is deliberately invoked only after the durable transaction.
#[expect(
    clippy::result_large_err,
    reason = "The commit boundary must return the complete persisted recording error to keep acknowledgement recovery diagnosable."
)]
async fn commit_then_ack<F, Fut>(
    journal: &Arc<ContinuationJournal>,
    identity: &RecordingIdentity,
    delivery: &OrderedDelivery,
    ack: F,
) -> Result<(), RecordingError>
where
    F: FnOnce(u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), DesktopError>>,
{
    commit_then_ack_version(journal, identity, delivery, 2, ack).await
}

#[expect(
    clippy::result_large_err,
    reason = "The versioned commit boundary preserves the complete recording error for retry and receipt encoding."
)]
async fn commit_then_ack_version<F, Fut>(
    journal: &Arc<ContinuationJournal>,
    identity: &RecordingIdentity,
    delivery: &OrderedDelivery,
    version: u8,
    ack: F,
) -> Result<(), RecordingError>
where
    F: FnOnce(u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), DesktopError>>,
{
    let id = delivery
        .delivery_id
        .ok_or(RecordingError::InvalidDelivery)?;
    let record =
        encode_poll_version(&delivery.poll, version).ok_or(RecordingError::InvalidDelivery)?;
    if serde_json::to_vec(&record).map_or(true, |bytes| bytes.len() > 1_048_576) {
        return Err(RecordingError::InvalidDelivery);
    }
    let (kind, generation) = match &identity.source_generation {
        SourceGeneration::Counter(value) => ("counter", value.to_string()),
        SourceGeneration::Opaque(value) => ("opaque", value.clone()),
    };
    let collection = identity.collection_id.to_string();
    let metadata = json!({"kind":"ordered","version":version,"recordingId":identity.recording_id,"peerId":identity.peer_id,"collectionId":collection,"sourceGeneration":{"kind":kind,"value":generation},"deliveryId":id.to_string()});
    let journal = Arc::clone(journal);
    let producer = identity.producer_incarnation.clone();
    let delivery_id = id.to_string();
    tokio::task::spawn_blocking(move || {
        journal.append_ordered(
            &producer,
            &collection,
            kind,
            &generation,
            &delivery_id,
            &metadata,
            &record,
        )
    })
    .await
    .map_err(|_| RecordingError::WorkerFailed)?
    .map_err(RecordingError::Storage)?;
    ack(id).await.map_err(RecordingError::Source)
}

fn membership(value: Membership) -> Value {
    json!({"member":value.member.to_string(),"epoch":value.epoch.to_string()})
}
fn scope(value: LossScope) -> Value {
    match value {
        LossScope::Member(id) => json!({"kind":"member","member":id.to_string()}),
        LossScope::Group => json!({"kind":"group"}),
        LossScope::Unknown => json!({"kind":"unknown"}),
    }
}
fn unit(value: LossUnit) -> &'static str {
    match value {
        LossUnit::Values => "values",
        LossUnit::SourceRecords => "source-records",
    }
}
fn loss(value: RejectedLoss) -> Value {
    json!({"scope":scope(value.scope),"unit":unit(value.unit),"count":value.count.map(|v|v.to_string()),"bytes":value.bytes.map(|v|v.to_string())})
}
fn failure(value: &Failure) -> Value {
    let metadata: serde_json::Map<String, Value> = value
        .platform_metadata
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                match value {
                    FailureValue::Int(v) => json!({"kind":"int","value":v.to_string()}),
                    FailureValue::Text(v) => json!({"kind":"text","value":v}),
                    FailureValue::Bool(v) => json!({"kind":"bool","value":v}),
                },
            )
        })
        .collect();
    json!({"code":value.code,"domain":value.domain,"operation":value.operation,"detail":value.detail,"platformDomain":value.platform_domain,"platformCode":value.platform_code,"platformMessage":value.platform_message,"platformMetadata":metadata})
}
#[cfg(test)]
fn try_legacy_failure(error: &DesktopError) -> Option<Value> {
    let value = crate::ordered_notification_groups::failure_from_desktop(error);
    failure_is_readable(&value, 1).then(|| failure(&value))
}
fn try_v2_failure(error: &DesktopError) -> Option<Value> {
    let value = crate::ordered_notification_groups::failure_from_desktop(error);
    failure_is_readable(&value, 2).then(|| failure_v2(&value))
}
pub(crate) fn failure_v2(value: &Failure) -> Value {
    let mut result = failure(value);
    if let Some(entries) = &value.platform_metadata_entries {
        result["platformMetadataEntries"] = entries_value(entries);
    }
    if !value.platform_cleanup.is_empty() {
        result["platformCleanup"] = Value::Array(
            value
                .platform_cleanup
                .iter()
                .map(platform_detail_v2)
                .collect(),
        );
    }
    if !value.platform_cleanup_failures.is_empty() {
        result["platformCleanupFailures"] = Value::Array(
            value
                .platform_cleanup_failures
                .iter()
                .map(failure_v2)
                .collect(),
        );
    }
    result
}

fn entries_value(entries: &[ubm_core::ordered_notifications::FailureMetadataEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|entry| {
                json!({"key":entry.key,"value":match &entry.value {
                    FailureValue::Int(value) => json!({"kind":"int","value":value.to_string()}),
                    FailureValue::Text(value) => json!({"kind":"text","value":value}),
                    FailureValue::Bool(value) => json!({"kind":"bool","value":value}),
                }})
            })
            .collect(),
    )
}

fn platform_detail_v2(detail: &ubm_core::ordered_notifications::FailurePlatformDetail) -> Value {
    let metadata: serde_json::Map<String, Value> = detail
        .metadata
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                match value {
                    FailureValue::Int(value) => json!({"kind":"int","value":value.to_string()}),
                    FailureValue::Text(value) => json!({"kind":"text","value":value}),
                    FailureValue::Bool(value) => json!({"kind":"bool","value":value}),
                },
            )
        })
        .collect();
    let mut result = json!({"domain":detail.domain,"code":detail.code,"message":detail.message,"metadata":metadata});
    if let Some(entries) = &detail.metadata_entries {
        result["metadataEntries"] = entries_value(entries);
    }
    if !detail.cleanup.is_empty() {
        result["cleanup"] = Value::Array(detail.cleanup.iter().map(platform_detail_v2).collect());
    }
    if !detail.cleanup_failures.is_empty() {
        result["cleanupFailures"] =
            Value::Array(detail.cleanup_failures.iter().map(failure_v2).collect());
    }
    result
}
fn reason_v2(value: &EndReason) -> Value {
    match value {
        EndReason::SetupFailed(v) => json!({"kind":"setup-failed","failure":failure_v2(v)}),
        EndReason::SourceFailed(v) => json!({"kind":"source-failed","failure":failure_v2(v)}),
        EndReason::HostReset(v) => json!({"kind":"host-reset","failure":failure_v2(v)}),
        _ => reason(value),
    }
}
fn reason(value: &EndReason) -> Value {
    match value {
        EndReason::SetupFailed(v) => json!({"kind":"setup-failed","failure":failure(v)}),
        EndReason::SourceFailed(v) => json!({"kind":"source-failed","failure":failure(v)}),
        EndReason::HostReset(v) => json!({"kind":"host-reset","failure":failure(v)}),
        other => {
            json!({"kind":match other { EndReason::Closed=>"closed",EndReason::Invalidated=>"invalidated",EndReason::ServiceChanged=>"service-changed",EndReason::ConnectionLost=>"connection-lost",EndReason::OwnerReleased=>"owner-released",EndReason::CapacityExceeded=>"capacity-exceeded",EndReason::OrdinalInvalid=>"ordinal-invalid",EndReason::SourceOverflow=>"source-overflow",EndReason::SourceOrdinalExhausted=>"source-ordinal-exhausted",_=>unreachable!() }})
        }
    }
}
fn rejection(value: SourceRejection) -> Value {
    match value {
        SourceRejection::Value { bytes } => json!({"kind":"value","bytes":bytes.to_string()}),
        SourceRejection::Loss { skipped_records } => {
            json!({"kind":"loss","skippedRecords":skipped_records.to_string()})
        }
        SourceRejection::Fence { membership: m } => {
            json!({"kind":"fence","membership":membership(m)})
        }
        SourceRejection::Barrier { id } => json!({"kind":"barrier","id":id.to_string()}),
        SourceRejection::Marker { marker_id } => {
            json!({"kind":"marker","markerId":marker_id.to_string()})
        }
        SourceRejection::OrdinalExhausted { bytes } => {
            json!({"kind":"ordinal-exhausted","bytes":bytes.to_string()})
        }
    }
}
fn terminal(value: &Terminal) -> Value {
    json!({"kind":"terminal","reason":reason(&value.reason),"tailUncertain":value.tail_uncertain,"rejectedValues":value.rejected_values.map(|v|v.to_string()),"rejectedLoss":value.rejected_loss.map(loss),"sourceRejection":value.source_rejection.map(rejection),"sourceFailureMember":value.source_failure_member.map(membership),"secondaryFailure":value.secondary_failure.as_ref().map(failure),"additionalFailureCount":value.additional_failure_count.to_string(),"failureSummaryVersion":1, "failureCountOverflowed":value.failure_count_overflowed,"failureObservationsOmitted":value.failure_observations_omitted})
}

// The current recording reader rejects these map-only keys. Refuse before
// persistence and source ACK instead of committing a row it cannot read.
pub(crate) fn failure_is_readable(value: &Failure, version: u8) -> bool {
    if version == 1
        && (value.platform_metadata_entries.is_some()
            || !value.platform_cleanup.is_empty()
            || !value.platform_cleanup_failures.is_empty())
    {
        return false;
    }
    if !failure_contents_valid(value, 0) {
        return false;
    }
    serde_json::to_vec(&if version == 2 {
        failure_v2(value)
    } else {
        failure(value)
    })
    .is_ok_and(|bytes| bytes.len() <= 1_048_576)
}

fn entries_valid(
    entries: &Option<Vec<ubm_core::ordered_notifications::FailureMetadataEntry>>,
) -> bool {
    entries.as_ref().is_none_or(|entries| {
        entries.len() <= 1024
            && entries.iter().all(|entry| {
                entry.key.len() <= 65536
                    && !matches!(&entry.value, FailureValue::Text(text) if text.len() > 65536)
            })
    })
}

fn failure_contents_valid(value: &Failure, depth: usize) -> bool {
    depth <= 32
        && [&value.code, &value.domain, &value.operation]
            .into_iter()
            .all(|text| !text.is_empty() && text.len() <= 65536)
        && [
            &value.detail,
            &value.platform_domain,
            &value.platform_code,
            &value.platform_message,
        ]
        .into_iter()
        .flatten()
        .all(|text| text.len() <= 65536)
        && metadata_values_valid(&value.platform_metadata)
        && entries_valid(&value.platform_metadata_entries)
        && metadata_readable(&value.platform_metadata, &value.platform_metadata_entries)
        && value
            .platform_cleanup
            .iter()
            .all(|detail| platform_detail_valid(detail, depth + 1))
        && value
            .platform_cleanup_failures
            .iter()
            .all(|failure| failure_contents_valid(failure, depth + 1))
}

fn platform_detail_valid(
    value: &ubm_core::ordered_notifications::FailurePlatformDetail,
    depth: usize,
) -> bool {
    depth <= 32
        && value.domain.len() <= 65536
        && value.code.len() <= 65536
        && value
            .message
            .as_ref()
            .is_none_or(|text| text.len() <= 65536)
        && metadata_values_valid(&value.metadata)
        && entries_valid(&value.metadata_entries)
        && metadata_readable(&value.metadata, &value.metadata_entries)
        && value
            .cleanup
            .iter()
            .all(|detail| platform_detail_valid(detail, depth + 1))
        && value
            .cleanup_failures
            .iter()
            .all(|failure| failure_contents_valid(failure, depth + 1))
}

fn metadata_values_valid(metadata: &std::collections::BTreeMap<String, FailureValue>) -> bool {
    metadata.iter().all(|(key, value)| {
        key.len() <= 65536 && !matches!(value, FailureValue::Text(text) if text.len() > 65536)
    })
}

fn metadata_readable(
    metadata: &std::collections::BTreeMap<String, FailureValue>,
    entries: &Option<Vec<ubm_core::ordered_notifications::FailureMetadataEntry>>,
) -> bool {
    metadata.iter().all(|(key, value)| {
        !matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
            || entries.as_ref().is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| &entry.key == key && &entry.value == value)
            })
    })
}

fn reason_is_readable(value: &EndReason, version: u8) -> bool {
    match value {
        EndReason::SetupFailed(failure)
        | EndReason::SourceFailed(failure)
        | EndReason::HostReset(failure) => failure_is_readable(failure, version),
        _ => true,
    }
}

fn encode_poll_version(poll: &Poll, version: u8) -> Option<Value> {
    if version != 1 && version != 2 {
        return None;
    }
    match poll {
        Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            ..
        }) if !failure_is_readable(failure, version) => return None,
        Poll::Terminal(terminal)
            if !reason_is_readable(&terminal.reason, version)
                || terminal
                    .secondary_failure
                    .as_ref()
                    .is_some_and(|failure| !failure_is_readable(failure, version)) =>
        {
            return None;
        }
        _ => {}
    }
    match poll {
        Poll::Record(Record::Value {
            ordinal,
            member,
            membership: m,
            value,
            delivery,
            provenance,
        }) => Some(
            json!({"kind":"value","ordinal":ordinal.to_string(),"member":member.to_string(),"membership":membership(*m),"bytes":value,"delivery":match delivery { ObservedDelivery::Notification=>"notification",ObservedDelivery::Indication=>"indication",ObservedDelivery::Unknown=>"unknown" },"provenance":provenance.as_str()}),
        ),
        Poll::Record(Record::Marker { marker_id, ordinal }) => Some(
            json!({"kind":"marker","markerId":marker_id.to_string(),"ordinal":ordinal.to_string()}),
        ),
        Poll::Record(Record::Loss {
            scope: s,
            unit: u,
            count,
            bytes,
        }) => Some(
            json!({"kind":"loss","scope":scope(*s),"unit":unit(*u),"count":count.map(|v|v.to_string()),"bytes":bytes.map(|v|v.to_string())}),
        ),
        Poll::Record(Record::FailureObservation {
            failure: observed_failure,
            source_rejection,
            source_failure_member,
            rejected_loss,
            tail_uncertain,
        }) => {
            if version != 2
                || (observed_failure.is_none()
                    && source_rejection.is_none()
                    && source_failure_member.is_none()
                    && rejected_loss.is_none()
                    && !tail_uncertain)
            {
                return None;
            }
            Some(
                json!({"kind":"failure-observation","failure":observed_failure.as_ref().map(failure_v2),"sourceRejection":source_rejection.map(rejection),"sourceFailureMember":source_failure_member.map(membership),"rejectedLoss":rejected_loss.map(loss),"tailUncertain":tail_uncertain}),
            )
        }
        Poll::Record(Record::FailureObservationsOmitted { count }) => {
            if version != 2 || *count == Some(0) {
                return None;
            }
            Some(
                json!({"kind":"failure-observations-omitted","count":count.map(|value|value.to_string())}),
            )
        }
        Poll::Terminal(value) if version == 1 => Some(terminal(value)),
        Poll::Terminal(value) => {
            if value.rejected_values.is_some()
                || value.rejected_loss.is_some()
                || value.source_rejection.is_some()
                || value.source_failure_member.is_some()
                || value.secondary_failure.is_some()
                || value.additional_failure_count != 0
                || value.failure_count_overflowed
                || value.failure_observations_omitted
            {
                return None;
            }
            Some(
                json!({"kind":"terminal","reason":reason_v2(&value.reason),"tailUncertain":value.tail_uncertain}),
            )
        }
        Poll::Empty | Poll::Closed => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn oversized_failure_identity_is_refused() {
        let failure = Failure {
            code: "x".repeat(65537),
            domain: "gatt".into(),
            operation: "read".into(),
            detail: None,
            platform_domain: None,
            platform_code: None,
            platform_message: None,
            platform_metadata: Default::default(),
            platform_metadata_entries: None,
            platform_cleanup: Vec::new(),
            platform_cleanup_failures: Vec::new(),
        };
        let poll = Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        });
        assert!(encode_poll_version(&poll, 2).is_none());
    }
    #[test]
    fn legacy_setup_encoding_refuses_unrepresentable_metadata() {
        let mut platform = crate::PlatformDetail::new("source", "failure");
        platform.metadata_entries = Some(Vec::new());
        let error = DesktopError::read_failed("read").with_platform(platform);
        assert!(try_legacy_failure(&error).is_none());
    }
    #[test]
    fn dangerous_map_value_absent_from_sequence_is_refused() {
        use crate::errors::{DesktopError, PlatformDetail, PlatformValue};
        let mut platform = PlatformDetail::new("native", "failed");
        platform.metadata.insert(
            "__proto__".into(),
            PlatformValue::Text("not in the sequence".into()),
        );
        platform.metadata_entries = Some(Vec::new());
        let failure = crate::ordered_notification_groups::failure_from_desktop(
            &DesktopError::read_failed("read").with_platform(platform),
        );
        let poll = Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        });
        assert!(encode_poll_version(&poll, 2).is_none());
    }
    #[test]
    fn oversized_cleanup_entries_are_refused_before_recording() {
        use crate::errors::{DesktopError, PlatformDetail, PlatformMetadataEntry, PlatformValue};
        let mut cleanup = PlatformDetail::new("cleanup", "failed");
        cleanup.metadata_entries = Some(vec![
            PlatformMetadataEntry {
                key: "x".into(),
                value: PlatformValue::Bool(true)
            };
            1025
        ]);
        let mut primary = PlatformDetail::new("native", "failed");
        primary.cleanup.push(cleanup);
        let failure = crate::ordered_notification_groups::failure_from_desktop(
            &DesktopError::read_failed("read").with_platform(primary),
        );
        let poll = ubm_core::ordered_notifications::Poll::Record(
            ubm_core::ordered_notifications::Record::FailureObservation {
                failure: Some(failure),
                source_rejection: None,
                source_failure_member: None,
                rejected_loss: None,
                tail_uncertain: false,
            },
        );
        assert!(super::encode_poll_version(&poll, 2).is_none());
    }
    #[test]
    fn ordered_failure_sequence_survives_desktop_conversion_and_v2_recording() {
        use crate::errors::{DesktopError, PlatformDetail, PlatformMetadataEntry, PlatformValue};
        let mut platform = PlatformDetail::new("native", "failure");
        platform.metadata_entries = Some(vec![
            PlatformMetadataEntry {
                key: "__proto__".into(),
                value: PlatformValue::Int(i64::MIN),
            },
            PlatformMetadataEntry {
                key: "__proto__".into(),
                value: PlatformValue::Int(i64::MAX),
            },
        ]);
        let error = DesktopError::read_failed("read").with_platform(platform);
        let failure = crate::ordered_notification_groups::failure_from_desktop(&error);
        let poll = ubm_core::ordered_notifications::Poll::Record(
            ubm_core::ordered_notifications::Record::FailureObservation {
                failure: Some(failure),
                source_rejection: None,
                source_failure_member: None,
                rejected_loss: None,
                tail_uncertain: false,
            },
        );
        let encoded = super::encode_poll_version(&poll, 2).unwrap();
        assert_eq!(
            encoded["failure"]["platformMetadataEntries"],
            serde_json::json!([
                {"key":"__proto__","value":{"kind":"int","value":i64::MIN.to_string()}},
                {"key":"__proto__","value":{"kind":"int","value":i64::MAX.to_string()}},
            ])
        );
    }
    use super::*;
    use ubm_core::central::NotificationProvenance;

    fn encode_poll(poll: &Poll) -> Option<Value> {
        encode_poll_version(poll, 1)
    }

    #[test]
    fn map_only_dangerous_names_are_refused_in_every_failure_position() {
        let make_failure = |key: &str| Failure {
            code: "platform.failure".into(),
            domain: "gatt".into(),
            operation: "read".into(),
            detail: None,
            platform_domain: None,
            platform_code: None,
            platform_message: None,
            platform_metadata: [(key.into(), FailureValue::Int(i64::MIN))].into(),
            platform_metadata_entries: None,
            platform_cleanup: Vec::new(),
            platform_cleanup_failures: Vec::new(),
        };
        for key in ["__proto__", "constructor", "prototype"] {
            let f = make_failure(key);
            let terminal = Terminal {
                reason: EndReason::SourceFailed(f.clone()),
                tail_uncertain: false,
                rejected_values: None,
                rejected_loss: None,
                source_rejection: None,
                source_failure_member: None,
                secondary_failure: None,
                additional_failure_count: 0,
                failure_count_overflowed: false,
                failure_observations_omitted: false,
            };
            assert!(encode_poll_version(&Poll::Terminal(terminal.clone()), 1).is_none());
            assert!(encode_poll_version(&Poll::Terminal(terminal.clone()), 2).is_none());
            let mut secondary = terminal;
            secondary.reason = EndReason::Closed;
            secondary.secondary_failure = Some(f.clone());
            assert!(encode_poll_version(&Poll::Terminal(secondary), 1).is_none());
            let observation = Poll::Record(Record::FailureObservation {
                failure: Some(f),
                source_rejection: None,
                source_failure_member: None,
                rejected_loss: None,
                tail_uncertain: false,
            });
            assert!(encode_poll_version(&observation, 2).is_none());
        }
        assert!(
            encode_poll_version(
                &Poll::Terminal(Terminal {
                    reason: EndReason::SourceFailed(make_failure("safe")),
                    tail_uncertain: false,
                    rejected_values: None,
                    rejected_loss: None,
                    source_rejection: None,
                    source_failure_member: None,
                    secondary_failure: None,
                    additional_failure_count: 0,
                    failure_count_overflowed: false,
                    failure_observations_omitted: false,
                }),
                1
            )
            .is_some()
        );
    }

    #[test]
    fn invalid_failure_observation_records_are_not_serialized() {
        let empty = Poll::Record(Record::FailureObservation {
            failure: None,
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        });
        assert!(encode_poll_version(&empty, 2).is_none());
        assert!(
            encode_poll_version(
                &Poll::Record(Record::FailureObservationsOmitted { count: Some(0) }),
                2
            )
            .is_none()
        );
        let unknown = Poll::Record(Record::FailureObservationsOmitted { count: None });
        assert_eq!(
            encode_poll_version(&unknown, 2).unwrap()["count"],
            Value::Null
        );
        assert!(encode_poll_version(&unknown, 1).is_none());
        assert!(encode_poll_version(&unknown, 3).is_none());
    }

    #[test]
    fn cleanup_error_preserves_typed_identity_and_safe_platform_metadata() {
        let error = DesktopError::new(
            ubm_core::contracts::BleErrorCode::ConnectionLost,
            ubm_core::contracts::BleErrorDomain::Connection,
            "connection.release",
        )
        .with_platform(
            crate::PlatformDetail::new("platform", "code")
                .with_message("safe")
                .with_metadata("large", crate::PlatformValue::Int(i64::MAX)),
        );
        let value = normalized_cleanup_error(&error);
        assert_eq!(value["code"], "connection.lost");
        assert_eq!(value["domain"], "connection");
        assert_eq!(value["operation"], "connection.release");
        assert_eq!(value["platform"]["safeMessage"], "safe");
        assert_eq!(value["platform"]["metadata"]["large"], i64::MAX.to_string());
        assert!(value.get("commit").is_some());
        assert!(value.get("retryability").is_some());
    }

    #[test]
    fn ordered_context_round_trips_exact_mapping_and_rejects_contract_drift() {
        let declaration = OrderedRecordingDeclaration {
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            database_generation: "database".into(),
            max_items: 8,
            max_bytes: 4096,
            max_controls: 4,
            members: vec![OrderedRecordingMember {
                member_id: u64::MAX,
                public_member_id: "heart".into(),
                selector: ubm_core::central::PathSelector {
                    service_uuid: "service".into(),
                    service_occurrence: Some(2),
                    characteristic_uuid: Some("characteristic".into()),
                    characteristic_occurrence: Some(u64::MAX),
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                },
                delivery: Some(DeliveryMode::Notification),
                preference: None,
            }],
        };
        let context = declaration.context().unwrap();
        let restored = OrderedRecordingDeclaration::from_context(&context).unwrap();
        assert_eq!(restored.context().unwrap(), context);
        assert_eq!(context["members"][0]["memberId"], u64::MAX.to_string());
        assert_eq!(
            context["members"][0]["selector"]["characteristicOccurrence"],
            u64::MAX.to_string()
        );
        let mut malformed = context.clone();
        malformed["members"][0]["unexpected"] = json!(true);
        assert!(OrderedRecordingDeclaration::from_context(&malformed).is_err());
        let mut duplicate = declaration.clone();
        duplicate.members.push(duplicate.members[0].clone());
        assert!(duplicate.context().is_err());
        let directory =
            std::env::temp_dir().join(format!("ubm-ordered-context-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let registry = JournalRegistry::default();
        registry.configure_directory(&directory).unwrap();
        let journal = registry
            .open(
                "context",
                &context,
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 8,
                },
            )
            .unwrap();
        drop(journal);
        let reopened = JournalRegistry::default();
        reopened.configure_directory(&directory).unwrap();
        assert_eq!(reopened.ordered_context("context").unwrap(), context);
    }

    #[test]
    fn prepare_and_owner_failure_receipts_do_not_invent_source_outcome() {
        let validation = prepare_failure_receipt("recording", &RecordingError::InvalidIdentity);
        assert_eq!(validation["sourceStarted"], false);
        assert_eq!(validation["journalEffect"], "none");
        let storage = RecordingError::Storage(JournalError {
            kind: "storage.io",
            detail: "io",
            operation: "open",
            sqlite_extended_code: None,
            sqlite_code: None,
        });
        let failed = prepare_failure_receipt("recording", &storage);
        assert_eq!(failed["sourceStarted"], false);
        assert_eq!(failed["journalEffect"], "unknown");
        assert_eq!(failed["error"]["storage"]["kind"], "storage.io");
        let owner = owner_failure_receipt("recording", u64::MAX, &RecordingError::WorkerFailed);
        assert_eq!(owner["collectionId"], u64::MAX.to_string());
        assert!(owner.get("sourceStarted").is_none());
    }

    #[test]
    fn stop_receipt_preserves_storage_and_cleanup_failure_identity() {
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        let storage = JournalError {
            kind: "storage.full",
            detail: "full",
            operation: "append",
            sqlite_extended_code: None,
            sqlite_code: None,
        };
        let mut platform = crate::PlatformDetail::new("source", "failed");
        platform
            .cleanup
            .push(crate::PlatformDetail::new("cleanup", "refused"));
        let source = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Cleanup,
            "source.fence",
        )
        .with_platform(platform);
        let receipt = stop_receipt(Ok(RecordingDriverStatus::Failed(
            RecordingError::StorageCleanup(storage, source),
        )));
        assert_eq!(receipt["version"], 2);
        assert_eq!(receipt["state"], "failed");
        assert_eq!(receipt["error"]["kind"], "storage-cleanup");
        assert_eq!(receipt["error"]["storage"]["kind"], "storage.full");
        assert_eq!(receipt["error"]["source"]["operation"], "source.fence");
        assert_eq!(
            receipt["error"]["source"]["platformCleanup"][0]["code"],
            "refused"
        );
    }

    #[tokio::test]
    async fn lost_ack_retries_committed_delivery_and_full_refuses_before_ack() {
        use crate::continuation_journal::JournalQuota;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        let test = "ordered_recording::tests::lost_ack_retries_committed_delivery_and_full_refuses_before_ack";
        let Some(directory) = crate::central::recording_fixture::isolated_fixture_process(test)
        else {
            return;
        };
        let path = directory.join("recording.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "recording",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 1,
                },
            )
            .unwrap(),
        );
        let identity = RecordingIdentity {
            recording_id: "recording".into(),
            producer_incarnation: "incarnation".into(),
            peer_id: "peer".into(),
            collection_id: 4,
            source_generation: SourceGeneration::Counter(u64::MAX),
        };
        journal.begin_ordered("incarnation", "4").unwrap();
        journal
            .bind_ordered_generation("incarnation", "counter", &u64::MAX.to_string())
            .unwrap();
        let delivery = OrderedDelivery {
            delivery_id: Some(1),
            poll: Poll::Record(Record::Loss {
                scope: LossScope::Group,
                unit: LossUnit::Values,
                count: Some(2),
                bytes: None,
            }),
        };
        let calls_storage = AtomicUsize::new(0);
        let calls = &calls_storage;
        let unsafe_delivery = OrderedDelivery {
            delivery_id: Some(1),
            poll: Poll::Terminal(Terminal {
                reason: EndReason::SourceFailed(Failure {
                    code: "platform.failure".into(),
                    domain: "gatt".into(),
                    operation: "read".into(),
                    detail: None,
                    platform_domain: None,
                    platform_code: None,
                    platform_message: None,
                    platform_metadata: [(
                        "__proto__".into(),
                        FailureValue::Text("retained".into()),
                    )]
                    .into(),
                    platform_metadata_entries: None,
                    platform_cleanup: Vec::new(),
                    platform_cleanup_failures: Vec::new(),
                }),
                tail_uncertain: false,
                rejected_values: None,
                rejected_loss: None,
                source_rejection: None,
                source_failure_member: None,
                secondary_failure: None,
                additional_failure_count: 0,
                failure_count_overflowed: false,
                failure_observations_omitted: false,
            }),
        };
        let rejected = commit_then_ack(&journal, &identity, &unsafe_delivery, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(rejected, Err(RecordingError::InvalidDelivery)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(journal.status().unwrap()["records"], 0);

        let mut sequence_platform = crate::PlatformDetail::new("source", "failed");
        sequence_platform.metadata_entries = Some(Vec::new());
        let sequence_failure = crate::ordered_notification_groups::failure_from_desktop(
            &DesktopError::read_failed("read").with_platform(sequence_platform),
        );
        let mut sequence_delivery = unsafe_delivery.clone();
        if let Poll::Terminal(terminal) = &mut sequence_delivery.poll {
            terminal.reason = EndReason::SourceFailed(sequence_failure);
        }
        let rejected =
            commit_then_ack_version(&journal, &identity, &sequence_delivery, 1, |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(matches!(rejected, Err(RecordingError::InvalidDelivery)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(journal.status().unwrap()["records"], 0);

        // A legacy reader cannot represent cleanup details or observations.
        let mut platform = crate::PlatformDetail::new("source", "failed");
        platform
            .cleanup
            .push(crate::PlatformDetail::new("cleanup", "refused"));
        let cause = crate::ordered_notification_groups::failure_from_desktop(
            &DesktopError::read_failed("read").with_platform(platform),
        );
        let cleanup_delivery = OrderedDelivery {
            delivery_id: Some(1),
            poll: Poll::Record(Record::FailureObservation {
                failure: Some(cause),
                source_rejection: None,
                source_failure_member: None,
                rejected_loss: None,
                tail_uncertain: false,
            }),
        };
        let rejected =
            commit_then_ack_version(&journal, &identity, &cleanup_delivery, 1, |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(matches!(rejected, Err(RecordingError::InvalidDelivery)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(journal.status().unwrap()["records"], 0);

        let oversized = OrderedDelivery {
            delivery_id: Some(1),
            poll: Poll::Record(Record::Value {
                ordinal: 1,
                member: 1,
                membership: Membership {
                    member: 1,
                    epoch: 1,
                },
                value: vec![255; 65536],
                delivery: ObservedDelivery::Unknown,
                provenance: NotificationProvenance::Unknown,
            }),
        };
        let rejected = commit_then_ack(&journal, &identity, &oversized, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(rejected, Err(RecordingError::Storage(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(journal.status().unwrap()["records"], 0);
        let first = commit_then_ack(&journal, &identity, &delivery, |id| async move {
            assert_eq!(id, 1);
            calls.fetch_add(1, Ordering::SeqCst);
            Err(DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Gatt,
                "test-ack",
            ))
        })
        .await;
        assert!(matches!(first, Err(RecordingError::Source(_))));
        assert_eq!(journal.status().unwrap()["records"], 1);
        commit_then_ack(&journal, &identity, &delivery, |id| async move {
            assert_eq!(id, 1);
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(journal.status().unwrap()["records"], 1);
        let next = OrderedDelivery {
            delivery_id: Some(2),
            poll: delivery.poll.clone(),
        };
        let refused = commit_then_ack(&journal, &identity, &next, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(
            matches!(refused, Err(RecordingError::Storage(error)) if error.kind == "storage.full")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(journal.status().unwrap()["records"], 1);
        drop(journal);
        crate::central::recording_fixture::complete_fixture_process(&directory);
    }

    #[tokio::test]
    async fn interleaved_value_marker_loss_and_terminal_replay_in_order() {
        use crate::continuation_journal::JournalQuota;
        let test =
            "ordered_recording::tests::interleaved_value_marker_loss_and_terminal_replay_in_order";
        let Some(directory) = crate::central::recording_fixture::isolated_fixture_process(test)
        else {
            return;
        };
        let path = directory.join("recording.sqlite");
        let journal = Arc::new(
            ContinuationJournal::open(
                &path,
                "fifo",
                &json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 10,
                },
            )
            .unwrap(),
        );
        let identity = RecordingIdentity {
            recording_id: "fifo".into(),
            producer_incarnation: "producer".into(),
            peer_id: "peer".into(),
            collection_id: 1,
            source_generation: SourceGeneration::Counter(3),
        };
        journal.begin_ordered("producer", "1").unwrap();
        journal
            .bind_ordered_generation("producer", "counter", "3")
            .unwrap();
        let mut native = crate::PlatformDetail::new("source", "failed");
        native
            .cleanup
            .push(crate::PlatformDetail::new("cleanup", "refused"));
        let failure = crate::ordered_notification_groups::failure_from_desktop(
            &DesktopError::read_failed("seal").with_platform(native),
        );
        let records = vec![
            Poll::Record(Record::Value {
                ordinal: u64::MAX - 1,
                member: 2,
                membership: Membership {
                    member: 7,
                    epoch: 8,
                },
                value: vec![0, 255],
                delivery: ObservedDelivery::Notification,
                provenance: NotificationProvenance::Notification,
            }),
            Poll::Record(Record::Marker {
                marker_id: u64::MAX,
                ordinal: u64::MAX,
            }),
            Poll::Record(Record::Loss {
                scope: LossScope::Unknown,
                unit: LossUnit::SourceRecords,
                count: None,
                bytes: None,
            }),
            Poll::Record(Record::FailureObservation {
                failure: Some(failure.clone()),
                source_rejection: Some(SourceRejection::Value { bytes: u64::MAX }),
                source_failure_member: Some(Membership {
                    member: 7,
                    epoch: 8,
                }),
                rejected_loss: Some(RejectedLoss {
                    scope: LossScope::Group,
                    unit: LossUnit::SourceRecords,
                    count: None,
                    bytes: Some(4),
                }),
                tail_uncertain: true,
            }),
            Poll::Record(Record::FailureObservation {
                failure: Some(failure),
                source_rejection: Some(SourceRejection::Value { bytes: u64::MAX }),
                source_failure_member: Some(Membership {
                    member: 7,
                    epoch: 8,
                }),
                rejected_loss: Some(RejectedLoss {
                    scope: LossScope::Group,
                    unit: LossUnit::SourceRecords,
                    count: None,
                    bytes: Some(4),
                }),
                tail_uncertain: true,
            }),
            Poll::Record(Record::FailureObservationsOmitted { count: Some(2) }),
            Poll::Terminal(Terminal {
                reason: EndReason::Closed,
                tail_uncertain: true,
                rejected_values: None,
                rejected_loss: None,
                source_rejection: None,
                source_failure_member: None,
                secondary_failure: None,
                additional_failure_count: 0,
                failure_count_overflowed: false,
                failure_observations_omitted: false,
            }),
        ];
        let blocked = OrderedDelivery {
            delivery_id: Some(1),
            poll: records[3].clone(),
        };
        let acknowledged = AtomicBool::new(false);
        let result = commit_then_ack_version(&journal, &identity, &blocked, 1, |_| async {
            acknowledged.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(RecordingError::InvalidDelivery)));
        assert!(!acknowledged.load(Ordering::SeqCst));
        assert_eq!(journal.status().unwrap()["records"], 0);
        for (index, poll) in records.into_iter().enumerate() {
            let delivery = OrderedDelivery {
                delivery_id: Some(index as u64 + 1),
                poll,
            };
            commit_then_ack_version(&journal, &identity, &delivery, 2, |_| async { Ok(()) })
                .await
                .unwrap();
        }
        let prepared = journal.prepare(10, 65536).unwrap();
        let entries = prepared["records"].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry["record"]["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "value",
                "marker",
                "loss",
                "failure-observation",
                "failure-observation",
                "failure-observations-omitted",
                "terminal"
            ]
        );
        assert_eq!(entries[0]["record"]["membership"]["epoch"], "8");
        assert_eq!(entries[0]["record"]["bytes"], json!([0, 255]));
        assert_eq!(entries[1]["record"]["markerId"], u64::MAX.to_string());
        assert!(entries[2]["record"]["count"].is_null());
        assert!(
            entries
                .iter()
                .all(|entry| entry["metadata"]["version"] == 2),
            "one producer must use one format version throughout a batch"
        );
        assert_eq!(entries[3]["metadata"]["version"], 2);
        assert_eq!(entries[4]["metadata"]["version"], 2);
        assert_ne!(
            entries[3]["metadata"]["deliveryId"],
            entries[4]["metadata"]["deliveryId"]
        );
        assert_eq!(entries[3]["record"], entries[4]["record"]);
        assert_eq!(
            entries[3]["record"]["sourceRejection"]["bytes"],
            u64::MAX.to_string()
        );
        assert_eq!(
            entries[3]["record"]["failure"]["platformCleanup"][0]["code"],
            "refused"
        );
        assert_eq!(entries[3]["record"]["sourceFailureMember"]["epoch"], "8");
        assert_eq!(entries[3]["record"]["rejectedLoss"]["bytes"], "4");
        assert_eq!(entries[5]["metadata"]["version"], 2);
        assert_eq!(entries[5]["record"]["count"], "2");
        assert_eq!(entries[6]["record"]["tailUncertain"], true);
        assert_eq!(
            entries[6]["record"],
            json!({"kind":"terminal","reason":{"kind":"closed"},"tailUncertain":true})
        );
        drop(journal);
        crate::central::recording_fixture::complete_fixture_process(&directory);
    }

    #[test]
    fn encodes_full_width_value_and_unknown_loss_without_inference() {
        let value = Poll::Record(Record::Value {
            ordinal: u64::MAX,
            member: 7,
            membership: Membership {
                member: 8,
                epoch: u64::MAX,
            },
            value: vec![0, 255],
            delivery: ObservedDelivery::Unknown,
            provenance: NotificationProvenance::ReadOrNotification,
        });
        let encoded = encode_poll(&value).unwrap();
        assert_eq!(encoded["ordinal"], u64::MAX.to_string());
        assert_eq!(encoded["membership"]["epoch"], u64::MAX.to_string());
        assert_eq!(encoded["bytes"], json!([0, 255]));
        let marker = encode_poll(&Poll::Record(Record::Marker {
            marker_id: u64::MAX,
            ordinal: u64::MAX,
        }))
        .unwrap();
        assert_eq!(marker["markerId"], u64::MAX.to_string());
        assert_eq!(marker["ordinal"], u64::MAX.to_string());
        let loss = encode_poll(&Poll::Record(Record::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::SourceRecords,
            count: None,
            bytes: None,
        }))
        .unwrap();
        assert_eq!(loss["scope"]["kind"], "unknown");
        assert!(loss["count"].is_null());
        assert!(loss["bytes"].is_null());
    }

    #[test]
    fn encodes_terminal_rejection_and_failure_evidence() {
        let terminal = Terminal {
            reason: EndReason::SourceOverflow,
            tail_uncertain: true,
            rejected_values: Some(3),
            rejected_loss: Some(RejectedLoss {
                scope: LossScope::Group,
                unit: LossUnit::Values,
                count: None,
                bytes: Some(42),
            }),
            source_rejection: Some(SourceRejection::Fence {
                membership: Membership {
                    member: 4,
                    epoch: 5,
                },
            }),
            source_failure_member: None,
            secondary_failure: None,
            additional_failure_count: u64::MAX,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        };
        assert!(encode_poll_version(&Poll::Terminal(terminal.clone()), 2).is_none());
        let encoded = encode_poll(&Poll::Terminal(terminal)).unwrap();
        assert_eq!(encoded["reason"]["kind"], "source-overflow");
        assert_eq!(encoded["sourceRejection"]["membership"]["epoch"], "5");
        assert!(encoded["rejectedLoss"]["count"].is_null());
        assert_eq!(encoded["additionalFailureCount"], u64::MAX.to_string());
    }
}
