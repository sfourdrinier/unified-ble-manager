// vendor/btleplug/src/common/util_source_groups.rs
//! Bounded peer groups serialized with native notification publication.

use super::{FaultState, NotificationFaults};

/// An opaque identity owned by one source connection generation.
#[derive(Clone, Debug)]
pub struct SourceGroupHandle {
    owner: std::sync::Arc<()>,
    generation: u64,
    id: u64,
    writes: std::sync::Arc<std::sync::Mutex<SourceWrites>>,
}

impl SourceGroupHandle {
    /// Cancel one exact request before source admission. Once admission has
    /// begun, return the existing receipt without changing its outcome.
    pub fn cancel_unresolved_marker_write(
        &self,
        request: &SourceUnresolvedWriteRequest,
    ) -> Result<SourceWriteReceipt, SourceGroupError> {
        let mut writes = self.writes.lock().unwrap();
        if let Some((existing, receipt)) = &writes.transaction {
            if !existing.matches_unresolved(request) {
                return Err(SourceGroupError::TransactionConflict);
            }
            let mut status = receipt.0.lock().unwrap();
            if matches!(*status, SourceWriteStatus::Pending) {
                *status = SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled);
            }
            drop(status);
            return Ok(receipt.clone());
        }
        if !request.valid_size() {
            return Err(SourceGroupError::InvalidRequest);
        }
        if writes.next_transaction != Some(request.id) {
            return Err(SourceGroupError::TransactionConflict);
        }
        let receipt = SourceWriteReceipt(std::sync::Arc::new(std::sync::Mutex::new(
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled),
        )));
        writes.transaction = Some((
            SourceWriteIdentity::Unresolved(request.clone()),
            receipt.clone(),
        ));
        Ok(receipt)
    }
    pub(crate) fn reserve_marker_write(
        &self,
        request: SourceWriteRequest,
    ) -> Result<(SourceWriteReceipt, bool), SourceGroupError> {
        let mut writes = self.writes.lock().unwrap();
        if let Some((existing, receipt)) = &writes.transaction {
            return if existing.matches_resolved(&request) {
                Ok((receipt.clone(), false))
            } else {
                Err(SourceGroupError::TransactionConflict)
            };
        }
        if !request.valid_size() {
            return Err(SourceGroupError::InvalidRequest);
        }
        if writes.next_transaction != Some(request.id) {
            return Err(SourceGroupError::TransactionConflict);
        }
        let receipt = SourceWriteReceipt(std::sync::Arc::new(std::sync::Mutex::new(
            SourceWriteStatus::Pending,
        )));
        writes.transaction = Some((SourceWriteIdentity::Resolved(request), receipt.clone()));
        Ok((receipt, true))
    }

    pub(crate) fn settle_pending_not_admitted(
        &self,
        request: &SourceWriteRequest,
        error: SourceGroupError,
    ) -> Result<SourceWriteReceipt, SourceGroupError> {
        if error == SourceGroupError::SubmissionUncertain {
            return Err(error);
        }
        let writes = self.writes.lock().unwrap();
        let Some((existing, receipt)) = &writes.transaction else {
            return Err(SourceGroupError::TransactionConflict);
        };
        if !existing.matches_resolved(request) {
            return Err(SourceGroupError::TransactionConflict);
        }
        let mut status = receipt.0.lock().unwrap();
        if matches!(*status, SourceWriteStatus::Pending) {
            *status = SourceWriteStatus::NotAdmitted(error);
        }
        drop(status);
        Ok(receipt.clone())
    }
    pub fn record_marker_not_admitted(
        &self,
        request: SourceWriteRequest,
        error: SourceGroupError,
    ) -> Result<SourceWriteReceipt, SourceGroupError> {
        if error == SourceGroupError::SubmissionUncertain {
            return Err(error);
        }
        let mut writes = self.writes.lock().unwrap();
        if let Some((existing, receipt)) = &writes.transaction {
            return if existing.matches_resolved(&request) {
                Ok(receipt.clone())
            } else {
                Err(SourceGroupError::TransactionConflict)
            };
        }
        if !request.valid_size() {
            return Err(SourceGroupError::InvalidRequest);
        }
        if writes.next_transaction != Some(request.id) {
            return Err(SourceGroupError::TransactionConflict);
        }
        let receipt = SourceWriteReceipt(std::sync::Arc::new(std::sync::Mutex::new(
            SourceWriteStatus::NotAdmitted(error),
        )));
        writes.transaction = Some((SourceWriteIdentity::Resolved(request), receipt.clone()));
        Ok(receipt)
    }
    pub fn record_unresolved_marker_not_admitted(
        &self,
        request: SourceUnresolvedWriteRequest,
        error: SourceGroupError,
    ) -> Result<SourceWriteReceipt, SourceGroupError> {
        if error == SourceGroupError::SubmissionUncertain {
            return Err(error);
        }
        let mut writes = self.writes.lock().unwrap();
        if let Some((existing, receipt)) = &writes.transaction {
            return if existing.matches_unresolved(&request) {
                Ok(receipt.clone())
            } else {
                Err(SourceGroupError::TransactionConflict)
            };
        }
        if !request.valid_size() {
            return Err(SourceGroupError::InvalidRequest);
        }
        if writes.next_transaction != Some(request.id) {
            return Err(SourceGroupError::TransactionConflict);
        }
        let receipt = SourceWriteReceipt(std::sync::Arc::new(std::sync::Mutex::new(
            SourceWriteStatus::NotAdmitted(error),
        )));
        writes.transaction = Some((SourceWriteIdentity::Unresolved(request), receipt.clone()));
        Ok(receipt)
    }
    pub fn replay_unresolved_marker_write(
        &self,
        request: &SourceUnresolvedWriteRequest,
    ) -> Result<Option<SourceWriteReceipt>, SourceGroupError> {
        let writes = self.writes.lock().unwrap();
        match &writes.transaction {
            Some((existing, receipt)) if existing.matches_unresolved(request) => {
                Ok(Some(receipt.clone()))
            }
            Some(_) => Err(SourceGroupError::TransactionConflict),
            None => Ok(None),
        }
    }
    /// Recover an exact receipt after the native source owner has retired.
    pub fn replay_marker_write(
        &self,
        request: &SourceWriteRequest,
    ) -> Result<Option<SourceWriteReceipt>, SourceGroupError> {
        let writes = self.writes.lock().unwrap();
        match &writes.transaction {
            Some((existing, receipt)) if existing.matches_resolved(request) => {
                Ok(Some(receipt.clone()))
            }
            Some(_) => Err(SourceGroupError::TransactionConflict),
            None => Ok(None),
        }
    }

    pub fn forget_marker_write(&self, id: u64) -> Result<(), SourceGroupError> {
        let mut writes = self.writes.lock().unwrap();
        if writes
            .forgotten_transaction
            .is_some_and(|forgotten| id <= forgotten)
        {
            return Ok(());
        }
        let Some((request, receipt)) = &writes.transaction else {
            return Err(SourceGroupError::TransactionConflict);
        };
        if request.id() != id
            || matches!(
                receipt.status(),
                SourceWriteStatus::Pending
                    | SourceWriteStatus::Admitted { .. }
                    | SourceWriteStatus::Started { .. }
            )
        {
            return Err(SourceGroupError::TransactionConflict);
        }
        writes.next_transaction = id.checked_add(1);
        writes.forgotten_transaction = Some(id);
        writes.transaction = None;
        Ok(())
    }
}

/// Read-only consumer. Source operations are only exposed by `NotificationFaults`.
/// Polling never calls back into the source: the only nested lock order is
/// source state followed by ingress state, never the reverse.
#[derive(Clone)]
pub struct SourceGroupConsumer(crate::ubm_peer_notification_ingress::PeerNotificationIngress);

/// Owns the accepted fanout independently of the source group's lifetime.
#[must_use = "replay or drop the callback reservation"]
pub(crate) struct SourceValueReservation {
    reservations: Vec<crate::ubm_peer_notification_ingress::IngressReservation>,
    retained_fault_hold: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    terminal_evidence: Option<(
        std::sync::Arc<dyn crate::ubm_peer_notification_ingress::TerminalEvidencePort>,
        String,
    )>,
    ordinal_exhausted: bool,
    retained_bytes: usize,
}

/// Retains the exact generation cut even when subsequent queue admission fails.
/// The per-source owner quota remains charged until the cut has settled.
/// This captures an already observed error; it is not a pre-admitted native
/// operation or a preallocated graph-gate node.
#[must_use = "release or drop to settle the captured failure"]
pub(crate) struct OwnedPendingSourceFailure {
    source: std::sync::Arc<NotificationFaults>,
    generation: u64,
    error: crate::PlatformError,
    held: Option<SourceValueReservation>,
    reserved: bool,
    finished: bool,
}

impl OwnedPendingSourceFailure {
    /// Checks the later transfer budget; recipient storage was already
    /// allocated when the observed error was captured.
    pub(crate) fn try_reserve(&mut self, budget: usize) -> Result<bool, SourceGroupError> {
        if self.reserved {
            return Ok(true);
        }
        // The source validated the activation and captured the recipients at
        // observation. Later retirement or replacement cannot revoke that cut.
        let bytes = std::mem::size_of::<Self>()
            .saturating_add(
                crate::ubm_peer_notification_ingress::platform_error_heap_bytes(&self.error),
            )
            .saturating_add(
                self.held
                    .as_ref()
                    .map_or(0, SourceValueReservation::retained_bytes),
            );
        if bytes > budget {
            return Err(SourceGroupError::Capacity);
        }
        self.reserved = true;
        Ok(true)
    }

    pub(crate) fn release(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.reserved = false;
        self.finished = true;
        // Every group already owns its terminal and the pending cut prevents
        // new groups. Closing preserves those terminals without cloning the
        // original error again for each already-terminated group.
        match self.source.close_source_generation(self.generation) {
            Ok(()) | Err(SourceGroupError::StaleGeneration) => {}
            Err(error) => {
                log::error!("notification failure owner could not finish generation: {error:?}")
            }
        }
        if let Some(held) = self.held.take() {
            held.release();
        }
        let mut state = self.source.lock_source_state();
        state.pending_failure_owners -= 1;
    }
}

impl Drop for OwnedPendingSourceFailure {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Keeps the physical route alive until the admitted cut replays. If the
/// queue abandons it, Drop recovers the held fence and uncertain terminal.
pub(crate) struct SourceRouteRetirement {
    source: std::sync::Arc<NotificationFaults>,
    group: SourceGroupHandle,
    route: crate::ubm_peer_notification_ingress::NotificationRoute,
    membership: crate::ubm_peer_notification_ingress::Membership,
    held: Option<SourceValueReservation>,
}

impl SourceRouteRetirement {
    pub(crate) fn release(mut self) {
        self.source
            .complete_source_route_retirement(&self.group, self.route, self.membership);
        if let Some(held) = self.held.take() {
            held.release();
        }
    }
}

impl Drop for SourceRouteRetirement {
    fn drop(&mut self) {
        self.source
            .complete_source_route_retirement(&self.group, self.route, self.membership);
    }
}

impl SourceValueReservation {
    /// The desktop boundary takes this token only after it has committed the
    /// canonical terminal bytes. Until then Drop releases the preactivation
    /// reservation instead of letting a detached native generation leak it.
    pub(crate) fn take_terminal_evidence(
        &mut self,
    ) -> Option<(
        std::sync::Arc<dyn crate::ubm_peer_notification_ingress::TerminalEvidencePort>,
        String,
    )> {
        self.terminal_evidence.take()
    }
    pub(crate) fn ordinal_exhausted(&self) -> bool {
        self.ordinal_exhausted
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
    pub(crate) fn release(mut self) {
        for reservation in self.reservations.drain(..) {
            reservation.release();
        }
        if let Some(hold) = self.retained_fault_hold.take() {
            hold.store(false, std::sync::atomic::Ordering::Release);
        }
        if let Some((port, reservation)) = self.terminal_evidence.take() {
            let _ = port.release(&reservation);
        }
    }
}

impl Drop for SourceValueReservation {
    fn drop(&mut self) {
        for reservation in self.reservations.drain(..) {
            reservation.abandon_and_recover();
        }
        if let Some(hold) = self.retained_fault_hold.take() {
            hold.store(false, std::sync::atomic::Ordering::Release);
        }
        if let Some((port, reservation)) = self.terminal_evidence.take() {
            let _ = port.release(&reservation);
        }
    }
}

impl SourceGroupConsumer {
    pub fn wake(&self) {
        self.0.wake();
    }
    pub fn poll(&self) -> crate::ubm_peer_notification_ingress::Poll {
        self.0.poll()
    }

    pub async fn recv(&self) -> Option<crate::ubm_peer_notification_ingress::IngressRecord> {
        self.0.recv().await
    }

    /// Retain the exact FIFO head until downstream ownership is established.
    pub async fn recv_transfer(
        &self,
    ) -> Option<crate::ubm_peer_notification_ingress::IngressTransfer> {
        self.0.recv_transfer().await
    }

    pub fn ack_transfer(
        &self,
        transfer: &crate::ubm_peer_notification_ingress::IngressTransfer,
    ) -> bool {
        self.0.ack_transfer(transfer)
    }

    /// Prepare the currently offered failure without consuming it. The host
    /// must retain this reader with the exact downstream recipient obligation.
    pub fn prepare_failure_recovery(
        &self,
        transfer: &crate::ubm_peer_notification_ingress::IngressTransfer,
    ) -> Option<crate::ubm_peer_notification_ingress::FailureRecovery> {
        self.0.prepare_failure_recovery(transfer)
    }
}

pub(super) struct SourceGroup {
    pub(super) ingress: crate::ubm_peer_notification_ingress::PeerNotificationIngress,
    pub(super) routes: Vec<(
        crate::ubm_peer_notification_ingress::NotificationRoute,
        crate::ubm_peer_notification_ingress::Membership,
        Option<u64>,
        bool, // logically retiring; physical owner remains until replay
    )>,
}

#[derive(Debug)]
struct SourceWrites {
    transaction: Option<(SourceWriteIdentity, SourceWriteReceipt)>,
    next_transaction: Option<u64>,
    forgotten_transaction: Option<u64>,
}

/// Exact request identity retained for idempotent retry within one source group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceWriteRequest {
    pub id: u64,
    pub graph: Vec<u8>,
    pub selector: SourceWriteSelector,
    pub route: crate::ubm_peer_notification_ingress::NotificationRoute,
    pub value: Vec<u8>,
    pub write_type: crate::api::WriteType,
}

pub const SOURCE_MARKER_MAX_WRITE_BYTES: usize = 512;
pub const SOURCE_MARKER_MAX_REQUEST_BYTES: usize = 524_288;
fn marker_request_size_valid(graph: &[u8], selector: &SourceWriteSelector, value: &[u8]) -> bool {
    value.len() <= SOURCE_MARKER_MAX_WRITE_BYTES
        && value
            .len()
            .saturating_add(graph.len())
            .saturating_add(selector.byte_len())
            <= SOURCE_MARKER_MAX_REQUEST_BYTES
}
impl SourceWriteRequest {
    fn valid_size(&self) -> bool {
        marker_request_size_valid(&self.graph, &self.selector, &self.value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceWriteSelector {
    pub peer_id: String,
    pub connection_generation: String,
    pub service_uuid: String,
    pub service_occurrence: u64,
    pub characteristic_uuid: String,
    pub characteristic_occurrence: u64,
}
impl SourceWriteSelector {
    fn byte_len(&self) -> usize {
        self.peer_id
            .len()
            .saturating_add(self.connection_generation.len())
            .saturating_add(self.service_uuid.len())
            .saturating_add(self.characteristic_uuid.len())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceUnresolvedWriteRequest {
    pub id: u64,
    pub graph: Vec<u8>,
    pub selector: SourceWriteSelector,
    pub value: Vec<u8>,
    pub write_type: crate::api::WriteType,
}
impl SourceUnresolvedWriteRequest {
    fn valid_size(&self) -> bool {
        marker_request_size_valid(&self.graph, &self.selector, &self.value)
    }
}
#[derive(Debug)]
enum SourceWriteIdentity {
    Resolved(SourceWriteRequest),
    Unresolved(SourceUnresolvedWriteRequest),
}
impl SourceWriteIdentity {
    fn id(&self) -> u64 {
        match self {
            Self::Resolved(r) => r.id,
            Self::Unresolved(r) => r.id,
        }
    }
    fn matches_resolved(&self, r: &SourceWriteRequest) -> bool {
        match self {
            Self::Resolved(existing) => existing == r,
            Self::Unresolved(existing) => {
                existing.id == r.id
                    && existing.graph == r.graph
                    && existing.selector == r.selector
                    && existing.value == r.value
                    && existing.write_type == r.write_type
            }
        }
    }
    fn matches_unresolved(&self, r: &SourceUnresolvedWriteRequest) -> bool {
        match self {
            Self::Resolved(existing) => {
                existing.id == r.id
                    && existing.graph == r.graph
                    && existing.selector == r.selector
                    && existing.value == r.value
                    && existing.write_type == r.write_type
            }
            Self::Unresolved(existing) => existing == r,
        }
    }
}

pub fn marker_graph_identity(identity: &(String, u64, u64)) -> Vec<u8> {
    let mut encoded = identity.0.as_bytes().to_vec();
    encoded.push(0);
    encoded.extend_from_slice(&identity.1.to_be_bytes());
    encoded.extend_from_slice(&identity.2.to_be_bytes());
    encoded
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceWriteStatus {
    Pending,
    NotAdmitted(SourceGroupError),
    Rejected(SourceGroupError),
    Admitted {
        ordinal: u64,
    },
    Started {
        ordinal: u64,
    },
    InitiationFailed {
        ordinal: u64,
        error: crate::PlatformError,
    },
    Settled {
        ordinal: u64,
        result: Result<(), crate::PlatformError>,
    },
}

#[derive(Clone, Debug)]
pub struct SourceWriteReceipt(std::sync::Arc<std::sync::Mutex<SourceWriteStatus>>);

impl SourceWriteReceipt {
    pub fn status(&self) -> SourceWriteStatus {
        self.0.lock().unwrap().clone()
    }

    pub fn settle(&self, result: Result<(), crate::PlatformError>) {
        let mut status = self.0.lock().unwrap();
        if let SourceWriteStatus::Started { ordinal } = *status {
            *status = SourceWriteStatus::Settled { ordinal, result };
        }
    }
}

pub enum SourceWriteStart<T> {
    Replay(SourceWriteReceipt),
    Initiated {
        receipt: SourceWriteReceipt,
        operation: T,
        wake: Vec<SourceGroupConsumer>,
    },
    Rejected {
        receipt: SourceWriteReceipt,
        wake: Vec<SourceGroupConsumer>,
    },
    InitiationFailed {
        receipt: SourceWriteReceipt,
        wake: Vec<SourceGroupConsumer>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceGroupError {
    StaleGeneration,
    UnknownGroup,
    Capacity,
    SourceStateUnavailable,
    TransactionConflict,
    OrdinalExhausted,
    SubmissionUncertain,
    InvalidRequest,
    TargetNotFound,
    Cancelled,
    Ingress(crate::ubm_peer_notification_ingress::IngressError),
}

/// Slots are owned by the source lock and remain bound to their original
/// generation after closure or replacement. Each slot preallocates the future
/// ingress hold while admission is still safe.
pub(super) struct FutureFailureSlots {
    generation: u64,
    limit: usize,
    recipients: Vec<(
        u64,
        crate::ubm_peer_notification_ingress::FutureFailureRecipient,
    )>,
    reservations: Vec<crate::ubm_peer_notification_ingress::IngressReservation>,
}

#[must_use = "keep the future failure owner until the native operation settles"]
pub(crate) struct FutureSourceFailure {
    source: std::sync::Arc<NotificationFaults>,
    generation: u64,
    id: u64,
    // A host-wide reservation for the one raw native callback this cut may
    // receive before its exact bytes enter durable storage. It is deliberately
    // retained across durable-store backpressure and released only after the
    // store accepts the bytes or this preflight cut is abandoned.
    pending_evidence_reservation:
        Option<std::sync::Arc<dyn crate::ubm_peer_notification_ingress::PendingFailureReservation>>,
    // A byte-budget refusal keeps the exact native allocation and the
    // pre-admitted source-time recipient slots. It must not install a
    // partially visible cut merely to remember that evidence.
    refused: Option<std::sync::Arc<crate::PlatformError>>,
    observed: Option<(CapturedFutureSourceFailure, usize)>,
    final_action: Option<Box<dyn FnOnce() + Send>>,
}

/// Coordinates the final retirement of every immutable recipient of one
/// future source failure. It holds the original capture only after the graph
/// callback has delivered it, then releases that capture and resumes the
/// source owner's deferred cleanup exactly once after every durable recipient
/// has retired its recovery receipt.
struct FutureFailureFinalizer {
    remaining: std::sync::atomic::AtomicUsize,
    captured: std::sync::Mutex<Option<CapturedFutureSourceFailure>>,
    final_action: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct FutureFailureFinalizerTicket {
    owner: std::sync::Arc<FutureFailureFinalizer>,
    completed: std::sync::atomic::AtomicBool,
}

impl FutureFailureFinalizer {
    fn new(recipients: usize, final_action: Box<dyn FnOnce() + Send>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            remaining: std::sync::atomic::AtomicUsize::new(recipients),
            captured: std::sync::Mutex::new(None),
            final_action: std::sync::Mutex::new(Some(final_action)),
        })
    }

    fn ticket(
        self: &std::sync::Arc<Self>,
    ) -> std::sync::Arc<dyn crate::ubm_peer_notification_ingress::FailureRecoveryFinalizer> {
        std::sync::Arc::new(FutureFailureFinalizerTicket {
            owner: self.clone(),
            completed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn arm(&self, captured: CapturedFutureSourceFailure) -> std::io::Result<()> {
        let mut slot = self
            .captured
            .lock()
            .expect("failure finalizer lock poisoned");
        debug_assert!(slot.is_none(), "future failure finalizer armed twice");
        *slot = Some(captured);
        drop(slot);
        if self.remaining.load(std::sync::atomic::Ordering::Acquire) == 0 {
            self.finish()?;
        }
        Ok(())
    }

    fn complete_one(&self) -> std::io::Result<()> {
        let previous = self
            .remaining
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |remaining| remaining.checked_sub(1),
            )
            .expect("failure recovery finalizer completed too many recipients");
        if previous == 1 {
            if let Err(error) = self.finish() {
                self.remaining
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    }

    fn finish(&self) -> std::io::Result<()> {
        let mut slot = self
            .captured
            .lock()
            .expect("failure finalizer lock poisoned");
        let Some(captured) = slot.as_ref() else {
            // The final recipient can retire before the graph callback arms
            // the capture. `arm` observes remaining=0 and performs this same
            // finalization once the callback arrives.
            return Ok(());
        };
        captured.retire_exact_evidence()?;
        let captured = slot.take().expect("captured failure after retirement");
        drop(slot);
        captured.release();
        if let Some(action) = self
            .final_action
            .lock()
            .expect("failure finalizer action lock poisoned")
            .take()
        {
            action();
        }
        Ok(())
    }
}

impl crate::ubm_peer_notification_ingress::FailureRecoveryFinalizer
    for FutureFailureFinalizerTicket
{
    fn complete(&self) -> std::io::Result<()> {
        if !self
            .completed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            if let Err(error) = self.owner.complete_one() {
                self.completed
                    .store(false, std::sync::atomic::Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    }
}

/// One queue node, backed by one admitted future failure owner. The source
/// quota remains charged after capture until the queued cut is settled.
pub(crate) struct FutureFailureCut {
    future: FutureSourceFailure,
    captured: std::sync::Arc<std::sync::Mutex<Option<CapturedFutureSourceFailure>>>,
    node: crate::ubm_reclaim_graph_gate::MandatoryCut<Box<dyn FnOnce() + Send>>,
}

/// A durable-evidence store did not accept a pre-admitted source failure. The
/// exact cut and original callback remain owned so the caller can wait for
/// store capacity or report an explicit non-advance condition; dropping either
/// would turn backpressure into evidence loss.
pub(crate) struct RetriableFutureFailureCut {
    pub(crate) cut: FutureFailureCut,
    pub(crate) reason: SourceGroupError,
    pub(crate) error: std::sync::Arc<crate::PlatformError>,
}

impl std::fmt::Debug for RetriableFutureFailureCut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetriableFutureFailureCut")
            .field("reason", &self.reason)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl FutureFailureCut {
    /// Capture and append while holding the same graph lock used by callbacks.
    pub(crate) fn capture_and_enqueue(
        mut self,
        gate: &mut crate::ubm_reclaim_graph_gate::GraphGate<Box<dyn FnOnce() + Send>>,
        error: crate::PlatformError,
    ) -> Result<bool, RetriableFutureFailureCut> {
        // Mandatory graph cuts intentionally do not consume ordinary callback
        // bytes. The source cut holds one shared native allocation and records
        // its exact retained-byte charge before changing any ingress state.
        // A later host recovery owner streams it in bounded pages; an arbitrary
        // producer-size ceiling would make that evidence unrecoverable.
        let captured = match self.future.capture(error, usize::MAX) {
            Ok(captured) => captured,
            Err((reason, error)) => {
                return Err(RetriableFutureFailureCut {
                    cut: self,
                    reason,
                    error,
                });
            }
        };
        *self.captured.lock().expect("failure cut lock poisoned") = Some(captured);
        Ok(gate.admit_mandatory_cut(self.node))
    }
}

impl FutureSourceFailure {
    /// The first observation freezes the source-time recipient slots even when
    /// the transfer budget refuses it. One shared error is retained, not one
    /// per recipient. A refusal changes no ingress state, so a later retry
    /// either installs the complete original cut or returns the same evidence.
    pub(crate) fn capture(
        &mut self,
        error: impl Into<std::sync::Arc<crate::PlatformError>>,
        budget: usize,
    ) -> Result<CapturedFutureSourceFailure, (SourceGroupError, std::sync::Arc<crate::PlatformError>)>
    {
        let error = error.into();
        if let Some((captured, _)) = self.observed.as_ref() {
            if captured._error != *error {
                return Err((SourceGroupError::TransactionConflict, error));
            }
            if captured
                .held
                .as_ref()
                .expect("held failure")
                .retained_bytes()
                > budget
            {
                return Err((SourceGroupError::Capacity, error));
            }
            return Ok(self.observed.take().expect("observed failure").0);
        }
        if let Some(refused) = self.refused.as_ref() {
            if refused.as_ref() != error.as_ref() {
                return Err((SourceGroupError::TransactionConflict, error));
            }
        }
        {
            let mut state = self.source.lock_source_state();
            let Some(slots) = state.future_failure_owners.get(&self.id) else {
                return Err((SourceGroupError::StaleGeneration, error));
            };
            let count = slots.recipients.len();
            let retained_bytes =
                std::mem::size_of::<CapturedFutureSourceFailure>()
                    .saturating_add(std::mem::size_of::<crate::PlatformError>())
                    .saturating_add(
                        crate::ubm_peer_notification_ingress::platform_error_heap_bytes(&error),
                    )
                    .saturating_add(slots.recipients.capacity().saturating_mul(
                        std::mem::size_of::<(
                            u64,
                            crate::ubm_peer_notification_ingress::FutureFailureRecipient,
                        )>(),
                    ))
                    .saturating_add(
                        slots
                            .reservations
                            .capacity()
                            .saturating_mul(std::mem::size_of::<
                                crate::ubm_peer_notification_ingress::IngressReservation,
                            >()),
                    )
                    .saturating_add(
                        count
                            .saturating_add(1)
                            .saturating_mul(3 * std::mem::size_of::<usize>()),
                    );
            if retained_bytes > budget {
                // Keep both the exact allocation and the source-time slots in
                // their original owners. In particular, do not drain a
                // recipient before the byte budget has admitted the whole
                // cut: that would turn a refusal into a partial delivery.
                self.refused = Some(error.clone());
                return Err((SourceGroupError::Capacity, error));
            }
            let shared_error = match self.source.failure_evidence_store() {
                Some(store) => {
                    crate::ubm_peer_notification_ingress::SharedPlatformError::store_exact(
                        error.as_ref().clone(),
                        store.as_ref(),
                    )
                    .map_err(|store_error| store_error.kind())
                }
                None => Ok(error.clone().into()),
            };
            let shared_error = match shared_error {
                Ok(shared_error) => {
                    // Exact bytes are now owned by the durable store; another
                    // source may safely enter its native operation.
                    self.pending_evidence_reservation = None;
                    shared_error
                }
                Err(kind) => {
                    // The original pre-admitted slots stay installed while
                    // the caller owns the raw callback for retry. This fences
                    // further source admission, so no second oversized error
                    // can turn store backpressure into unbounded retention.
                    if state.source_generation == Some(self.generation) {
                        state.pending_source_failure = true;
                    }
                    self.refused = Some(error.clone());
                    let reason = if kind == std::io::ErrorKind::WouldBlock {
                        SourceGroupError::Capacity
                    } else {
                        SourceGroupError::SourceStateUnavailable
                    };
                    return Err((reason, error));
                }
            };
            let finalizer = FutureFailureFinalizer::new(
                count,
                self.final_action.take().unwrap_or_else(|| Box::new(|| {})),
            );
            let mut slots = state
                .future_failure_owners
                .remove(&self.id)
                .expect("preflighted future failure slots");
            for (_, recipient) in slots.recipients.drain(..) {
                if let Some(reservation) = recipient.install_shared(
                    self.generation,
                    shared_error
                        .clone()
                        .with_recovery_finalizer(finalizer.ticket()),
                ) {
                    slots.reservations.push(reservation);
                }
            }
            if state.source_generation == Some(self.generation) {
                state.pending_source_failure = true;
            }
            state.pending_failure_owners += 1;
            self.observed = Some((
                CapturedFutureSourceFailure {
                    source: self.source.clone(),
                    generation: self.generation,
                    _error: shared_error,
                    held: Some(SourceValueReservation {
                        reservations: slots.reservations,
                        retained_fault_hold: None,
                        terminal_evidence: None,
                        ordinal_exhausted: false,
                        retained_bytes,
                    }),
                    finalizer: Some(finalizer),
                    published: false,
                    finished: false,
                },
                count,
            ));
        }
        self.refused = None;
        Ok(self.observed.take().expect("observed failure").0)
    }

    pub(crate) fn recipient_count(&self) -> Result<usize, SourceGroupError> {
        if let Some((_, count)) = &self.observed {
            return Ok(*count);
        }
        let state = self.source.lock_source_state();
        if state.source_generation != Some(self.generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        let slots = state
            .future_failure_owners
            .get(&self.id)
            .filter(|slots| slots.generation == self.generation)
            .ok_or(SourceGroupError::StaleGeneration)?;
        Ok(slots.recipients.len())
    }
}

#[must_use = "release the captured failure in source order"]
pub(crate) struct CapturedFutureSourceFailure {
    source: std::sync::Arc<NotificationFaults>,
    generation: u64,
    _error: crate::ubm_peer_notification_ingress::SharedPlatformError,
    held: Option<SourceValueReservation>,
    finalizer: Option<std::sync::Arc<FutureFailureFinalizer>>,
    published: bool,
    finished: bool,
}

impl CapturedFutureSourceFailure {
    fn retire_exact_evidence(&self) -> std::io::Result<()> {
        self._error.retire_exact_evidence()
    }

    pub(crate) fn release(mut self) {
        self.publish();
        self.finish();
    }

    /// Transfer source completion to the exact recipient finalizers. The
    /// native cut remains fenced until every recovery has reached durable
    /// retirement; a service change may then resume through `final_action`.
    pub(crate) fn defer_release(mut self) {
        self.publish();
        let finalizer = self
            .finalizer
            .take()
            .expect("captured future failure is missing its finalizer");
        // A zero-recipient source has no durable collector or source-ack
        // receipt. Preserve the failed evidence for restart reconciliation;
        // normal durable retirement is reported by `try_retire_ack`.
        if let Err(error) = finalizer.arm(self) {
            log::error!("unobserved source failure evidence could not retire: {error}");
        }
    }

    fn publish(&mut self) {
        if self.published {
            return;
        }
        if let Some(held) = self.held.take() {
            held.release();
        }
        self.published = true;
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.publish();
        self.finished = true;
        match self.source.close_source_generation(self.generation) {
            Ok(()) | Err(SourceGroupError::StaleGeneration) => {}
            Err(error) => {
                log::error!("future notification failure could not finish generation: {error:?}")
            }
        }
        self.source.lock_source_state().pending_failure_owners -= 1;
    }
}
impl Drop for CapturedFutureSourceFailure {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Drop for FutureSourceFailure {
    fn drop(&mut self) {
        self.source
            .lock_source_state()
            .future_failure_owners
            .remove(&self.id);
    }
}

impl NotificationFaults {
    /// Own the node and all possible recipient slots before native entry.
    pub(crate) fn reserve_future_failure_cut(
        self: &std::sync::Arc<Self>,
        generation: u64,
        deliver: impl FnOnce(CapturedFutureSourceFailure) + Send + 'static,
    ) -> Result<FutureFailureCut, SourceGroupError> {
        self.reserve_future_failure_cut_with_finalizer(generation, deliver, || {})
    }

    /// Like [`Self::reserve_future_failure_cut`], but invokes `final_action`
    /// only after every durable recovery recipient retires its source ACK.
    pub(crate) fn reserve_future_failure_cut_with_finalizer(
        self: &std::sync::Arc<Self>,
        generation: u64,
        deliver: impl FnOnce(CapturedFutureSourceFailure) + Send + 'static,
        final_action: impl FnOnce() + Send + 'static,
    ) -> Result<FutureFailureCut, SourceGroupError> {
        let mut future = self.reserve_future_source_failure(generation, self.capacity)?;
        future.final_action = Some(Box::new(final_action));
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let queued = captured.clone();
        let callback: Box<dyn FnOnce() + Send> = Box::new(move || {
            let captured = queued
                .lock()
                .expect("failure cut lock poisoned")
                .take()
                .expect("queued failure cut was captured");
            deliver(captured);
        });
        Ok(FutureFailureCut {
            future,
            captured,
            node: crate::ubm_reclaim_graph_gate::MandatoryCut::new(callback),
        })
    }

    /// Reserve bounded future recipient holds before the native operation.
    /// Further group opens reserve their hold under this same source lock or
    /// fail without disturbing the operation owner.
    pub(crate) fn reserve_future_source_failure(
        self: &std::sync::Arc<Self>,
        generation: u64,
        limit: usize,
    ) -> Result<FutureSourceFailure, SourceGroupError> {
        // Reserve host-wide raw capacity before taking the source lock. A
        // configured store is shared across peers, and executing its host
        // implementation while holding a source lock could otherwise invert
        // host/source lock ordering. Dropping this local guard returns the
        // slot if source-local preflight subsequently refuses the operation.
        let pending_evidence_reservation = match self.failure_evidence_store() {
            Some(store) => store.reserve_pending_failure().map_err(|error| {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    SourceGroupError::Capacity
                } else {
                    SourceGroupError::SourceStateUnavailable
                }
            })?,
            None => None,
        };
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) || state.pending_source_failure {
            return Err(SourceGroupError::StaleGeneration);
        }
        if limit > self.capacity
            || state.groups.len() > limit
            || state
                .future_failure_owners
                .len()
                .saturating_add(state.pending_failure_owners)
                >= self.capacity
            // A durable store may apply backpressure after a native callback
            // has produced its one unavoidable raw allocation. Reserve only
            // one such owner at a time so a full store cannot accumulate an
            // unbounded set of raw callback errors before their evidence is
            // persisted.
            || (self.failure_evidence_store().is_some()
                && (!state.future_failure_owners.is_empty()
                    || state.pending_failure_owners != 0))
        {
            return Err(SourceGroupError::Capacity);
        }
        let id = state.next_future_failure_owner;
        let next = id.checked_add(1).ok_or(SourceGroupError::Capacity)?;
        let mut recipients = Vec::new();
        recipients
            .try_reserve_exact(limit)
            .map_err(|_| SourceGroupError::Capacity)?;
        for (&group_id, group) in &state.groups {
            if let Some(recipient) = group.ingress.future_failure_recipient() {
                recipients.push((group_id, recipient));
            }
        }
        let mut reservations = Vec::new();
        reservations
            .try_reserve_exact(limit)
            .map_err(|_| SourceGroupError::Capacity)?;
        state.future_failure_owners.insert(
            id,
            FutureFailureSlots {
                generation,
                limit,
                recipients,
                reservations,
            },
        );
        state.next_future_failure_owner = next;
        Ok(FutureSourceFailure {
            source: self.clone(),
            generation,
            id,
            pending_evidence_reservation,
            refused: None,
            observed: None,
            final_action: None,
        })
    }

    /// Capture an observed failure under a byte budget, or return its exact
    /// error to the caller without installing a partial cut. This is not a
    /// reservation made before the native operation begins.
    pub(crate) fn owned_pending_source_failure(
        self: &std::sync::Arc<Self>,
        generation: u64,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        activation: u64,
        error: crate::PlatformError,
        budget: usize,
    ) -> Result<OwnedPendingSourceFailure, (SourceGroupError, crate::PlatformError)> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err((SourceGroupError::StaleGeneration, error));
        }
        let key = (
            route.service_uuid,
            route.service_instance,
            route.characteristic_uuid,
            route.characteristic_instance,
        );
        if !state.source_activations.contains(&(key, activation)) {
            return Err((
                SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                ),
                error,
            ));
        }
        if state
            .pending_failure_owners
            .saturating_add(state.future_failure_owners.len())
            >= self.capacity
        {
            return Err((SourceGroupError::Capacity, error));
        }
        let owner_bytes = std::mem::size_of::<OwnedPendingSourceFailure>().saturating_add(
            crate::ubm_peer_notification_ingress::platform_error_heap_bytes(&error),
        );
        let Some(recipient_budget) = budget.checked_sub(owner_bytes) else {
            return Err((SourceGroupError::Capacity, error));
        };
        let held = match Self::reserve_pending_source_failure_locked(
            &mut state,
            generation,
            &error,
            recipient_budget,
        ) {
            Ok(Some(held)) => held,
            Ok(None) => return Err((SourceGroupError::Capacity, error)),
            Err(reason) => return Err((reason, error)),
        };
        state.pending_failure_owners += 1;
        drop(state);
        Ok(OwnedPendingSourceFailure {
            source: self.clone(),
            generation,
            error,
            held: Some(held),
            reserved: false,
            finished: false,
        })
    }
    /// Retire a generation logically while its terminal remains held by the
    /// callback owner. Already terminated groups retain their earlier reason.
    #[cfg(test)]
    pub(crate) fn reserve_close_source_generation(
        &self,
        generation: u64,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        self.reserve_close_source_generation_bounded(generation, usize::MAX)
    }

    pub(crate) fn reserve_close_source_generation_bounded(
        &self,
        generation: u64,
        max_retained_bytes: usize,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        self.reserve_terminal_source_generation(generation, max_retained_bytes, None)
    }

    /// Retire the exact generation and hold its original failure until replay.
    pub(crate) fn reserve_failure_source_generation(
        &self,
        generation: u64,
        error: crate::PlatformError,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        self.reserve_terminal_source_generation(generation, usize::MAX, Some(error))
    }

    /// Reserve a queued failure without disposing of the source owners until
    /// its physical replay. New groups and callback admissions are refused.
    pub(crate) fn reserve_pending_source_failure(
        &self,
        generation: u64,
        error: crate::PlatformError,
        budget: usize,
    ) -> Result<Option<(u64, SourceValueReservation)>, SourceGroupError> {
        self.reserve_pending_source_failure_inner(generation, error, budget, None)
    }

    fn reserve_pending_source_failure_inner(
        &self,
        generation: u64,
        error: crate::PlatformError,
        budget: usize,
        activation: Option<(crate::ubm_peer_notification_ingress::NotificationRoute, u64)>,
    ) -> Result<Option<(u64, SourceValueReservation)>, SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        if let Some((route, activation)) = activation {
            let key = (
                route.service_uuid,
                route.service_instance,
                route.characteristic_uuid,
                route.characteristic_instance,
            );
            if !state.source_activations.contains(&(key, activation)) {
                return Err(SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                ));
            }
        }
        Self::reserve_pending_source_failure_locked(&mut state, generation, &error, budget)
            .map(|held| held.map(|held| (generation, held)))
    }

    fn reserve_pending_source_failure_locked(
        state: &mut FaultState,
        generation: u64,
        error: &crate::PlatformError,
        budget: usize,
    ) -> Result<Option<SourceValueReservation>, SourceGroupError> {
        if state.pending_source_failure {
            return Ok(None);
        }
        let (reservations, retained_bytes) =
            crate::ubm_peer_notification_ingress::reserve_failure_recipients(
                generation,
                state.groups.values().map(|group| &group.ingress),
                error,
                budget,
            )
            .map_err(SourceGroupError::Ingress)?
            .ok_or(SourceGroupError::Capacity)?;
        state.pending_source_failure = true;
        Ok(Some(SourceValueReservation {
            reservations,
            retained_fault_hold: None,
            terminal_evidence: None,
            ordinal_exhausted: false,
            retained_bytes,
        }))
    }

    fn reserve_terminal_source_generation(
        &self,
        generation: u64,
        max_retained_bytes: usize,
        error: Option<crate::PlatformError>,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        // Generation identity is immutable within an ingress. Check all
        // groups before installing any held terminal, so an invariant failure
        // cannot expose a partially reserved cut.
        if state
            .groups
            .values()
            .any(|group| !group.ingress.has_generation(generation))
        {
            return Err(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::StaleGeneration,
            ));
        }
        let token_bytes =
            std::mem::size_of::<crate::ubm_peer_notification_ingress::IngressReservation>()
                .saturating_add(3 * std::mem::size_of::<usize>());
        if state.groups.len().saturating_mul(token_bytes) > max_retained_bytes {
            return Err(SourceGroupError::Capacity);
        }
        let reservations = Vec::with_capacity(state.groups.len());
        let retained_bytes = reservations.capacity().saturating_mul(token_bytes);
        if retained_bytes > max_retained_bytes {
            return Err(SourceGroupError::Capacity);
        }
        let mut reservation = SourceValueReservation {
            reservations,
            retained_fault_hold: None,
            terminal_evidence: None,
            ordinal_exhausted: false,
            retained_bytes,
        };
        // This admission was made before the native producer was enabled.
        // Move it with the terminal cut; do not allocate a second durable
        // slot while detaching, when refusal would be too late to prevent
        // unpersistable native output.
        reservation.terminal_evidence = state.terminal_evidence.take();
        for group in state.groups.values() {
            let hold = match &error {
                Some(error) => group.ingress.reserve_failure(generation, error.clone()),
                None => group.ingress.reserve_close(generation),
            }
            .map_err(SourceGroupError::Ingress)?;
            if let Some(hold) = hold {
                reservation.reservations.push(hold);
            }
        }
        state.source_generation = None;
        state.pending_source_failure = false;
        state.groups.clear();
        state.enable_epochs.clear();
        state.direct_faults.clear();
        state.held_direct_faults.clear();
        state.direct_faults_exhausted = false;
        state.source_activations.clear();
        state.activation_faults.clear();
        state.held_activation_faults.clear();
        Ok(reservation)
    }

    /// Capture a native callback and all current memberships under the
    /// source lock. Visibility is held until the callback owner replays it.
    #[cfg(test)]
    pub(crate) fn reserve_source_value(
        &self,
        generation: u64,
        value: &crate::api::ValueNotification,
        provenance: crate::ubm_peer_notification_ingress::CallbackProvenance,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        self.reserve_source_value_bounded(generation, value, provenance, usize::MAX)
    }

    /// Preflight the token allocation while holding the source lock, before
    /// assigning an ordinal or installing any held ingress record.
    pub(crate) fn reserve_source_value_bounded(
        &self,
        generation: u64,
        value: &crate::api::ValueNotification,
        provenance: crate::ubm_peer_notification_ingress::CallbackProvenance,
        max_retained_bytes: usize,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        self.reserve_source_value_inner(generation, value, provenance, max_retained_bytes, None)
    }

    /// Reserve one physical activation's callback and its current memberships.
    pub(crate) fn reserve_source_value_for_activation_bounded(
        &self,
        generation: u64,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        activation: u64,
        value: &crate::api::ValueNotification,
        max_retained_bytes: usize,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        self.reserve_source_value_inner(
            generation,
            value,
            crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown,
            max_retained_bytes,
            Some((route, activation)),
        )
    }

    fn reserve_source_value_inner(
        &self,
        generation: u64,
        value: &crate::api::ValueNotification,
        provenance: crate::ubm_peer_notification_ingress::CallbackProvenance,
        max_retained_bytes: usize,
        activation: Option<(crate::ubm_peer_notification_ingress::NotificationRoute, u64)>,
    ) -> Result<SourceValueReservation, SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) || state.pending_source_failure {
            return Err(SourceGroupError::StaleGeneration);
        }
        if let Some((route, id)) = activation {
            let key = (
                route.service_uuid,
                route.service_instance,
                route.characteristic_uuid,
                route.characteristic_instance,
            );
            if !route.matches(value) || !state.source_activations.contains(&(key, id)) {
                return Err(SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                ));
            }
        }
        if state
            .groups
            .values()
            .any(|group| !group.ingress.has_generation(generation))
        {
            return Err(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::StaleGeneration,
            ));
        }
        // Each ingress token also owns an Arc allocation for its hold. Use a
        // conservative charge for the Arc's counters and atomic state.
        let token_bytes =
            std::mem::size_of::<crate::ubm_peer_notification_ingress::IngressReservation>()
                .saturating_add(3 * std::mem::size_of::<usize>());
        let fault_bytes = if value.source_failure.is_some() {
            3 * std::mem::size_of::<usize>()
        } else {
            0
        };
        if state
            .groups
            .len()
            .saturating_mul(token_bytes)
            .saturating_add(fault_bytes)
            > max_retained_bytes
        {
            return Err(SourceGroupError::Capacity);
        }
        let reservations = Vec::with_capacity(state.groups.len());
        let retained_bytes = reservations
            .capacity()
            .saturating_mul(token_bytes)
            .saturating_add(fault_bytes);
        if retained_bytes > max_retained_bytes {
            return Err(SourceGroupError::Capacity);
        }
        let Some(ordinal) = state.next_source_ordinal else {
            let mut reservations = reservations;
            reservations.extend(state.groups.values().filter_map(|group| {
                group
                    .ingress
                    .reserve_source_ordinal_exhausted(generation, value.value.len())
            }));
            return Ok(SourceValueReservation {
                reservations,
                retained_fault_hold: None,
                terminal_evidence: None,
                ordinal_exhausted: true,
                retained_bytes,
            });
        };
        state.next_source_ordinal = ordinal.checked_add(1);
        let mut retained_fault_hold = None;
        if value.source_failure.is_some() && activation.is_some() {
            let (route, id) = activation.expect("checked activation");
            let key = (
                route.service_uuid,
                route.service_instance,
                route.characteristic_uuid,
                route.characteristic_instance,
            );
            if !state.activation_faults.contains_key(&(key, id))
                && state.activation_faults.len() < self.capacity
            {
                let mut retained = value.clone();
                retained.lost_before = 0;
                let hold = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                state.activation_faults.insert((key, id), retained);
                state.held_activation_faults.insert((key, id), hold.clone());
                retained_fault_hold = Some(hold);
            } else if !state.activation_faults.contains_key(&(key, id)) {
                state.direct_faults_exhausted = true;
            }
        } else if value.source_failure.is_some() {
            let key = Self::key(value);
            if !state.direct_faults.contains_key(&key) {
                if state.direct_faults.len() < self.capacity {
                    let mut retained = value.clone();
                    retained.lost_before = 0;
                    let hold = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                    state.direct_faults.insert(key, retained);
                    state.held_direct_faults.insert(key, hold.clone());
                    retained_fault_hold = Some(hold);
                } else {
                    state.direct_faults_exhausted = true;
                }
            }
        }
        let mut reservation = SourceValueReservation {
            reservations,
            retained_fault_hold,
            terminal_evidence: None,
            ordinal_exhausted: false,
            retained_bytes,
        };
        for group in state.groups.values() {
            let memberships: Vec<_> = group
                .routes
                .iter()
                .filter(|(route, _, owner, retiring)| {
                    !retiring
                        && match activation {
                            Some((expected, id)) => *route == expected && *owner == Some(id),
                            None => route.matches(value),
                        }
                })
                .map(|(_, membership, _, _)| *membership)
                .collect();
            if memberships.is_empty() && value.lost_before == 0 {
                continue;
            }
            let mut scoped = value.clone();
            if memberships.is_empty() {
                scoped.source_failure = None;
            }
            match group.ingress.reserve_members_at_ordinal(
                generation,
                &memberships,
                &scoped,
                provenance,
                ordinal,
            ) {
                Ok(ingress) => reservation.reservations.push(ingress),
                // Ingress records its own terminal when its bound is exceeded;
                // another already-terminated group cannot accept this callback.
                Err(crate::ubm_peer_notification_ingress::ReservationError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::Terminated,
                )) => {}
                Err(crate::ubm_peer_notification_ingress::ReservationError::Ingress(error)) => {
                    drop(state);
                    drop(reservation);
                    return Err(SourceGroupError::Ingress(error));
                }
            }
        }
        Ok(reservation)
    }

    /// Install the exact physical activation before its native enable begins.
    /// Distinct physical owners of the same route remain independent.
    pub fn start_source_activation(
        &self,
        generation: u64,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        activation: u64,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        let key = (
            route.service_uuid,
            route.service_instance,
            route.characteristic_uuid,
            route.characteristic_instance,
        );
        if !state.source_activations.contains(&(key, activation))
            && state.source_activations.len() >= self.capacity
        {
            return Err(SourceGroupError::Capacity);
        }
        state.source_activations.insert((key, activation));
        Ok(())
    }

    /// Admit only values belonging to the currently installed physical activation.
    pub fn publish_source_value_for_activation(
        &self,
        generation: u64,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        activation: u64,
        value: crate::api::ValueNotification,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        let key = (
            route.service_uuid,
            route.service_instance,
            route.characteristic_uuid,
            route.characteristic_instance,
        );
        if !state.source_activations.contains(&(key, activation)) || !route.matches(&value) {
            return Err(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
            ));
        }
        if value.source_failure.is_some() {
            if state.activation_faults.contains_key(&(key, activation))
                || state.activation_faults.len() < self.capacity
            {
                let mut retained = value.clone();
                retained.lost_before = 0;
                state
                    .activation_faults
                    .entry((key, activation))
                    .or_insert(retained);
            } else {
                state.direct_faults_exhausted = true;
            }
        }
        let Some(ordinal) = Self::source_ordinal_locked(&mut state, value.value.len()) else {
            return Err(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::Terminated,
            ));
        };
        for group in state.groups.values() {
            let memberships: Vec<_> = group
                .routes
                .iter()
                .filter(|(candidate, _, owner, retiring)| {
                    !retiring && *candidate == route && *owner == Some(activation)
                })
                .map(|(_, membership, _, _)| *membership)
                .collect();
            if !memberships.is_empty() {
                let _ = group.ingress.admit_members_at_ordinal(
                    generation,
                    &memberships,
                    crate::api::NotificationEvent::Value(value.clone()),
                    crate::ubm_peer_notification_ingress::CallbackProvenance::Unknown,
                    ordinal,
                );
            } else if value.lost_before != 0 {
                let _ = group.ingress.admit(
                    generation,
                    None,
                    crate::api::NotificationEvent::Lagged {
                        skipped_records: value.lost_before,
                    },
                );
            }
        }
        Ok(())
    }

    /// Clear only the failure and identity belonging to a confirmed retirement.
    pub fn retire_source_activation(
        &self,
        generation: u64,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        activation: u64,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        let key = (
            route.service_uuid,
            route.service_instance,
            route.characteristic_uuid,
            route.characteristic_instance,
        );
        if !state.source_activations.remove(&(key, activation)) {
            return Err(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
            ));
        }
        state.activation_faults.remove(&(key, activation));
        state.held_activation_faults.remove(&(key, activation));
        Ok(())
    }

    /// Start a strictly newer physical generation; prior groups must already be closed.
    pub fn start_source_generation(&self, generation: u64) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation.is_some()
            || state
                .last_source_generation
                .is_some_and(|last| generation <= last)
        {
            return Err(SourceGroupError::StaleGeneration);
        }
        // Durable terminal capacity is an activation prerequisite.  This is
        // intentionally before source_generation becomes visible: callers
        // must not enable the native producer after a truthful refusal.
        let terminal_evidence = match self.terminal_evidence_port.as_ref() {
            Some(port) => Some((
                port.clone(),
                port.reserve("subscription", generation, self.capacity)
                    .map_err(|_| SourceGroupError::Capacity)?,
            )),
            None => None,
        };
        state.source_generation = Some(generation);
        state.pending_source_failure = false;
        state.next_source_ordinal = Some(0);
        state.last_source_generation = Some(generation);
        state.terminal_evidence = terminal_evidence;
        Ok(())
    }

    pub fn open_source_group(
        &self,
        generation: u64,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<(SourceGroupHandle, SourceGroupConsumer), SourceGroupError> {
        let mut state = self.lock_source_state();
        if state.source_generation != Some(generation) || state.pending_source_failure {
            return Err(SourceGroupError::StaleGeneration);
        }
        if state.groups.len() >= self.capacity
            || state.future_failure_owners.values().any(|owner| {
                owner.generation == generation && owner.recipients.len() >= owner.limit
            })
        {
            return Err(SourceGroupError::Capacity);
        }
        let id = state.next_group;
        let next_group = id.checked_add(1).ok_or(SourceGroupError::Capacity)?;
        if !self.wake_coordinator.reserve() {
            return Err(SourceGroupError::Capacity);
        }
        let ingress =
            crate::ubm_peer_notification_ingress::PeerNotificationIngress::new_source_owned(
                generation,
                max_records,
                max_bytes,
                self.wake_coordinator.clone(),
            );
        for owner in state
            .future_failure_owners
            .values_mut()
            .filter(|owner| owner.generation == generation)
        {
            if let Some(recipient) = ingress.future_failure_recipient() {
                owner.recipients.push((id, recipient));
            }
        }
        let writes = std::sync::Arc::new(std::sync::Mutex::new(SourceWrites {
            transaction: None,
            next_transaction: Some(0),
            forgotten_transaction: None,
        }));
        state.groups.insert(
            id,
            SourceGroup {
                ingress: ingress.clone(),
                routes: Vec::new(),
            },
        );
        state.next_group = next_group;
        Ok((
            SourceGroupHandle {
                generation,
                id,
                owner: self.owner.clone(),
                writes,
            },
            SourceGroupConsumer(ingress),
        ))
    }

    /// Admit the marker and synchronously initiate the native write while the
    /// source admission lock is held. The caller must retain and drive the
    /// operation and wake the consumer after releasing any platform locks.
    pub fn start_source_marker_write<T>(
        &self,
        handle: &SourceGroupHandle,
        request: SourceWriteRequest,
        initiate: impl FnOnce() -> Result<T, crate::PlatformError>,
    ) -> Result<SourceWriteStart<T>, SourceGroupError> {
        self.start_source_marker_write_inner(handle, request, false, initiate)
    }

    pub(crate) fn start_reserved_source_marker_write<T>(
        &self,
        handle: &SourceGroupHandle,
        request: SourceWriteRequest,
        initiate: impl FnOnce() -> Result<T, crate::PlatformError>,
    ) -> Result<SourceWriteStart<T>, SourceGroupError> {
        self.start_source_marker_write_inner(handle, request, true, initiate)
    }

    fn start_source_marker_write_inner<T>(
        &self,
        handle: &SourceGroupHandle,
        request: SourceWriteRequest,
        consume_pending: bool,
        initiate: impl FnOnce() -> Result<T, crate::PlatformError>,
    ) -> Result<SourceWriteStart<T>, SourceGroupError> {
        if !consume_pending
            && let Some(receipt) = self.replay_source_marker_write(handle, &request)?
        {
            return Ok(SourceWriteStart::Replay(receipt));
        }
        let mut state = self.lock_source_state();
        let receipt = {
            self.source_group_mut(&mut state, handle)?;
            let mut writes = handle.writes.lock().unwrap();
            if let Some((existing, receipt)) = &writes.transaction {
                if !existing.matches_resolved(&request) {
                    return Err(SourceGroupError::TransactionConflict);
                }
                if consume_pending && matches!(receipt.status(), SourceWriteStatus::Pending) {
                    receipt.clone()
                } else {
                    return Ok(SourceWriteStart::Replay(receipt.clone()));
                }
            } else {
                if consume_pending {
                    return Err(SourceGroupError::TransactionConflict);
                }
                if !request.valid_size() {
                    return Err(SourceGroupError::InvalidRequest);
                }
                if writes.next_transaction != Some(request.id) {
                    return Err(SourceGroupError::TransactionConflict);
                }
                let receipt = SourceWriteReceipt(std::sync::Arc::new(std::sync::Mutex::new(
                    SourceWriteStatus::Pending,
                )));
                writes.transaction = Some((
                    SourceWriteIdentity::Resolved(request.clone()),
                    receipt.clone(),
                ));
                receipt
            }
        };
        // The receipt lock is the cancellation/admission linearization. A
        // cancellation that wins here prevents both marker and write; after
        // this lock publishes Admitted it can no longer reverse the effect.
        let mut marker_status = receipt.0.lock().unwrap();
        if !matches!(*marker_status, SourceWriteStatus::Pending) {
            drop(marker_status);
            return Ok(SourceWriteStart::Replay(receipt));
        }
        let already_terminated = self
            .source_group_mut(&mut state, handle)?
            .ingress
            .marker_admission_terminated(handle.generation)
            .map_err(SourceGroupError::Ingress)?;
        if already_terminated {
            *marker_status = SourceWriteStatus::NotAdmitted(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::Terminated,
            ));
            drop(marker_status);
            return Ok(SourceWriteStart::Rejected {
                receipt,
                wake: Vec::new(),
            });
        }
        let Some(ordinal) = state.next_source_ordinal else {
            let wake = state
                .groups
                .iter()
                .map(|(id, group)| {
                    if *id == handle.id {
                        group
                            .ingress
                            .exhaust_marker_ordinal_deferred(handle.generation, request.id);
                    } else {
                        group
                            .ingress
                            .exhaust_source_ordinal_deferred(handle.generation, 0);
                    }
                    SourceGroupConsumer(group.ingress.clone())
                })
                .collect();
            *marker_status = SourceWriteStatus::Rejected(SourceGroupError::OrdinalExhausted);
            drop(marker_status);
            return Ok(SourceWriteStart::Rejected { receipt, wake });
        };
        state.next_source_ordinal = ordinal.checked_add(1);
        let group = self.source_group_mut(&mut state, handle)?;
        let wake = vec![SourceGroupConsumer(group.ingress.clone())];
        let admitted =
            group
                .ingress
                .admit_marker_at_ordinal(handle.generation, request.id, ordinal);
        let status = match admitted {
            Ok(()) => SourceWriteStatus::Admitted { ordinal },
            Err(error) => SourceWriteStatus::Rejected(SourceGroupError::Ingress(error)),
        };
        *marker_status = status;
        drop(marker_status);
        match admitted {
            Ok(()) => match initiate() {
                Ok(operation) => {
                    *receipt.0.lock().unwrap() = SourceWriteStatus::Started { ordinal };
                    Ok(SourceWriteStart::Initiated {
                        receipt,
                        operation,
                        wake,
                    })
                }
                Err(error) => {
                    *receipt.0.lock().unwrap() =
                        SourceWriteStatus::InitiationFailed { ordinal, error };
                    Ok(SourceWriteStart::InitiationFailed { receipt, wake })
                }
            },
            Err(_) => Ok(SourceWriteStart::Rejected { receipt, wake }),
        }
    }

    /// Replays an exact retained transaction even after the group graph retires.
    pub fn replay_source_marker_write(
        &self,
        handle: &SourceGroupHandle,
        request: &SourceWriteRequest,
    ) -> Result<Option<SourceWriteReceipt>, SourceGroupError> {
        if !std::sync::Arc::ptr_eq(&self.owner, &handle.owner) {
            return Err(SourceGroupError::UnknownGroup);
        }
        handle.replay_marker_write(request)
    }

    /// Forget a settled or rejected exact transaction; IDs are never reused.
    pub fn forget_source_marker_write(
        &self,
        handle: &SourceGroupHandle,
        id: u64,
    ) -> Result<(), SourceGroupError> {
        if !std::sync::Arc::ptr_eq(&self.owner, &handle.owner) {
            return Err(SourceGroupError::UnknownGroup);
        }
        handle.forget_marker_write(id)
    }

    pub fn register_source_route(
        &self,
        handle: &SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
    ) -> Result<crate::ubm_peer_notification_ingress::Membership, SourceGroupError> {
        self.register_source_route_inner(handle, route, member, None)
    }

    /// Bind a membership to one exact physical source activation.
    pub fn register_source_route_for_activation(
        &self,
        handle: &SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
        activation: u64,
    ) -> Result<crate::ubm_peer_notification_ingress::Membership, SourceGroupError> {
        self.register_source_route_inner(handle, route, member, Some(activation))
    }

    fn register_source_route_inner(
        &self,
        handle: &SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        member: u64,
        activation: Option<u64>,
    ) -> Result<crate::ubm_peer_notification_ingress::Membership, SourceGroupError> {
        let mut state = self.lock_source_state();
        let key = (
            route.service_uuid,
            route.service_instance,
            route.characteristic_uuid,
            route.characteristic_instance,
        );
        if let Some(activation) = activation {
            if !state.source_activations.contains(&(key, activation)) {
                return Err(SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                ));
            }
            if state
                .held_activation_faults
                .get(&(key, activation))
                .is_some_and(|hold| hold.load(std::sync::atomic::Ordering::Acquire))
            {
                return Err(SourceGroupError::SourceStateUnavailable);
            }
            state.held_activation_faults.remove(&(key, activation));
        }
        if state
            .held_direct_faults
            .get(&key)
            .is_some_and(|hold| hold.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(SourceGroupError::SourceStateUnavailable);
        }
        state.held_direct_faults.remove(&key);
        let retained =
            state.direct_faults.get(&key).cloned().or_else(|| {
                activation.and_then(|id| state.activation_faults.get(&(key, id)).cloned())
            });
        if retained.is_none() && state.direct_faults_exhausted {
            return Err(SourceGroupError::SourceStateUnavailable);
        }
        let group = self.source_group_mut(&mut state, handle)?;
        if group.routes.len() >= self.capacity {
            return Err(SourceGroupError::Capacity);
        }
        let membership = group
            .ingress
            .register(handle.generation, route, member)
            .map_err(SourceGroupError::Ingress)?;
        group.routes.push((route, membership, activation, false));
        if let Some(failure) = retained {
            // The captured failure and registration share the publication lock.
            // A different characteristic's retained error says nothing about this route.
            let _ = group.ingress.admit(
                handle.generation,
                Some(membership),
                crate::api::NotificationEvent::Value(failure),
            );
        }
        Ok(membership)
    }

    pub fn retire_source_route(
        &self,
        handle: &SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        membership: crate::ubm_peer_notification_ingress::Membership,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        let group = self.source_group_mut(&mut state, handle)?;
        let index = group
            .routes
            .iter()
            .position(|entry| entry.0 == route && entry.1 == membership && !entry.3)
            .ok_or(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
            ))?;
        let result = group.ingress.retire(handle.generation, route, membership);
        // A failed enqueue terminates the group, so it must not retain this route.
        group.routes.remove(index);
        result.map_err(SourceGroupError::Ingress)
    }

    /// Establish logical exclusion and a held FIFO cut under the source lock.
    pub(crate) fn reserve_source_route_retirement(
        self: &std::sync::Arc<Self>,
        handle: &SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        membership: crate::ubm_peer_notification_ingress::Membership,
    ) -> Result<SourceRouteRetirement, SourceGroupError> {
        let mut state = self.lock_source_state();
        let group = self.source_group_mut(&mut state, handle)?;
        let index = group
            .routes
            .iter()
            .position(|entry| entry.0 == route && entry.1 == membership && !entry.3)
            .ok_or(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
            ))?;
        let reservation = group
            .ingress
            .reserve_retirement(handle.generation, route, membership)
            .map_err(SourceGroupError::Ingress)?;
        group.routes[index].3 = true;
        let reservations: Vec<_> = reservation.into_iter().collect();
        let retained_bytes = reservations.capacity().saturating_mul(
            std::mem::size_of::<crate::ubm_peer_notification_ingress::IngressReservation>()
                .saturating_add(3 * std::mem::size_of::<usize>()),
        );
        Ok(SourceRouteRetirement {
            source: self.clone(),
            group: handle.clone(),
            route,
            membership,
            held: Some(SourceValueReservation {
                reservations,
                retained_fault_hold: None,
                terminal_evidence: None,
                ordinal_exhausted: false,
                retained_bytes,
            }),
        })
    }

    /// Complete only the exact admitted physical owner; a detached group or a
    /// replacement generation has no effect on this retirement.
    pub(crate) fn complete_source_route_retirement(
        &self,
        handle: &SourceGroupHandle,
        route: crate::ubm_peer_notification_ingress::NotificationRoute,
        membership: crate::ubm_peer_notification_ingress::Membership,
    ) {
        let mut state = self.lock_source_state();
        if let Ok(group) = self.source_group_mut(&mut state, handle) {
            if let Some(index) = group
                .routes
                .iter()
                .position(|entry| entry.0 == route && entry.1 == membership && entry.3)
            {
                group.routes.remove(index);
            }
        }
    }

    pub fn source_group_barrier(
        &self,
        handle: &SourceGroupHandle,
        id: u64,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        let group = self.source_group_mut(&mut state, handle)?;
        group
            .ingress
            .barrier(handle.generation, id)
            .map_err(SourceGroupError::Ingress)
    }

    pub fn detach_source_group(&self, handle: &SourceGroupHandle) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        self.source_group_mut(&mut state, handle)?;
        let group = state
            .groups
            .remove(&handle.id)
            .ok_or(SourceGroupError::UnknownGroup)?;
        // Pre-admitted failure owners retain their ingress holds even after
        // the group leaves the routing table.
        let _ = group.ingress.close(handle.generation, true);
        Ok(())
    }

    /// Detach only after the caller holds the source admission fence and has
    /// received downstream acknowledgement for this exact consumed barrier.
    /// Refuses without detaching when routes, FIFO records or pre-admitted
    /// failure owners remain, or the barrier is stale. Forced detach remains
    /// available with uncertain tail and retains those failure obligations.
    pub fn detach_source_group_after_barrier(
        &self,
        handle: &SourceGroupHandle,
        barrier_id: u64,
    ) -> Result<(), SourceGroupError> {
        let mut state = self.lock_source_state();
        let group = self.source_group_mut(&mut state, handle)?;
        group
            .ingress
            .close_after_barrier(handle.generation, barrier_id)
            .map_err(SourceGroupError::Ingress)?;
        state
            .groups
            .remove(&handle.id)
            .ok_or(SourceGroupError::UnknownGroup)?;
        Ok(())
    }

    fn source_group_mut<'a>(
        &self,
        state: &'a mut FaultState,
        handle: &SourceGroupHandle,
    ) -> Result<&'a mut SourceGroup, SourceGroupError> {
        if !std::sync::Arc::ptr_eq(&self.owner, &handle.owner) {
            return Err(SourceGroupError::UnknownGroup);
        }
        if state.source_generation != Some(handle.generation) {
            return Err(SourceGroupError::StaleGeneration);
        }
        state
            .groups
            .get_mut(&handle.id)
            .ok_or(SourceGroupError::UnknownGroup)
    }

    pub(super) fn close_source_groups(state: &mut FaultState) {
        state.pending_source_failure = false;
        if let Some(generation) = state.source_generation.take() {
            for (_, group) in std::mem::take(&mut state.groups) {
                // Already-terminated groups retain their original terminal reason.
                let _ = group.ingress.close(generation, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Characteristic;
    use crate::common::util::retained_fault_tests::note;
    use uuid::Uuid;

    struct RefusingTerminalEvidence;

    impl crate::ubm_peer_notification_ingress::TerminalEvidencePort for RefusingTerminalEvidence {
        fn reserve(
            &self,
            _producer: &str,
            _generation: u64,
            _maximum_bytes: usize,
        ) -> std::io::Result<String> {
            Err(std::io::Error::other("quota exhausted"))
        }

        fn release(&self, _reservation: &str) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_evidence_refusal_prevents_source_activation() {
        let source = NotificationFaults::new_with_terminal_evidence_port(
            2,
            None,
            Some(std::sync::Arc::new(RefusingTerminalEvidence)),
        );

        assert_eq!(
            source.start_source_generation(1),
            Err(SourceGroupError::Capacity)
        );
        assert!(source.lock_source_state().source_generation.is_none());
        assert_eq!(
            source.open_source_group(1, 1, 1),
            Err(SourceGroupError::StaleGeneration)
        );
    }

    #[test]
    fn cancellation_while_reserved_actor_waits_on_source_fence_prevents_write() {
        use std::sync::{Arc, mpsc};
        let source = Arc::new(NotificationFaults::new(2));
        source.start_source_generation(1).unwrap();
        let (group, consumer) = source.open_source_group(1, 2, 8).unwrap();
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![2],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 0,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 0,
            },
        };
        let unresolved = SourceUnresolvedWriteRequest {
            id: request.id,
            graph: request.graph.clone(),
            selector: request.selector.clone(),
            value: request.value.clone(),
            write_type: request.write_type,
        };
        group.reserve_marker_write(request.clone()).unwrap();
        let guard = source.state.lock().unwrap();
        let other = source.clone();
        let other_group = group.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            other
                .start_reserved_source_marker_write::<()>(&other_group, request, || {
                    panic!("cancelled write dispatched")
                })
                .unwrap()
        });
        entered_rx.recv().unwrap();
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&unresolved)
                .unwrap()
                .status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
        );
        drop(guard);
        assert!(matches!(
            worker.join().unwrap(),
            SourceWriteStart::Replay(_)
        ));
        assert_eq!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Empty
        );
        group.forget_marker_write(0).unwrap();
    }

    #[test]
    fn exact_cancel_wins_before_admission_and_preserves_post_admission_truth() {
        let source = NotificationFaults::new(4);
        source.start_source_generation(1).unwrap();
        let (group, consumer) = source.open_source_group(1, 4, 8).unwrap();
        let mut request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![2],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 0,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 0,
            },
        };
        let unresolved = |r: &SourceWriteRequest| SourceUnresolvedWriteRequest {
            id: r.id,
            graph: r.graph.clone(),
            selector: r.selector.clone(),
            value: r.value.clone(),
            write_type: r.write_type,
        };
        let original = unresolved(&request);
        let cancelled = group.cancel_unresolved_marker_write(&original).unwrap();
        assert_eq!(
            cancelled.status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
        );
        assert!(matches!(
            source
                .start_source_marker_write::<()>(&group, request.clone(), || panic!(
                    "cancelled write dispatched"
                ))
                .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&original)
                .unwrap()
                .status(),
            cancelled.status()
        );
        assert_eq!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Empty
        );
        group.forget_marker_write(0).unwrap();

        request.id = 1;
        let pending = unresolved(&request);
        let (receipt, reserved) = group.reserve_marker_write(request.clone()).unwrap();
        assert!(reserved);
        assert_eq!(receipt.status(), SourceWriteStatus::Pending);
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&pending)
                .unwrap()
                .status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::Cancelled)
        );
        assert!(matches!(
            source
                .start_reserved_source_marker_write::<()>(&group, request.clone(), || panic!(
                    "cancelled queued write dispatched"
                ))
                .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        group.forget_marker_write(1).unwrap();

        request.id = 2;
        let admitted_request = unresolved(&request);
        let SourceWriteStart::Initiated { receipt, wake, .. } = source
            .start_source_marker_write(&group, request.clone(), || {
                assert_eq!(
                    group
                        .cancel_unresolved_marker_write(&admitted_request)
                        .unwrap()
                        .status(),
                    SourceWriteStatus::Admitted { ordinal: 0 }
                );
                Ok(())
            })
            .unwrap()
        else {
            panic!("expected dispatch")
        };
        for consumer in wake {
            consumer.wake();
        }
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Marker { id: 2, ordinal: 0 }
            )
        ));
        assert_eq!(
            group
                .cancel_unresolved_marker_write(&admitted_request)
                .unwrap()
                .status(),
            SourceWriteStatus::Started { ordinal: 0 }
        );
        receipt.settle(Ok(()));
        assert!(matches!(
            group
                .cancel_unresolved_marker_write(&admitted_request)
                .unwrap()
                .status(),
            SourceWriteStatus::Settled {
                ordinal: 0,
                result: Ok(())
            }
        ));
        let mut conflict = admitted_request;
        conflict.value.push(3);
        assert!(matches!(
            group.cancel_unresolved_marker_write(&conflict),
            Err(SourceGroupError::TransactionConflict)
        ));
        group.forget_marker_write(2).unwrap();
    }

    #[test]
    fn marker_after_existing_terminal_is_not_admitted_and_preserves_terminal() {
        use crate::ubm_peer_notification_ingress::{
            IngressRecord, Poll, RejectedRecord, TerminalReason,
        };
        for exhausted in [false, true] {
            let source = NotificationFaults::new(2);
            source.start_source_generation(1).unwrap();
            let (group, consumer) = source.open_source_group(1, 0, 8).unwrap();
            assert!(source.source_group_barrier(&group, 8).is_err());
            if exhausted {
                source.state.lock().unwrap().next_source_ordinal = None;
            }
            let request = SourceWriteRequest {
                selector: Default::default(),
                id: 0,
                graph: vec![1],
                value: vec![2],
                write_type: crate::api::WriteType::WithResponse,
                route: crate::ubm_peer_notification_ingress::NotificationRoute {
                    service_uuid: Uuid::nil(),
                    service_instance: 0,
                    characteristic_uuid: Uuid::nil(),
                    characteristic_instance: 0,
                },
            };
            let SourceWriteStart::Rejected { receipt, .. } = source
                .start_source_marker_write::<()>(&group, request, || {
                    panic!("terminal group dispatched")
                })
                .unwrap()
            else {
                panic!("expected refusal")
            };
            assert!(matches!(
                receipt.status(),
                SourceWriteStatus::NotAdmitted(SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::Terminated
                ))
            ));
            assert!(matches!(
                consumer.poll(),
                Poll::Record(IngressRecord::Terminal {
                    reason: TerminalReason::Overflow {
                        rejected: RejectedRecord::Barrier { id: 8 },
                        ..
                    },
                    ..
                })
            ));
            group.forget_marker_write(0).unwrap();
        }
    }

    #[test]
    fn write_retention_is_independent_of_fifo_bytes_and_invalid_size_does_not_consume_id() {
        let source = NotificationFaults::new(2);
        source.start_source_generation(1).unwrap();
        let (group, _) = source.open_source_group(1, 4, 1).unwrap();
        let mut request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![2; SOURCE_MARKER_MAX_WRITE_BYTES],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 0,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 0,
            },
        };
        let SourceWriteStart::Initiated { receipt, .. } = source
            .start_source_marker_write(&group, request.clone(), || Ok(()))
            .unwrap()
        else {
            panic!("expected dispatch")
        };
        receipt.settle(Ok(()));
        group.forget_marker_write(0).unwrap();
        request.id = 1;
        request.value.push(3);
        assert!(matches!(
            source.start_source_marker_write::<()>(&group, request.clone(), || panic!(
                "oversized write"
            )),
            Err(SourceGroupError::InvalidRequest)
        ));
        request.value.pop();
        assert!(matches!(
            source
                .start_source_marker_write(&group, request, || Ok(()))
                .unwrap(),
            SourceWriteStart::Initiated { .. }
        ));
    }

    #[test]
    fn unresolved_selector_refusal_replays_when_target_later_resolves() {
        let source = NotificationFaults::new(2);
        source.start_source_generation(1).unwrap();
        let (group, _) = source.open_source_group(1, 2, 512).unwrap();
        let selector = SourceWriteSelector {
            peer_id: "peer".into(),
            connection_generation: "generation".into(),
            service_uuid: "service".into(),
            service_occurrence: 1,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 2,
        };
        let unresolved = SourceUnresolvedWriteRequest {
            id: 0,
            graph: vec![1],
            selector: selector.clone(),
            value: vec![3],
            write_type: crate::api::WriteType::WithResponse,
        };
        let receipt = group
            .record_unresolved_marker_not_admitted(
                unresolved.clone(),
                SourceGroupError::StaleGeneration,
            )
            .unwrap();
        assert_eq!(
            receipt.status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::StaleGeneration)
        );
        let resolved = SourceWriteRequest {
            selector,
            id: 0,
            graph: vec![1],
            value: vec![3],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 123,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 456,
            },
        };
        assert!(matches!(
            source
                .start_source_marker_write::<()>(&group, resolved, || panic!(
                    "refused selector dispatched"
                ))
                .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        assert!(
            group
                .replay_unresolved_marker_write(&unresolved)
                .unwrap()
                .is_some()
        );
        group.forget_marker_write(0).unwrap();
    }

    #[test]
    fn preflight_refusal_is_retained_and_forget_allows_next_id() {
        let source = NotificationFaults::new(2);
        source.start_source_generation(1).unwrap();
        let (group, _) = source.open_source_group(1, 2, 128).unwrap();
        let mut request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![2],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 0,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 0,
            },
        };
        let receipt = group
            .record_marker_not_admitted(request.clone(), SourceGroupError::StaleGeneration)
            .unwrap();
        assert_eq!(
            receipt.status(),
            SourceWriteStatus::NotAdmitted(SourceGroupError::StaleGeneration)
        );
        assert!(matches!(
            source
                .start_source_marker_write::<()>(&group, request.clone(), || panic!(
                    "refused transaction dispatched"
                ))
                .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        group.forget_marker_write(0).unwrap();
        request.id = 1;
        assert!(matches!(
            source
                .start_source_marker_write(&group, request, || Ok(()))
                .unwrap(),
            SourceWriteStart::Initiated { .. }
        ));
    }

    #[tokio::test]
    async fn delayed_owner_delivery_keeps_admission_bounded_and_flushes_after_release() {
        use std::future::Future;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::task::{Context, Poll, Wake, Waker};
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let source = Arc::new(NotificationFaults::new(2));
        source.start_source_generation(1).unwrap();
        let (group, consumer) = source.open_source_group(1, 1, 8).unwrap();
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let task_source = source.clone();
        let task_lifecycle = lifecycle.clone();
        let owner = tokio::spawn(async move {
            let _delivery = task_source.hold_wake_delivery();
            let _guard = task_lifecycle.write().await;
            ready_tx.send(()).unwrap();
            finish_rx.await.unwrap();
        });
        ready_rx.await.unwrap();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = std::pin::pin!(consumer.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        source.source_group_barrier(&group, 1).unwrap();
        assert!(matches!(
            source.source_group_barrier(&group, 2),
            Err(SourceGroupError::Ingress(
                crate::ubm_peer_notification_ingress::IngressError::Terminated
            ))
        ));
        assert_eq!(count.0.load(Ordering::SeqCst), 0);
        finish_tx.send(()).unwrap();
        owner.await.unwrap();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            wait.as_mut().poll(&mut cx),
            Poll::Ready(Some(
                crate::ubm_peer_notification_ingress::IngressRecord::Barrier { id: 1 }
            ))
        ));
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Terminal {
                    reason: crate::ubm_peer_notification_ingress::TerminalReason::Overflow { .. },
                    ..
                }
            )
        ));
    }

    #[tokio::test]
    async fn cancelling_slow_owner_releases_guard_before_queued_wake() {
        use std::future::Future;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::task::{Context, Poll, Wake, Waker};
        struct Check {
            lifecycle: Arc<tokio::sync::RwLock<()>>,
            unlocked: AtomicBool,
        }
        impl Wake for Check {
            fn wake(self: Arc<Self>) {
                self.unlocked
                    .store(self.lifecycle.try_read().is_ok(), Ordering::SeqCst);
            }
        }
        let source = Arc::new(NotificationFaults::new(2));
        source.start_source_generation(1).unwrap();
        let (group, consumer) = source.open_source_group(1, 2, 8).unwrap();
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let other_source = source.clone();
        let other_lifecycle = lifecycle.clone();
        let task = tokio::spawn(async move {
            let _delivery = other_source.hold_wake_delivery();
            let _guard = other_lifecycle.write().await;
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        let check = Arc::new(Check {
            lifecycle,
            unlocked: AtomicBool::new(false),
        });
        let waker = Waker::from(check.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = std::pin::pin!(consumer.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        source.source_group_barrier(&group, 1).unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(check.unlocked.load(Ordering::SeqCst));
        assert!(matches!(
            wait.as_mut().poll(&mut cx),
            Poll::Ready(Some(
                crate::ubm_peer_notification_ingress::IngressRecord::Barrier { id: 1 }
            ))
        ));
    }

    #[test]
    fn detached_live_groups_bound_deferred_wake_obligations() {
        let faults = NotificationFaults::new(1);
        faults.start_source_generation(1).unwrap();
        let scope = faults.defer_wakes();
        let (group, consumer) = faults.open_source_group(1, 2, 8).unwrap();
        faults.source_group_barrier(&group, 1).unwrap();
        faults.detach_source_group(&group).unwrap();
        assert!(matches!(
            faults.open_source_group(1, 2, 8),
            Err(SourceGroupError::Capacity)
        ));
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Barrier { id: 1 }
            )
        ));
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Terminal { .. }
            )
        ));
        drop(consumer);
        assert!(faults.open_source_group(1, 2, 8).is_ok());
        drop(scope);
    }

    #[test]
    fn refused_group_admission_rolls_back_identity_and_preserves_old_consumer() {
        use crate::ubm_peer_notification_ingress::{IngressRecord, Poll, TerminalReason};

        let source = NotificationFaults::new(1);
        source.start_source_generation(1).unwrap();
        let (old, consumer) = source.open_source_group(1, 2, 8).unwrap();
        source.source_group_barrier(&old, 17).unwrap();
        source.detach_source_group(&old).unwrap();

        let next_id = source.lock_source_state().next_group;
        assert!(matches!(
            source.open_source_group(1, 2, 8),
            Err(SourceGroupError::Capacity)
        ));
        assert_eq!(source.lock_source_state().next_group, next_id);
        assert_eq!(
            consumer.poll(),
            Poll::Record(IngressRecord::Barrier { id: 17 })
        );
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Closed {
                    tail_uncertain: true
                },
                ..
            })
        ));
        drop(consumer);

        let (replacement, _) = source.open_source_group(1, 2, 8).unwrap();
        assert_eq!(replacement.id, next_id);
    }

    #[test]
    fn wake_scope_covers_groups_opened_while_nested() {
        use std::future::Future;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        use std::task::{Context, Poll, Wake, Waker};
        struct Count {
            count: AtomicUsize,
            outer: Arc<Mutex<()>>,
            unlocked: AtomicBool,
        }
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.unlocked
                    .store(self.outer.try_lock().is_ok(), Ordering::SeqCst);
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
        let source = Arc::new(NotificationFaults::new(8));
        source.start_source_generation(1).unwrap();
        let outer = source.defer_wakes();
        let owner = Arc::new(Mutex::new(()));
        let owner_guard = owner.lock().unwrap();
        let other = source.clone();
        let (group, consumer) =
            std::thread::spawn(move || other.open_source_group(1, 8, 128).unwrap())
                .join()
                .unwrap();
        let count = Arc::new(Count {
            count: AtomicUsize::new(0),
            outer: owner.clone(),
            unlocked: AtomicBool::new(false),
        });
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = std::pin::pin!(consumer.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        let inner = source.defer_wakes();
        source.source_group_barrier(&group, 7).unwrap();
        drop(inner);
        assert_eq!(count.count.load(Ordering::SeqCst), 0);
        drop(owner_guard);
        drop(outer);
        assert_eq!(count.count.load(Ordering::SeqCst), 1);
        assert!(count.unlocked.load(Ordering::SeqCst));
        assert!(matches!(
            wait.as_mut().poll(&mut cx),
            Poll::Ready(Some(
                crate::ubm_peer_notification_ingress::IngressRecord::Barrier { id: 7 }
            ))
        ));
    }

    #[test]
    fn value_and_fence_wake_after_source_lock_is_released() {
        use std::future::Future;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::task::{Context, Poll, Wake, Waker};
        struct CheckWake {
            source: Arc<NotificationFaults>,
            unlocked: AtomicBool,
        }
        impl Wake for CheckWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.unlocked
                    .store(self.source.state.try_lock().is_ok(), Ordering::SeqCst);
            }
        }
        let source = Arc::new(NotificationFaults::new(8));
        source.start_source_generation(1).unwrap();
        let (group, consumer) = source.open_source_group(1, 8, 128).unwrap();
        let value = note(1, false);
        let route = crate::ubm_peer_notification_ingress::NotificationRoute {
            service_uuid: value.service_uuid,
            service_instance: value.service_instance,
            characteristic_uuid: value.uuid,
            characteristic_instance: value.instance,
        };
        let membership = source.register_source_route(&group, route, 1).unwrap();
        let wake = Arc::new(CheckWake {
            source: source.clone(),
            unlocked: AtomicBool::new(false),
        });
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = std::pin::pin!(consumer.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        let other = source.clone();
        std::thread::spawn(move || {
            other
                .publish_source_event(1, crate::api::NotificationEvent::Value(value))
                .unwrap()
        })
        .join()
        .unwrap();
        assert!(wake.unlocked.load(Ordering::SeqCst));
        drop(wait);
        let _ = consumer.poll();
        wake.unlocked.store(false, Ordering::SeqCst);
        let mut wait = std::pin::pin!(consumer.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        source
            .retire_source_route(&group, route, membership)
            .unwrap();
        assert!(wake.unlocked.load(Ordering::SeqCst));
        drop(wait);
        let _ = consumer.poll();
        wake.unlocked.store(false, Ordering::SeqCst);
        let mut wait = std::pin::pin!(consumer.recv());
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        source.close_source_generation(1).unwrap();
        assert!(wake.unlocked.load(Ordering::SeqCst));
    }

    #[test]
    fn marker_write_replays_and_waits_for_settlement_before_forget() {
        let faults = NotificationFaults::new(8);
        faults.start_source_generation(1).unwrap();
        let (group, consumer) = faults.open_source_group(1, 8, 128).unwrap();
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![9],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 1,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 2,
            },
        };
        let SourceWriteStart::Initiated {
            receipt,
            operation,
            wake,
        } = faults
            .start_source_marker_write(&group, request.clone(), || {
                assert_eq!(
                    group
                        .replay_marker_write(&request)
                        .unwrap()
                        .unwrap()
                        .status(),
                    SourceWriteStatus::Admitted { ordinal: 0 }
                );
                Ok(17)
            })
            .unwrap()
        else {
            panic!("expected initiation")
        };
        assert_eq!(operation, 17);
        for consumer in wake {
            consumer.wake();
        }
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Marker { id: 0, ordinal: 0 }
            )
        ));
        assert!(matches!(
            faults
                .start_source_marker_write::<()>(&group, request, || panic!("redispatched"))
                .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        assert_eq!(
            faults.forget_source_marker_write(&group, 0),
            Err(SourceGroupError::TransactionConflict)
        );
        faults.close_source_generation(1).unwrap();
        assert!(
            faults
                .replay_source_marker_write(
                    &group,
                    &SourceWriteRequest {
                        selector: Default::default(),
                        id: 0,
                        graph: vec![1],
                        value: vec![9],
                        write_type: crate::api::WriteType::WithResponse,
                        route: crate::ubm_peer_notification_ingress::NotificationRoute {
                            service_uuid: Uuid::nil(),
                            service_instance: 1,
                            characteristic_uuid: Uuid::nil(),
                            characteristic_instance: 2,
                        },
                    }
                )
                .unwrap()
                .is_some()
        );
        receipt.settle(Ok(()));
        drop(faults);
        group.forget_marker_write(0).unwrap();
        group.forget_marker_write(0).unwrap();
    }

    #[test]
    fn rejected_marker_never_initiates_and_failed_initiation_keeps_marker() {
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![9],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 1,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 2,
            },
        };
        let faults = NotificationFaults::new(8);
        faults.start_source_generation(1).unwrap();
        let (full, _) = faults.open_source_group(1, 0, 128).unwrap();
        let SourceWriteStart::Rejected { receipt, wake } = faults
            .start_source_marker_write::<()>(&full, request.clone(), || {
                panic!("write after rejection")
            })
            .unwrap()
        else {
            panic!("expected rejection")
        };
        for consumer in wake {
            consumer.wake();
        }
        assert!(matches!(receipt.status(), SourceWriteStatus::Rejected(_)));
        assert!(matches!(
            faults
                .start_source_marker_write::<()>(&full, request.clone(), || panic!("redispatched"))
                .unwrap(),
            SourceWriteStart::Replay(_)
        ));
        let (group, consumer) = faults.open_source_group(1, 1, 128).unwrap();
        let error = crate::PlatformError::new("test", "failed", "failed");
        let SourceWriteStart::InitiationFailed { receipt, wake } = faults
            .start_source_marker_write::<()>(&group, request, || Err(error.clone()))
            .unwrap()
        else {
            panic!("expected failure")
        };
        for consumer in wake {
            consumer.wake();
        }
        assert!(
            matches!(receipt.status(), SourceWriteStatus::InitiationFailed { ordinal: 1, error: found } if found == error)
        );
        assert!(matches!(
            consumer.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Marker { id: 0, ordinal: 1 }
            )
        ));
    }

    #[test]
    fn exhausted_ordinal_retains_rejection_and_returns_all_deferred_wakes() {
        let faults = NotificationFaults::new(8);
        faults.start_source_generation(1).unwrap();
        let (group, first) = faults.open_source_group(1, 8, 128).unwrap();
        let (_, second) = faults.open_source_group(1, 8, 128).unwrap();
        faults.state.lock().unwrap().next_source_ordinal = None;
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: vec![1],
            value: vec![9],
            write_type: crate::api::WriteType::WithResponse,
            route: crate::ubm_peer_notification_ingress::NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 1,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 2,
            },
        };
        let SourceWriteStart::Rejected { receipt, wake } = faults
            .start_source_marker_write::<()>(&group, request.clone(), || {
                panic!("write after exhaustion")
            })
            .unwrap()
        else {
            panic!("expected rejection")
        };
        assert_eq!(
            receipt.status(),
            SourceWriteStatus::Rejected(SourceGroupError::OrdinalExhausted)
        );
        assert_eq!(wake.len(), 2);
        for consumer in wake {
            consumer.wake();
        }
        assert!(matches!(
            first.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Terminal {
                    reason:
                        crate::ubm_peer_notification_ingress::TerminalReason::MarkerOrdinalExhausted { id: 0, tail_uncertain: true },
                    ..
                }
            )
        ));
        assert!(matches!(
            second.poll(),
            crate::ubm_peer_notification_ingress::Poll::Record(
                crate::ubm_peer_notification_ingress::IngressRecord::Terminal {
                    reason:
                        crate::ubm_peer_notification_ingress::TerminalReason::OrdinalExhausted { .. },
                    ..
                }
            )
        ));
        assert_eq!(
            faults
                .replay_source_marker_write(&group, &request)
                .unwrap()
                .unwrap()
                .status(),
            receipt.status()
        );
    }

    include!("util_source_group_detach_tests.rs");
    include!("util_source_group_routing_tests.rs");
    include!("util_source_group_failure_tests.rs");
}
