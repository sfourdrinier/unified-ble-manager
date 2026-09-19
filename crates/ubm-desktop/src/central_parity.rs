//! Desktop parity operations on [`DesktopCentral`] (PR210 decision 7):
//! link security (state, pair, cancel pairing, unpair, change events),
//! adapter authorization, address targeting, address type and the per-mode
//! maximum write length.
//!
//! Each operation is admitted like [`DesktopCentral::read_rssi`]: refused
//! before any radio effect after shutdown, on a cancelled ticket or an
//! expired budget; the radio call runs under the caller budget or a named
//! liveness backstop. A radio that cannot answer reports
//! `capability.unsupported` through its [`RadioBoundary`] default, never a
//! plausible substitute.
//!
//! Pairing runs as a task on the shared executor so the ceremony — and any
//! pairing-generation hold around it — always finishes, even when its
//! caller stops waiting: a cancelled or expired pair asks the OS to cancel
//! the ceremony and then reports the ceremony's own answer.
//! [`DesktopCentral::cancel_pairing`] reads the same answer, so `pair` and
//! `cancel_pairing` cannot disagree about one ceremony.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::{Mutex, broadcast, watch};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CommitState};

use super::{
    CentralSignal, DesktopCentral, Inner, OpKind, PathSelector, Wait, classify, contract_error,
    drive, lock_std, timed_out,
};
use crate::boundary::{
    AdapterAuthorization, AddressType, BondState, PairOutcome, RadioBoundary, SecurityState,
    UnpairOutcome,
};
use crate::errors::{DesktopError, Retryability};
use crate::op_control::{LIVENESS_CLEANUP, LIVENESS_OP, OpControl, SettleOnDrop};
use ubm_core::central::ConnectionState;

/// One link-security change, published after a pair/unpair answered (or
/// the OS reported the change). `sequence` increases by one per security
/// event of this central.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityEvent {
    pub sequence: u64,
    pub peer_id: String,
    pub state: SecurityState,
}

/// One write-without-response readiness report for a connection (legacy
/// CoreBluetooth `onWriteWithoutResponseReadiness`). `connection_generation`
/// is read when the report arrives, so a consumer matches it against the
/// connection it watches; `sequence` increases by one per report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReadinessEvent {
    pub sequence: u64,
    pub peer_id: String,
    pub connection_generation: Option<String>,
    pub ready: bool,
}

/// A scan the OS ended without a stop request (legacy WinRT
/// `OnScanTerminal`, or the OS event source closing). `aborted` when the OS
/// reported an error; `detail` is the OS's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTerminalEvent {
    pub sequence: u64,
    pub operation_id: ubm_core::contracts::OperationId,
    pub aborted: bool,
    pub detail: String,
}

pub(super) async fn publish_write_readiness<B>(inner: &Inner<B>, peer_id: &str, ready: bool) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let connection_generation = match peer_key {
        Some(peer_key) => inner.core.lock().await.connection_generation(&peer_key),
        None => None,
    };
    let sequence = inner
        .write_readiness_sequence
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    // No receiver is not a failure: readiness stays readable through
    // `write_readiness`.
    let event = WriteReadinessEvent {
        sequence,
        peer_id: peer_id.to_owned(),
        connection_generation,
        ready,
    };
    let _ = inner.write_readiness.send(event.clone());
    // Finding 118: wake the host as the legacy readiness callback did.
    inner.signal(CentralSignal::WriteReadiness(event));
}

pub(super) fn publish_scan_terminal<B>(
    inner: &Inner<B>,
    operation_id: &ubm_core::contracts::OperationId,
    aborted: bool,
    detail: &str,
) {
    let sequence = inner.scan_terminal_sequence.fetch_add(1, Ordering::SeqCst) + 1;
    // No receiver is not a failure: the scan's own op already settled.
    let event = ScanTerminalEvent {
        sequence,
        operation_id: operation_id.clone(),
        aborted,
        detail: detail.to_owned(),
    };
    let _ = inner.scan_terminal.send(event.clone());
    // Finding 118: wake the host as the legacy scan-terminal callback did.
    inner.signal(CentralSignal::ScanTerminal(event));
}

/// A directed LE pairing generation (`secureConnections` other than
/// `prefer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecureConnections {
    Require,
    Disallow,
}

/// Adapter-level LE pairing generation a host-supplied controller reads
/// and sets (BlueZ mgmt `SET_SECURE_CONN`: off / on / only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairingGeneration {
    LegacyOnly,
    Enabled,
    Required,
}

impl PairingGeneration {
    /// Frozen wire string (legacy `BluezPairingGeneration`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyOnly => "legacy-only",
            Self::Enabled => "enabled",
            Self::Required => "required",
        }
    }
}

impl SecureConnections {
    /// The adapter generation that honors this direction.
    #[must_use]
    pub const fn generation(self) -> PairingGeneration {
        match self {
            Self::Require => PairingGeneration::Required,
            Self::Disallow => PairingGeneration::LegacyOnly,
        }
    }
}

/// Future a [`PairingGenerationController`] returns; `Err` carries the
/// controller's own failure text.
pub type ControllerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// A privileged operation the host supplies to read and set the adapter's
/// pairing generation. The package never escalates on its own: without a
/// controller a directed generation is `capability.unsupported`. Setting
/// the generation changes it for every pairing on that adapter until it is
/// restored; the central restores the previous generation after the
/// ceremony and counts a failed restore
/// ([`crate::ResourceCounters::generation_restore_failures`]).
pub trait PairingGenerationController: Send + Sync {
    fn read<'a>(&'a self, adapter_id: &'a str) -> ControllerFuture<'a, PairingGeneration>;
    fn set<'a>(
        &'a self,
        adapter_id: &'a str,
        generation: PairingGeneration,
    ) -> ControllerFuture<'a, ()>;
}

/// What a caller asks of one pairing.
#[derive(Clone, Default)]
pub struct PairRequest {
    /// `None` defers the generation to the platform (`prefer`).
    pub secure_connections: Option<SecureConnections>,
    /// The privileged generation controller, required for a directed
    /// generation.
    pub generation_controller: Option<Arc<dyn PairingGenerationController>>,
}

impl std::fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairRequest")
            .field("secure_connections", &self.secure_connections)
            .field(
                "generation_controller",
                &self.generation_controller.is_some(),
            )
            .finish()
    }
}

/// What a cancellation achieved, read from the pairing's own answer
/// (`cancelOutcomeForPairResult`): one fact, one word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelPairingOutcome {
    /// The ceremony ended cancelled.
    Cancelled,
    /// No ceremony was in flight for this peer.
    NotPairing,
    /// The ceremony won: a bond exists.
    Paired,
    /// The OS or the peer refused on its own.
    Rejected(Option<String>),
}

/// The cancellation outcome implied by a pairing's own result.
#[must_use]
pub fn cancel_outcome_for(outcome: &PairOutcome) -> CancelPairingOutcome {
    match outcome {
        PairOutcome::Paired(_) | PairOutcome::AlreadyPaired(_) => CancelPairingOutcome::Paired,
        PairOutcome::Rejected(reason) => CancelPairingOutcome::Rejected(reason.clone()),
        PairOutcome::Cancelled => CancelPairingOutcome::Cancelled,
    }
}

/// One ceremony's own answer, shared with `cancel_pairing`.
pub(super) type PairAnswer = Result<PairOutcome, DesktopError>;

/// Publish one security change on the broadcast. Zero receivers is not a
/// failure: the state stays readable through `security_state`.
pub(super) fn publish_security<B>(inner: &Inner<B>, peer_id: &str, state: SecurityState) {
    let sequence = inner.security_sequence.fetch_add(1, Ordering::SeqCst) + 1;
    let event = SecurityEvent {
        sequence,
        peer_id: peer_id.to_owned(),
        state,
    };
    let _ = inner.security.send(event.clone());
    // Finding 118: wake the host as the legacy bond-change callback did.
    inner.signal(CentralSignal::Security(event));
}

/// Per-adapter serialization of generation holds: two pairings never
/// interleave their read-set-restore sequences on one adapter.
fn adapter_hold_lock(adapter_id: &str) -> Arc<Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        locks
            .entry(adapter_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn security_failure(operation: &str, detail: impl Into<String>) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformSecurity,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(detail)
}

/// Hold the adapter at `target` around `ceremony`, then restore the
/// previous generation (legacy `withPairingGeneration`). A failed restore
/// is counted and reported; when the ceremony also failed, the restore
/// failure rides its detail.
pub(super) async fn with_pairing_generation<F>(
    controller: &dyn PairingGenerationController,
    adapter_id: &str,
    target: PairingGeneration,
    ceremony: F,
    restore_failures: &std::sync::atomic::AtomicU64,
) -> PairAnswer
where
    F: Future<Output = PairAnswer>,
{
    let lock = adapter_hold_lock(adapter_id);
    let _held = lock.lock().await;
    let previous = controller
        .read(adapter_id)
        .await
        .map_err(|detail| security_failure("security.pair.generation-read", detail))?;
    let held = previous != target;
    if held && let Err(detail) = controller.set(adapter_id, target).await {
        let restore = controller.set(adapter_id, previous).await.err();
        if let Some(restore_detail) = &restore {
            restore_failures.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "ubm-desktop: adapter {adapter_id} may be left at pairing generation {} \
                 because restoring {} failed: {restore_detail}",
                target.as_str(),
                previous.as_str()
            );
        }
        return Err(security_failure(
            "security.pair.generation-set",
            match restore {
                Some(restore_detail) => format!("{detail}; restore also failed: {restore_detail}"),
                None => detail,
            },
        ));
    }
    let answer = ceremony.await;
    if !held {
        return answer;
    }
    match controller.set(adapter_id, previous).await {
        Ok(()) => answer,
        Err(restore_detail) => {
            restore_failures.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "ubm-desktop: adapter {adapter_id} left at pairing generation {} because \
                 restoring {} failed: {restore_detail}",
                target.as_str(),
                previous.as_str()
            );
            answer.map_err(|error| {
                let detail = format!(
                    "{}; adapter left at pairing generation {} (restore failed: {restore_detail})",
                    error.detail().unwrap_or(error.operation()),
                    target.as_str()
                );
                error.with_detail(detail)
            })
        }
    }
}

/// Removes this ceremony's in-flight slot when the ceremony task ends.
struct PairingSlot<B> {
    inner: Arc<Inner<B>>,
    peer_id: String,
    answer: watch::Receiver<Option<PairAnswer>>,
}

impl<B> Drop for PairingSlot<B> {
    fn drop(&mut self) {
        let mut pairings = lock_std(&self.inner.pairings);
        if pairings
            .get(&self.peer_id)
            .is_some_and(|live| live.same_channel(&self.answer))
        {
            pairings.remove(&self.peer_id);
        }
    }
}

fn unknown_outcome(operation: &str, detail: impl Into<String>) -> DesktopError {
    contract_error(
        BleErrorCode::OperationTimedOut,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(detail)
    .with_outcome(Some(CommitState::Unknown), Retryability::Never)
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Subscribe to write-without-response readiness reports. Same lag
    /// rule as [`DesktopCentral::lifecycle_events`].
    #[must_use]
    pub fn write_readiness_events(&self) -> broadcast::Receiver<WriteReadinessEvent> {
        self.inner.write_readiness.subscribe()
    }

    /// Subscribe to scans the OS ended without a stop request. Same lag
    /// rule as [`DesktopCentral::lifecycle_events`].
    #[must_use]
    pub fn scan_terminal_events(&self) -> broadcast::Receiver<ScanTerminalEvent> {
        self.inner.scan_terminal.subscribe()
    }

    /// Whether the connection to `peer_id` can take a write without
    /// response now (`gatt:write-without-response-readiness`), for the
    /// lease holding it. Admitted like [`DesktopCentral::read_rssi`].
    pub async fn write_readiness(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<bool, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.write-readiness")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        self.require_connected_lease(&peer_key, lease, "gatt.write-readiness")
            .await?;
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.write_without_response_ready(peer_id),
        )
        .await
        {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("gatt.write-readiness", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("gatt.write-readiness"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// Subscribe to link-security changes. Same lag rule as
    /// [`DesktopCentral::lifecycle_events`].
    #[must_use]
    pub fn security_events(&self) -> broadcast::Receiver<SecurityEvent> {
        self.inner.security.subscribe()
    }

    /// Whether this process may use the adapter, as the OS reports it,
    /// under the budget ([`LIVENESS_OP`] without one). An adapter reset
    /// never ends the read: it answers the post-transition state (finding
    /// 94).
    pub async fn adapter_authorization(
        &self,
        ctl: OpControl,
    ) -> Result<AdapterAuthorization, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck_adapter_read(&ctl, "adapter.authorization")?;
        let window = ctl.budget.window(LIVENESS_OP);
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.adapter_authorization(),
        )
        .await
        {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("adapter.authorization", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("adapter.authorization"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// Link-security facts for a known peer, as the OS reports them.
    pub async fn security_state(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<SecurityState, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "security.state")?;
        let window = ctl.budget.window(LIVENESS_OP);
        self.known_peer_key(peer_id).await?;
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.security_state(peer_id),
        )
        .await
        {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("security.state", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("security.state"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// Pair with a known peer through the OS ceremony. One ceremony per
    /// peer at a time (`ownership.denied` otherwise). A directed
    /// generation needs [`PairRequest::generation_controller`]; without one
    /// it is `capability.unsupported` before any effect. A cancel or an
    /// expired budget asks the OS to cancel the ceremony, then reports the
    /// ceremony's own answer (`Cancelled`, or `Paired` when the bond won
    /// the race); if the ceremony has not answered within
    /// [`LIVENESS_CLEANUP`] after that, the bond state is unknown and the
    /// error says so (commit `unknown`, never retryable).
    pub async fn pair(
        &self,
        peer_id: &str,
        request: PairRequest,
        ctl: OpControl,
    ) -> Result<PairOutcome, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "security.pair")?;
        // Finding 123: without a caller budget the ceremony waits as long as
        // the OS does (a passkey dialog, a system prompt), as every legacy
        // host did; a cancel of the pairing ends it.
        let window = ctl.budget.window_without_backstop();
        self.known_peer_key(peer_id).await?;
        let hold = match (request.secure_connections, request.generation_controller) {
            (None, _) => None,
            (Some(direction), controller) => {
                // The registered capability is the runtime truth: a profile
                // that did not register `security:pairing-generation`
                // refuses a directed generation even with a controller.
                self.inner
                    .core
                    .lock()
                    .await
                    .check_capability(
                        "security:pairing-generation",
                        "security.pair.secure-connections",
                    )
                    .map_err(DesktopError::from)?;
                let Some(controller) = controller else {
                    return Err(contract_error(
                        BleErrorCode::CapabilityUnsupported,
                        BleErrorDomain::Capability,
                        "security.pair.secure-connections",
                    )
                    .with_detail(
                        "directing the pairing generation needs a host-supplied \
                         pairing-generation controller",
                    ));
                };
                Some((controller, direction.generation()))
            }
        };
        let (answer_tx, answer_rx) = watch::channel(None);
        {
            let mut pairings = lock_std(&self.inner.pairings);
            if pairings.contains_key(peer_id) {
                return Err(contract_error(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Platform,
                    "security.pair.arbitration",
                )
                .with_detail("a pairing with this peer is already in flight"));
            }
            pairings.insert(peer_id.to_owned(), answer_rx.clone());
        }
        let slot = PairingSlot {
            inner: Arc::clone(&self.inner),
            peer_id: peer_id.to_owned(),
            answer: answer_rx,
        };
        let adapter_id = lock_std(&self.inner.attachment)
            .adapter_id()
            .as_str()
            .to_owned();
        let peer = peer_id.to_owned();
        let mut ceremony = tokio::spawn(async move {
            let inner = Arc::clone(&slot.inner);
            let answer = match hold {
                None => inner.boundary.pair(&peer).await,
                Some((controller, generation)) => {
                    with_pairing_generation(
                        controller.as_ref(),
                        &adapter_id,
                        generation,
                        inner.boundary.pair(&peer),
                        &inner.generation_restore_failures,
                    )
                    .await
                }
            };
            if !inner.boundary.reports_security_changes()
                && let Ok(PairOutcome::Paired(state) | PairOutcome::AlreadyPaired(state)) = &answer
            {
                publish_security(&inner, &peer, *state);
            }
            let _ = answer_tx.send(Some(answer.clone()));
            drop(slot);
            answer
        });
        let joined = |outcome: Result<PairAnswer, tokio::task::JoinError>| match outcome {
            Ok(answer) => answer,
            Err(join) => Err(contract_error(
                BleErrorCode::LifecycleInvariantViolation,
                BleErrorDomain::Core,
                "security.pair",
            )
            .with_detail(format!("pairing task failed: {join}"))
            .with_outcome(Some(CommitState::Unknown), Retryability::Never)),
        };
        match drive(&ctl.ticket, window, &mut ceremony).await {
            Wait::Done(outcome) => return joined(outcome),
            Wait::Expired | Wait::Cancelled => {}
        }
        // Cancelled or expired: ask the OS to stop the ceremony, then read
        // the ceremony's own answer. A refused cancel is not the answer:
        // the ceremony may still end either way.
        let cancel = tokio::time::timeout(
            LIVENESS_CLEANUP,
            self.inner.boundary.cancel_pairing(peer_id),
        )
        .await;
        match tokio::time::timeout(LIVENESS_CLEANUP, &mut ceremony).await {
            Ok(outcome) => joined(outcome),
            Err(_) => {
                let cancel_note = match cancel {
                    Ok(Ok(())) => "the OS accepted the cancellation".to_owned(),
                    Ok(Err(error)) => format!(
                        "the OS refused the cancellation ({})",
                        error.detail().unwrap_or(error.code_str())
                    ),
                    Err(_) => "the cancellation did not answer".to_owned(),
                };
                Err(unknown_outcome(
                    "security.pair",
                    format!(
                        "pairing stopped waiting; {cancel_note}, and the ceremony has not \
                         answered: the bond state is unknown"
                    ),
                ))
            }
        }
    }

    /// Cancel the in-flight pairing with `peer_id` and report what the
    /// ceremony ended as ([`cancel_outcome_for`]). `NotPairing` when none
    /// is in flight.
    pub async fn cancel_pairing(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<CancelPairingOutcome, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "security.cancel-pairing")?;
        // Finding 123: the OS cancellation and the ceremony's own answer are
        // awaited without a backstop, as the legacy hosts did.
        let window = ctl.budget.window_without_backstop();
        let Some(mut answer) = lock_std(&self.inner.pairings).get(peer_id).cloned() else {
            return Ok(CancelPairingOutcome::NotPairing);
        };
        let settled = answer.borrow().clone();
        if let Some(settled) = settled {
            return settled.map(|outcome| cancel_outcome_for(&outcome));
        }
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.cancel_pairing(peer_id),
        )
        .await
        {
            Wait::Done(Ok(())) => {}
            Wait::Done(Err(error)) => return Err(error),
            Wait::Expired => {
                return Err(classify(
                    timed_out("security.cancel-pairing", window),
                    OpKind::Cleanup,
                    true,
                ));
            }
            Wait::Cancelled => {
                return Err(classify(
                    ctl.ticket.interruption("security.cancel-pairing"),
                    OpKind::Cleanup,
                    true,
                ));
            }
        }
        let own_answer = async move {
            answer
                .wait_for(Option::is_some)
                .await
                .map(|settled| settled.clone())
        };
        match drive(&ctl.ticket, window, own_answer).await {
            Wait::Done(Ok(Some(settled))) => settled.map(|outcome| cancel_outcome_for(&outcome)),
            Wait::Done(Ok(None) | Err(_)) => Err(unknown_outcome(
                "security.cancel-pairing",
                "the pairing ended without an answer",
            )),
            Wait::Expired => Err(unknown_outcome(
                "security.cancel-pairing",
                "the OS accepted the cancellation, but the ceremony has not answered",
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("security.cancel-pairing"),
                OpKind::Cleanup,
                true,
            )),
        }
    }

    /// Remove the OS bond with a known peer. Refused while a pairing with
    /// that peer is in flight.
    pub async fn unpair(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<UnpairOutcome, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "security.unpair")?;
        let window = ctl.budget.window(LIVENESS_OP);
        self.known_peer_key(peer_id).await?;
        if lock_std(&self.inner.pairings).contains_key(peer_id) {
            return Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Platform,
                "security.unpair.arbitration",
            )
            .with_detail("a pairing with this peer is in flight"));
        }
        let outcome = match drive(&ctl.ticket, window, self.inner.boundary.unpair(peer_id)).await {
            Wait::Done(outcome) => outcome?,
            Wait::Expired => {
                return Err(unknown_outcome(
                    "security.unpair",
                    "the unpair did not answer inside the budget; the bond state is unknown",
                ));
            }
            Wait::Cancelled => {
                return Err(contract_error(
                    BleErrorCode::OperationAborted,
                    BleErrorDomain::Platform,
                    "security.unpair",
                )
                .with_detail("the unpair was dispatched; the bond state is unknown")
                .with_outcome(Some(CommitState::Unknown), Retryability::Never));
            }
        };
        if !self.inner.boundary.reports_security_changes() {
            // The unpair's own answer is the bond fact; the OS read adds
            // whether pairing is possible when it answers inside the budget.
            let read = drive(
                &ctl.ticket,
                window,
                self.inner.boundary.security_state(peer_id),
            )
            .await;
            let state = match read {
                Wait::Done(Ok(state)) => state,
                _ => SecurityState {
                    bond: BondState::NotBonded,
                    pairing_possible: None,
                },
            };
            publish_security(&self.inner, peer_id, state);
        }
        Ok(outcome)
    }

    /// Resolve an out-of-band LE address to a radio peer id and make it a
    /// known peer (`peer:address-targeting`). The address must be six
    /// colon-separated hex octets; anything else is `argument.invalid`
    /// before any radio effect.
    pub async fn resolve_address(
        &self,
        address: &str,
        address_type: AddressType,
        ctl: OpControl,
    ) -> Result<String, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "peer.address-targeting")?;
        let canonical = canonical_address(address).ok_or_else(|| {
            contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "peer.address-targeting",
            )
            .with_detail("an LE address is six colon-separated hex octets")
        })?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_id = match drive(
            &ctl.ticket,
            window,
            self.inner
                .boundary
                .resolve_address(&canonical, address_type),
        )
        .await
        {
            Wait::Done(outcome) => outcome?,
            Wait::Expired => {
                return Err(classify(
                    timed_out("peer.address-targeting", window),
                    OpKind::Read,
                    true,
                ));
            }
            Wait::Cancelled => {
                return Err(classify(
                    ctl.ticket.interruption("peer.address-targeting"),
                    OpKind::Read,
                    true,
                ));
            }
        };
        let peer_key = {
            let mut core = self.inner.core.lock().await;
            core.resolve_peer("platform-guid", &peer_id)
                .map_err(DesktopError::from)?
        };
        self.inner
            .peers
            .lock()
            .await
            .insert(peer_id.clone(), peer_key);
        Ok(peer_id)
    }

    /// LE address type of a known peer, or `None` when the OS does not say.
    pub async fn address_type(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<Option<AddressType>, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "peer.address-type")?;
        let window = ctl.budget.window(LIVENESS_OP);
        self.known_peer_key(peer_id).await?;
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.address_type(peer_id),
        )
        .await
        {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("peer.address-type", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("peer.address-type"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// The largest single write the link to `peer_id` accepts for one mode
    /// (`gatt:maximum-write-length`) — the same limit
    /// [`DesktopCentral::write`] enforces. Admitted like
    /// [`DesktopCentral::read_rssi`]; an unmeasured limit is
    /// `capability.unavailable`, never a guess. `selector` must address a
    /// characteristic of the current database.
    pub async fn maximum_write_length(
        &self,
        peer_id: &str,
        lease: &str,
        selector: &PathSelector,
        with_response: bool,
        ctl: OpControl,
    ) -> Result<u64, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.maximum-write-length")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        self.require_connected_lease(&peer_key, lease, "gatt.maximum-write-length")
            .await?;
        let measured = self
            .measured_write_limit(
                peer_id,
                with_response,
                &ctl.ticket,
                window,
                "gatt.maximum-write-length",
            )
            .await?;
        let core = self.inner.core.lock().await;
        Self::resolve_instance(
            &core,
            &peer_key,
            peer_id,
            selector,
            "gatt.maximum-write-length",
            false,
        )?;
        Self::write_maximum(&core, measured, "gatt.maximum-write-length")
    }

    /// The largest single write the OS accepts on the link to `peer_id`,
    /// per mode, without naming a characteristic and without discovery
    /// (finding 65; legacy CoreBluetooth `maximumWriteValueLengthForType:`
    /// on the peripheral). Admitted like
    /// [`DesktopCentral::maximum_write_length`]: the lease holder of a
    /// connected link. An unmeasured limit is `capability.unavailable`,
    /// never a guessed 20.
    pub async fn connection_maximum_write_length(
        &self,
        peer_id: &str,
        lease: &str,
        with_response: bool,
        ctl: OpControl,
    ) -> Result<u64, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "gatt.maximum-write-length")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        self.require_connected_lease(&peer_key, lease, "gatt.maximum-write-length")
            .await?;
        let measured = self
            .measured_write_limit(
                peer_id,
                with_response,
                &ctl.ticket,
                window,
                "gatt.maximum-write-length",
            )
            .await?;
        let core = self.inner.core.lock().await;
        Self::write_maximum(&core, measured, "gatt.maximum-write-length")
    }

    /// Effective ATT MTU of the live link to `peer_id`
    /// (`connection:effective-mtu`): the ATT MTU the OS negotiated, as the
    /// OS reports it. macOS derives
    /// `CBPeripheral.maximumWriteValueLength(for: .withResponse) + 3` per
    /// link (finding 217: the same derivation as the Apple React Native
    /// route, so both hosts report the same value); Windows reads the
    /// `GattSession.MaxPduSize` btleplug already tracks as the ATT MTU;
    /// Linux reads the `org.bluez.GattCharacteristic1` MTU. Admitted like
    /// [`DesktopCentral::connection_maximum_write_length`]: the lease
    /// holder of a connected link. A radio that withholds the measurement
    /// answers `capability.unsupported` with the reason, never a guessed
    /// 23.
    pub async fn read_effective_mtu(
        &self,
        peer_id: &str,
        lease: &str,
        ctl: OpControl,
    ) -> Result<u16, DesktopError> {
        let _settle = SettleOnDrop(&ctl.ticket);
        self.precheck(&ctl, "connection.effective-mtu")?;
        let window = ctl.budget.window(LIVENESS_OP);
        let peer_key = self.known_peer_key(peer_id).await?;
        self.require_connected_lease(&peer_key, lease, "connection.effective-mtu")
            .await?;
        match drive(
            &ctl.ticket,
            window,
            self.inner.boundary.read_effective_mtu(peer_id),
        )
        .await
        {
            Wait::Done(outcome) => outcome,
            Wait::Expired => Err(classify(
                timed_out("connection.effective-mtu", window),
                OpKind::Read,
                true,
            )),
            Wait::Cancelled => Err(classify(
                ctl.ticket.interruption("connection.effective-mtu"),
                OpKind::Read,
                true,
            )),
        }
    }

    /// Lease admission shared with [`DesktopCentral::read_rssi`]: no record
    /// is `connection.not-found`, a foreign lease `ownership.denied`, a link
    /// that is not connected `connection.stale`.
    async fn require_connected_lease(
        &self,
        peer_key: &str,
        lease: &str,
        operation: &'static str,
    ) -> Result<(), DesktopError> {
        let core = self.inner.core.lock().await;
        match core.connection_state(peer_key) {
            None => Err(contract_error(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                operation,
            )),
            Some(_) if !core.holds_lease(peer_key, lease) => Err(contract_error(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                operation,
            )),
            Some(ConnectionState::Connected) => Ok(()),
            Some(_) => Err(contract_error(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                operation,
            )),
        }
    }
}

/// `AA:BB:CC:DD:EE:FF` (upper-case) for a well-formed LE address.
#[must_use]
pub fn canonical_address(address: &str) -> Option<String> {
    let octets: Vec<&str> = address.split(':').collect();
    if octets.len() != 6
        || octets
            .iter()
            .any(|octet| octet.len() != 2 || !octet.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(address.to_ascii_uppercase())
}
