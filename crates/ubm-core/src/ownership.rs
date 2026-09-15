//! Transition kernel: one owner for operations, leases, and cleanup.
//!
//! Derived from C-UBM `hosts.ts` (arbitration), `transitions.ts` (operation
//! machine), `effects.ts` (arbitration/commit), `cleanup.ts` (receipts), and
//! the BLE Rust convergence plan execution model (plan section 7) plus the
//! CORE-KERNEL card: scoped IDs, generations, leases, queued/admitted
//! operations, monotonic deadlines, cancellation-phase receipts, and retained
//! cleanup. State maps, effect batches, and retained records are bounded; long
//! work advances in bounded sweeps and yields when budgets run out.
//!
//! Lock discipline: [`Kernel::handle`] takes `&mut self` for one input, emits
//! effects into the caller-provided [`EffectBatch`], and returns. The host
//! executes those effects after the borrow ends — foreign code never runs
//! while core state is mutably borrowed, and synchronous callback reentrancy
//! is normalized by feeding results back as new inputs with generations.
//!
//! Admission order per input: shutdown gate, handshake gate (PKG-02/OPS-01),
//! ownership/attachment verification (OWN-02) before admission, capacity and
//! per-owner bounds, then deadline computation. Every early return leaves
//! state unchanged; mutating inputs first prove effect-batch space so a
//! settlement is never half-emitted.
//!
//! Effect kinds mirror the C-UBM families plus `timer.cancel`, which the plan
//! (§7.2 minimum effect families) requires explicitly.

use crate::contracts::{
    AttachmentTuple, BleErrorCode, BleErrorDomain, CommitState, CompletionTerminal, Contender,
    ContenderKind, CoreError, Generation, HandshakeState, LeaseId, MonotonicTime, OperationId,
    OperationTerminalKind, TerminalRecord, assert_handshake_complete, assert_same_attachment,
    assert_timeout_ms, commit_for, is_deadline_expired, is_generation_current, paths_invalid_for,
    reached_radio_for, terminal_for_winner, to_deadline,
};

/// Kernel-local admission bound on live operations (queued + dispatched +
/// terminal awaiting release report). Documented as kernel-local: C-UBM
/// freezes per-resource counters, not this aggregate.
pub const DEFAULT_MAX_OPERATIONS: usize = 256;
/// Kernel-local bound on effects appended by one [`Kernel::handle`] call.
pub const DEFAULT_MAX_EFFECTS_PER_CALL: usize = 64;
/// Kernel-local bound on live operations per owner lease.
pub const DEFAULT_MAX_OPERATIONS_PER_OWNER: usize = 8;

/// Kernel configuration. All bounds are explicit so hosts size the kernel to
/// their budgets; admission-boundary tests use small values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelConfig {
    /// Maximum live operations.
    pub max_operations: usize,
    /// Maximum effects appended per `handle` call.
    pub max_effects_per_call: usize,
    /// Maximum live operations per owner lease.
    pub max_operations_per_owner: usize,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            max_operations: DEFAULT_MAX_OPERATIONS,
            max_effects_per_call: DEFAULT_MAX_EFFECTS_PER_CALL,
            max_operations_per_owner: DEFAULT_MAX_OPERATIONS_PER_OWNER,
        }
    }
}

impl KernelConfig {
    /// Validate and build a configuration. Zero bounds admit nothing, so they
    /// are rejected instead of silently wedging the kernel.
    pub fn new(
        max_operations: usize,
        max_effects_per_call: usize,
        max_operations_per_owner: usize,
    ) -> Result<Self, CoreError> {
        if max_operations == 0 || max_effects_per_call == 0 || max_operations_per_owner == 0 {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "kernel.config.bounds",
            ));
        }
        Ok(Self {
            max_operations,
            max_effects_per_call,
            max_operations_per_owner,
        })
    }
}

/// Concrete effect the host must execute outside the core borrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    kind: EffectKind,
    operation_id: OperationId,
    detail: String,
}

impl Effect {
    /// Borrow the effect kind.
    #[must_use]
    pub const fn kind(&self) -> EffectKind {
        self.kind
    }

    /// Borrow the owning operation id.
    #[must_use]
    pub const fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Borrow the human-readable detail (never a secret or raw payload).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Effect family. The first five mirror C-UBM `EffectKind`; `TimerCancel`
/// comes from the plan §7.2 minimum families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectKind {
    RadioDispatch,
    TimerSchedule,
    TimerCancel,
    StatePublish,
    CleanupRelease,
    ObservationDeliver,
}

impl EffectKind {
    /// Frozen wire string for this effect kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RadioDispatch => "radio.dispatch",
            Self::TimerSchedule => "timer.schedule",
            Self::TimerCancel => "timer.cancel",
            Self::StatePublish => "state.publish",
            Self::CleanupRelease => "cleanup.release",
            Self::ObservationDeliver => "observation.deliver",
        }
    }
}

/// Bounded batch of effects appended by one `handle` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectBatch {
    effects: Vec<Effect>,
    capacity: usize,
}

impl EffectBatch {
    /// Build an empty batch holding at most `capacity` effects.
    pub fn new(capacity: usize) -> Self {
        Self {
            effects: Vec::new(),
            capacity,
        }
    }

    /// Append an effect. A full batch fails as `stream.quota`: bounded queues
    /// never grow because a consumer stops reading.
    pub fn push(&mut self, effect: Effect) -> Result<(), CoreError> {
        if self.effects.len() >= self.capacity {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "effect-batch.full",
            ));
        }
        self.effects.push(effect);
        Ok(())
    }

    /// Number of staged effects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.effects.len()
    }

    /// Whether the batch holds no effects.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// Maximum effects this batch holds.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Free slots remaining.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.effects.len())
    }

    /// Borrow staged effects in order.
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    /// Take staged effects, leaving the batch empty.
    #[must_use]
    pub fn drain(&mut self) -> Vec<Effect> {
        core::mem::take(&mut self.effects)
    }
}

/// Cancellation phase for a cancel receipt: before or after radio dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelPhase {
    BeforeDispatch,
    AfterDispatch,
}

impl CancelPhase {
    /// Frozen wire string for this phase.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeDispatch => "before-dispatch",
            Self::AfterDispatch => "after-dispatch",
        }
    }
}

/// How an operation settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettlePhase {
    Contended,
    Cancelled(CancelPhase),
    Expired,
    Shutdown,
}

/// Settlement receipt: exactly one terminal outcome plus the arbitration
/// facts (OPS-02 commit state, radio reach, path validity, phase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementReceipt {
    operation_id: OperationId,
    kind: OperationTerminalKind,
    cause: Option<BleErrorCode>,
    ingress_ordinal: u64,
    started_at: MonotonicTime,
    settled_at: MonotonicTime,
    reached_radio: bool,
    paths_invalid_before_settlement: bool,
    commit_state: CommitState,
    phase: SettlePhase,
    suppressed_duplicates: u64,
}

impl SettlementReceipt {
    /// Borrow the operation id.
    #[must_use]
    pub const fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Terminal kind.
    #[must_use]
    pub const fn kind(&self) -> OperationTerminalKind {
        self.kind
    }

    /// Cause code (`None` only for `Succeeded`).
    #[must_use]
    pub const fn cause(&self) -> Option<BleErrorCode> {
        self.cause
    }

    /// Winning contender ordinal.
    #[must_use]
    pub const fn ingress_ordinal(&self) -> u64 {
        self.ingress_ordinal
    }

    /// Admission instant.
    #[must_use]
    pub const fn started_at(&self) -> MonotonicTime {
        self.started_at
    }

    /// Settlement instant.
    #[must_use]
    pub const fn settled_at(&self) -> MonotonicTime {
        self.settled_at
    }

    /// Whether the winner reached the radio.
    #[must_use]
    pub const fn reached_radio(&self) -> bool {
        self.reached_radio
    }

    /// Whether paths went invalid before settlement (host must invalidate).
    #[must_use]
    pub const fn paths_invalid_before_settlement(&self) -> bool {
        self.paths_invalid_before_settlement
    }

    /// Radio commit disposition.
    #[must_use]
    pub const fn commit_state(&self) -> CommitState {
        self.commit_state
    }

    /// How the operation settled.
    #[must_use]
    pub const fn phase(&self) -> SettlePhase {
        self.phase
    }

    /// Late callbacks suppressed after settlement at receipt build time.
    #[must_use]
    pub const fn suppressed_duplicates(&self) -> u64 {
        self.suppressed_duplicates
    }

    /// Project this receipt onto the frozen terminal record.
    pub fn terminal_record(&self) -> Result<TerminalRecord, CoreError> {
        TerminalRecord::new(
            self.operation_id.clone(),
            self.kind,
            self.cause,
            self.ingress_ordinal,
            self.started_at,
            self.settled_at,
        )
    }
}

/// Outcome of one [`Kernel::handle`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleOutcome {
    Admitted {
        deadline: MonotonicTime,
    },
    Dispatched,
    Settled {
        receipt: SettlementReceipt,
    },
    Cancelled {
        receipt: SettlementReceipt,
    },
    /// An invalid contender on a live operation: ignored, state unchanged.
    ContenderIgnored,
    /// Input for an already-terminal operation (late callback): suppressed,
    /// state unchanged, no effects.
    DuplicateSuppressed,
    Swept {
        settled: usize,
        truncated: bool,
    },
    ReleaseRecorded {
        reaped: bool,
    },
    ShutDown {
        settled_queued: usize,
        retained: usize,
        truncated: bool,
    },
}

/// Validated input to the transition kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelInput {
    Admit {
        operation_id: OperationId,
        owner: LeaseId,
        attachment: AttachmentTuple,
        generation: Generation,
        timeout_ms: u64,
    },
    Dispatch {
        operation_id: OperationId,
        generation: Generation,
    },
    Complete {
        operation_id: OperationId,
        generation: Generation,
        contender: Contender,
    },
    Cancel {
        operation_id: OperationId,
        generation: Generation,
    },
    /// Settle queued operations whose deadline has passed at `now`.
    ExpireSweep,
    /// Host reports a physical release result for a terminal operation and
    /// the kernel reaps it. `ok == false` requires `code` and retains a
    /// failure record: a failed release is never reported as success.
    ReleaseReport {
        operation_id: OperationId,
        ok: bool,
        code: Option<BleErrorCode>,
    },
    /// Stop admission, settle queued work as destroyed, request release of
    /// dispatched work, and retain cleanup. Repeat until `truncated` is false.
    Shutdown,
}

/// Cleanup state for one retained record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupState {
    Released,
    ReleaseFailed,
}

/// One retained cleanup failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupFailure {
    resource_kind: String,
    code: BleErrorCode,
}

impl CleanupFailure {
    /// Borrow the resource kind.
    #[must_use]
    pub fn resource_kind(&self) -> &str {
        &self.resource_kind
    }

    /// Borrow the failure code.
    #[must_use]
    pub const fn code(&self) -> BleErrorCode {
        self.code
    }
}

/// Retained cleanup record: batch cleanup continues after an individual
/// failure and reports one composite record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRecord {
    operation_id: Option<OperationId>,
    state: CleanupState,
    failures: Vec<CleanupFailure>,
}

impl CleanupRecord {
    /// A successful release carries zero failures; a failed release carries
    /// one or more. Mixed states are rejected instead of constructed.
    pub fn new(
        operation_id: Option<OperationId>,
        state: CleanupState,
        failures: Vec<CleanupFailure>,
    ) -> Result<Self, CoreError> {
        let consistent = match state {
            CleanupState::Released => failures.is_empty(),
            CleanupState::ReleaseFailed => !failures.is_empty(),
        };
        if !consistent {
            return Err(CoreError::new(
                BleErrorCode::ProtocolMalformed,
                BleErrorDomain::Boundary,
                "cleanup.record",
            ));
        }
        Ok(Self {
            operation_id,
            state,
            failures,
        })
    }

    /// Borrow the owning operation, if any.
    #[must_use]
    pub const fn operation_id(&self) -> Option<&OperationId> {
        self.operation_id.as_ref()
    }

    /// Cleanup state.
    #[must_use]
    pub const fn state(&self) -> CleanupState {
        self.state
    }

    /// Borrow retained failures.
    #[must_use]
    pub fn failures(&self) -> &[CleanupFailure] {
        &self.failures
    }
}

/// Ownership decision for one physical resource (OWN-01), mirroring C-UBM
/// `hosts.ts` arbitration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipDecision {
    GrantPhysical,
    GrantLease { lease: String },
    Reject { code: BleErrorCode },
}

/// One physical scan controller. A second non-shared request fails without
/// changing the first; an explicitly shared request receives an independently
/// bounded stream lease.
#[must_use]
pub fn arbitrate_scan_request(
    physical_active: bool,
    share_token: Option<&str>,
) -> OwnershipDecision {
    if !physical_active {
        return OwnershipDecision::GrantPhysical;
    }
    match share_token {
        Some(token) if !token.is_empty() => {
            let mut lease = String::with_capacity("scan-share:".len() + token.len());
            lease.push_str("scan-share:");
            lease.push_str(token);
            OwnershipDecision::GrantLease { lease }
        }
        _ => OwnershipDecision::Reject {
            code: BleErrorCode::ScanAlreadyActive,
        },
    }
}

/// Shared-link arbitration: multiple leases share one physical link only when
/// the backend reports sharing support; otherwise the second request fails.
#[must_use]
pub fn arbitrate_connection_request(
    sharing_supported: bool,
    existing_leases: u64,
) -> OwnershipDecision {
    if existing_leases == 0 {
        return OwnershipDecision::GrantPhysical;
    }
    if sharing_supported {
        let mut lease = String::from("connection-lease:");
        append_u64(&mut lease, existing_leases.saturating_add(1));
        OwnershipDecision::GrantLease { lease }
    } else {
        OwnershipDecision::Reject {
            code: BleErrorCode::ConnectionAlreadyOwned,
        }
    }
}

fn append_u64(into: &mut String, mut value: u64) {
    if value == 0 {
        into.push('0');
        return;
    }
    let mut digits: [u8; 20] = [0; 20];
    let mut count = 0usize;
    while value > 0 && count < digits.len() {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    while count > 0 {
        count -= 1;
        into.push(digits[count] as char);
    }
}

/// Observable lifecycle of one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpStateView {
    Queued,
    Dispatched,
    Terminal(OperationTerminalKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OpLifecycle {
    Queued,
    Dispatched,
    Terminal { kind: OperationTerminalKind },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpEntry {
    id: OperationId,
    owner: LeaseId,
    generation: Generation,
    state: OpLifecycle,
    deadline: MonotonicTime,
    started_at: MonotonicTime,
    suppressed: u64,
    winner_ordinal: Option<u64>,
    release_requested: bool,
}

struct AdmitRequest {
    operation_id: OperationId,
    owner: LeaseId,
    attachment: AttachmentTuple,
    generation: Generation,
    timeout_ms: u64,
    now: MonotonicTime,
}

#[derive(Clone, Copy)]
struct SettleRequest<'a> {
    kind: ContenderKind,
    cause: Option<BleErrorCode>,
    ordinal: u64,
    now: MonotonicTime,
    phase: SettlePhase,
    effects: &'a [EffectKind],
}

/// One transition owner: scoped IDs, generations, leases, queued/admitted
/// operations, monotonic deadlines, cancellation-phase receipts, and retained
/// cleanup. Operations live in a bounded insertion-ordered map; every
/// emission path proves effect-batch space before mutating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    config: KernelConfig,
    attachment: AttachmentTuple,
    generation: Generation,
    handshake: HandshakeState,
    admission_open: bool,
    ops: Vec<OpEntry>,
    cleanup_retained: Vec<CleanupRecord>,
    authority: u64,
}

impl Kernel {
    /// Build a kernel bound to one attachment scope and generation. Work
    /// starts only after the handshake completes (PKG-02).
    pub fn new(
        config: KernelConfig,
        attachment: AttachmentTuple,
        generation: Generation,
        handshake: HandshakeState,
    ) -> Self {
        Self {
            config,
            attachment,
            generation,
            handshake,
            admission_open: true,
            ops: Vec::new(),
            cleanup_retained: Vec::new(),
            authority: 0,
        }
    }

    /// Whether admission is still open (false after [`KernelInput::Shutdown`]).
    #[must_use]
    pub const fn admission_open(&self) -> bool {
        self.admission_open
    }

    /// Number of live operations (queued, dispatched, or terminal awaiting a
    /// release report).
    #[must_use]
    pub fn live_operation_count(&self) -> usize {
        self.ops.len()
    }

    /// Number of retained cleanup records awaiting drain.
    #[must_use]
    pub fn retained_cleanup_count(&self) -> usize {
        self.cleanup_retained.len()
    }

    /// Observable lifecycle of one operation, if present.
    #[must_use]
    pub fn operation_state(&self, id: &OperationId) -> Option<OpStateView> {
        self.find(id).map(|entry| match &entry.state {
            OpLifecycle::Queued => OpStateView::Queued,
            OpLifecycle::Dispatched => OpStateView::Dispatched,
            OpLifecycle::Terminal { kind } => OpStateView::Terminal(*kind),
        })
    }

    /// Late callbacks suppressed for one operation, if present.
    #[must_use]
    pub fn suppressed_count(&self, id: &OperationId) -> Option<u64> {
        self.find(id).map(|entry| entry.suppressed)
    }

    /// Drain up to `max` retained cleanup records in retention order.
    pub fn drain_cleanup(&mut self, max: usize) -> Vec<CleanupRecord> {
        let count = max.min(self.cleanup_retained.len());
        self.cleanup_retained.drain(..count).collect()
    }

    /// Advance one validated input at monotonic time `now`, appending effects
    /// to `out`. The batch carries its own bound; when it fills, mutating
    /// inputs stop before changing state and report truncation.
    pub fn handle(
        &mut self,
        input: KernelInput,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<HandleOutcome, CoreError> {
        match input {
            KernelInput::Admit {
                operation_id,
                owner,
                attachment,
                generation,
                timeout_ms,
            } => self.admit(
                AdmitRequest {
                    operation_id,
                    owner,
                    attachment,
                    generation,
                    timeout_ms,
                    now,
                },
                out,
            ),
            KernelInput::Dispatch {
                operation_id,
                generation,
            } => self.dispatch(&operation_id, &generation, out),
            KernelInput::Complete {
                operation_id,
                generation,
                contender,
            } => self.complete(&operation_id, &generation, contender, now, out),
            KernelInput::Cancel {
                operation_id,
                generation,
            } => self.cancel(&operation_id, &generation, now, out),
            KernelInput::ExpireSweep => self.expire_sweep(now, out),
            KernelInput::ReleaseReport {
                operation_id,
                ok,
                code,
            } => self.release_report(&operation_id, ok, code),
            KernelInput::Shutdown => self.shutdown(out),
        }
    }

    fn find(&self, id: &OperationId) -> Option<&OpEntry> {
        self.ops.iter().find(|entry| &entry.id == id)
    }

    fn live_owned_by(&self, owner: &LeaseId) -> usize {
        self.ops
            .iter()
            .filter(|entry| {
                &entry.owner == owner && !matches!(entry.state, OpLifecycle::Terminal { .. })
            })
            .count()
    }

    fn take_ordinal(&mut self) -> u64 {
        let ordinal = self.authority;
        self.authority = self.authority.saturating_add(1);
        ordinal
    }

    fn stage(
        out: &mut EffectBatch,
        kind: EffectKind,
        operation_id: &OperationId,
        detail: String,
    ) -> Result<(), CoreError> {
        out.push(Effect {
            kind,
            operation_id: operation_id.clone(),
            detail,
        })
    }

    fn settle_live(
        &mut self,
        index: usize,
        request: SettleRequest<'_>,
        out: &mut EffectBatch,
    ) -> Result<SettlementReceipt, CoreError> {
        let dispatched = matches!(self.ops[index].state, OpLifecycle::Dispatched);
        let terminal = terminal_for_winner(request.kind);
        let terminal_kind = terminal_kind_from_completion(terminal);
        for effect in request.effects {
            Self::stage(
                out,
                *effect,
                &self.ops[index].id,
                effect_detail(*effect, &terminal_kind, self.ops[index].deadline),
            )?;
        }
        let entry = &mut self.ops[index];
        entry.state = OpLifecycle::Terminal {
            kind: terminal_kind,
        };
        entry.winner_ordinal = Some(request.ordinal);
        Ok(SettlementReceipt {
            operation_id: entry.id.clone(),
            kind: terminal_kind,
            cause: request.cause,
            ingress_ordinal: request.ordinal,
            started_at: entry.started_at,
            settled_at: request.now,
            reached_radio: reached_radio_for(dispatched, request.kind),
            paths_invalid_before_settlement: paths_invalid_for(request.kind),
            commit_state: commit_for(dispatched, request.kind),
            phase: request.phase,
            suppressed_duplicates: entry.suppressed,
        })
    }

    fn admit(
        &mut self,
        request: AdmitRequest,
        out: &mut EffectBatch,
    ) -> Result<HandleOutcome, CoreError> {
        if !self.admission_open {
            return Err(CoreError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                "kernel.admit",
            ));
        }
        assert_handshake_complete(self.handshake, "kernel.admit")?;
        assert_same_attachment(&request.attachment, &self.attachment, "kernel.admit")?;
        if !is_generation_current(&request.generation, &self.generation) {
            return Err(CoreError::new(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                "kernel.admit",
            ));
        }
        if self.find(&request.operation_id).is_some() {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "kernel.admit.duplicate",
            ));
        }
        if self.ops.len() >= self.config.max_operations {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "kernel.admit.bound",
            ));
        }
        if self.live_owned_by(&request.owner) >= self.config.max_operations_per_owner {
            return Err(CoreError::new(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "kernel.admit.owner-bound",
            ));
        }
        assert_timeout_ms(request.timeout_ms, "kernel.admit.timeout")?;
        let deadline = to_deadline(request.now, request.timeout_ms)?;
        if out.remaining() < 1 {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "effect-batch.full",
            ));
        }
        Self::stage(
            out,
            EffectKind::TimerSchedule,
            &request.operation_id,
            timer_detail(deadline),
        )?;
        self.ops.push(OpEntry {
            id: request.operation_id,
            owner: request.owner,
            generation: request.generation,
            state: OpLifecycle::Queued,
            deadline,
            started_at: request.now,
            suppressed: 0,
            winner_ordinal: None,
            release_requested: false,
        });
        Ok(HandleOutcome::Admitted { deadline })
    }

    fn dispatch(
        &mut self,
        operation_id: &OperationId,
        generation: &Generation,
        out: &mut EffectBatch,
    ) -> Result<HandleOutcome, CoreError> {
        let index = self.position_of(operation_id, "kernel.dispatch.unknown")?;
        self.require_current_generation(index, generation, "kernel.dispatch")?;
        match self.ops[index].state {
            OpLifecycle::Queued => {}
            _ => {
                return Err(CoreError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "kernel.dispatch.state",
                ));
            }
        }
        if out.remaining() < 2 {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "effect-batch.full",
            ));
        }
        Self::stage(
            out,
            EffectKind::RadioDispatch,
            operation_id,
            String::from("radio.dispatch"),
        )?;
        Self::stage(
            out,
            EffectKind::StatePublish,
            operation_id,
            String::from("state.dispatched"),
        )?;
        self.ops[index].state = OpLifecycle::Dispatched;
        Ok(HandleOutcome::Dispatched)
    }

    fn complete(
        &mut self,
        operation_id: &OperationId,
        generation: &Generation,
        contender: Contender,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<HandleOutcome, CoreError> {
        let index = self.position_of(operation_id, "kernel.complete.unknown")?;
        self.require_current_generation(index, generation, "kernel.complete")?;
        if matches!(self.ops[index].state, OpLifecycle::Terminal { .. }) {
            self.ops[index].suppressed = self.ops[index].suppressed.saturating_add(1);
            return Ok(HandleOutcome::DuplicateSuppressed);
        }
        if !contender.valid {
            return Ok(HandleOutcome::ContenderIgnored);
        }
        if out.remaining() < 3 {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "effect-batch.full",
            ));
        }
        let cause = cause_for(contender.kind);
        let receipt = self.settle_live(
            index,
            SettleRequest {
                kind: contender.kind,
                cause,
                ordinal: contender.ingress_ordinal,
                now,
                phase: SettlePhase::Contended,
                effects: &[
                    EffectKind::TimerCancel,
                    EffectKind::StatePublish,
                    EffectKind::CleanupRelease,
                ],
            },
            out,
        )?;
        Ok(HandleOutcome::Settled { receipt })
    }

    fn cancel(
        &mut self,
        operation_id: &OperationId,
        generation: &Generation,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<HandleOutcome, CoreError> {
        let index = self.position_of(operation_id, "kernel.cancel.unknown")?;
        self.require_current_generation(index, generation, "kernel.cancel")?;
        if matches!(self.ops[index].state, OpLifecycle::Terminal { .. }) {
            self.ops[index].suppressed = self.ops[index].suppressed.saturating_add(1);
            return Ok(HandleOutcome::DuplicateSuppressed);
        }
        let dispatched = matches!(self.ops[index].state, OpLifecycle::Dispatched);
        let phase = if dispatched {
            CancelPhase::AfterDispatch
        } else {
            CancelPhase::BeforeDispatch
        };
        if out.remaining() < 3 {
            return Err(CoreError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "effect-batch.full",
            ));
        }
        let ordinal = self.take_ordinal();
        let receipt = self.settle_live(
            index,
            SettleRequest {
                kind: ContenderKind::Abort,
                cause: Some(BleErrorCode::OperationAborted),
                ordinal,
                now,
                phase: SettlePhase::Cancelled(phase),
                effects: &[
                    EffectKind::TimerCancel,
                    EffectKind::StatePublish,
                    EffectKind::CleanupRelease,
                ],
            },
            out,
        )?;
        Ok(HandleOutcome::Cancelled { receipt })
    }

    fn expire_sweep(
        &mut self,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<HandleOutcome, CoreError> {
        let mut settled = 0usize;
        let mut truncated = false;
        let mut index = 0usize;
        while index < self.ops.len() {
            let due = matches!(self.ops[index].state, OpLifecycle::Queued)
                && is_deadline_expired(now, self.ops[index].deadline);
            if !due {
                index += 1;
                continue;
            }
            if out.remaining() < 2 {
                truncated = true;
                break;
            }
            let ordinal = self.take_ordinal();
            match self.settle_live(
                index,
                SettleRequest {
                    kind: ContenderKind::Timeout,
                    cause: Some(BleErrorCode::OperationTimedOut),
                    ordinal,
                    now,
                    phase: SettlePhase::Expired,
                    effects: &[EffectKind::StatePublish, EffectKind::CleanupRelease],
                },
                out,
            ) {
                Ok(_) => {}
                Err(error) => return Err(error),
            }
            settled += 1;
            index += 1;
        }
        Ok(HandleOutcome::Swept { settled, truncated })
    }

    fn release_report(
        &mut self,
        operation_id: &OperationId,
        ok: bool,
        code: Option<BleErrorCode>,
    ) -> Result<HandleOutcome, CoreError> {
        let index = self.position_of(operation_id, "kernel.release.unknown")?;
        if !matches!(self.ops[index].state, OpLifecycle::Terminal { .. }) {
            return Err(CoreError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "kernel.release.live",
            ));
        }
        if !ok {
            let Some(code) = code else {
                return Err(CoreError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "kernel.release.cause",
                ));
            };
            if self.cleanup_retained.len() >= self.config.max_operations {
                return Err(CoreError::new(
                    BleErrorCode::StreamQuota,
                    BleErrorDomain::Stream,
                    "kernel.release.retained-full",
                ));
            }
            let record = CleanupRecord::new(
                Some(operation_id.clone()),
                CleanupState::ReleaseFailed,
                Vec::from([CleanupFailure {
                    resource_kind: String::from("operation"),
                    code,
                }]),
            )?;
            self.cleanup_retained.push(record);
        }
        self.ops.remove(index);
        Ok(HandleOutcome::ReleaseRecorded { reaped: true })
    }

    fn shutdown(&mut self, out: &mut EffectBatch) -> Result<HandleOutcome, CoreError> {
        self.admission_open = false;
        let mut settled_queued = 0usize;
        let mut truncated = false;
        let mut index = 0usize;
        while index < self.ops.len() {
            if !matches!(self.ops[index].state, OpLifecycle::Queued) {
                index += 1;
                continue;
            }
            if out.remaining() < 2 || self.cleanup_retained.len() >= self.config.max_operations {
                truncated = true;
                break;
            }
            let ordinal = self.take_ordinal();
            // Shutdown settles at the operation's own deadline instant: the
            // record keeps `settled_at >= started_at` by construction.
            let settled_at = self.ops[index].deadline.max(self.ops[index].started_at);
            match self.settle_live(
                index,
                SettleRequest {
                    kind: ContenderKind::Destroy,
                    cause: Some(BleErrorCode::OperationCancelledByDestroy),
                    ordinal,
                    now: settled_at,
                    phase: SettlePhase::Shutdown,
                    effects: &[EffectKind::StatePublish, EffectKind::CleanupRelease],
                },
                out,
            ) {
                Ok(_) => {}
                Err(error) => return Err(error),
            }
            let settled_id = self.ops[index].id.clone();
            let record = CleanupRecord::new(Some(settled_id), CleanupState::Released, Vec::new())?;
            self.cleanup_retained.push(record);
            settled_queued += 1;
            index += 1;
        }
        // Request release of dispatched work still owned by the kernel. Each
        // request is emitted once; hosts answer with `ReleaseReport`.
        for entry in self.ops.iter_mut() {
            if !matches!(entry.state, OpLifecycle::Dispatched) || entry.release_requested {
                continue;
            }
            if out.remaining() < 1 {
                truncated = true;
                break;
            }
            let staged = out.push(Effect {
                kind: EffectKind::CleanupRelease,
                operation_id: entry.id.clone(),
                detail: String::from("cleanup.release-request"),
            });
            match staged {
                Ok(()) => entry.release_requested = true,
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
        let retained = self.cleanup_retained.len();
        Ok(HandleOutcome::ShutDown {
            settled_queued,
            retained,
            truncated,
        })
    }

    fn position_of(&self, operation_id: &OperationId, operation: &str) -> Result<usize, CoreError> {
        self.ops
            .iter()
            .position(|entry| &entry.id == operation_id)
            .ok_or_else(|| {
                CoreError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    String::from(operation),
                )
            })
    }

    fn require_current_generation(
        &self,
        index: usize,
        generation: &Generation,
        operation: &str,
    ) -> Result<(), CoreError> {
        if !is_generation_current(&self.ops[index].generation, &self.generation)
            || !is_generation_current(generation, &self.generation)
        {
            return Err(CoreError::new(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                String::from(operation),
            ));
        }
        Ok(())
    }
}

const fn terminal_kind_from_completion(terminal: CompletionTerminal) -> OperationTerminalKind {
    match terminal {
        CompletionTerminal::Succeeded => OperationTerminalKind::Succeeded,
        CompletionTerminal::Aborted => OperationTerminalKind::Aborted,
        CompletionTerminal::TimedOut => OperationTerminalKind::TimedOut,
        CompletionTerminal::Disconnected => OperationTerminalKind::Disconnected,
        CompletionTerminal::Reset => OperationTerminalKind::Reset,
        CompletionTerminal::AdapterUnavailable => OperationTerminalKind::AdapterUnavailable,
        CompletionTerminal::Destroyed => OperationTerminalKind::Destroyed,
    }
}

fn cause_for(kind: ContenderKind) -> Option<BleErrorCode> {
    match kind {
        ContenderKind::Success | ContenderKind::DispatchBegin => None,
        ContenderKind::Abort | ContenderKind::SessionStop => Some(BleErrorCode::OperationAborted),
        ContenderKind::Timeout => Some(BleErrorCode::OperationTimedOut),
        ContenderKind::Disconnect => Some(BleErrorCode::OperationDisconnected),
        ContenderKind::Reset => Some(BleErrorCode::OperationReset),
        ContenderKind::Destroy => Some(BleErrorCode::OperationCancelledByDestroy),
        ContenderKind::AdapterLoss => Some(BleErrorCode::OperationAdapterUnavailable),
    }
}

fn effect_detail(
    kind: EffectKind,
    terminal: &OperationTerminalKind,
    deadline: MonotonicTime,
) -> String {
    match kind {
        EffectKind::TimerSchedule | EffectKind::TimerCancel => timer_detail(deadline),
        EffectKind::StatePublish => {
            let mut detail = String::from("state.");
            detail.push_str(terminal.as_str());
            detail
        }
        EffectKind::RadioDispatch => String::from("radio.dispatch"),
        EffectKind::CleanupRelease => String::from("cleanup.release"),
        EffectKind::ObservationDeliver => String::from("observation.deliver"),
    }
}

fn timer_detail(deadline: MonotonicTime) -> String {
    let mut detail = String::from("timer.deadline=");
    append_u64(&mut detail, deadline);
    detail
}

#[cfg(test)]
use crate::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, BackendGeneration, BackendInstanceId,
};

#[cfg(test)]
fn test_attachment_id() -> AttachmentId {
    // `AttachmentId::new` rejects only empty strings; the literal is
    // non-empty, so the fallback always succeeds.
    match AttachmentId::new("attach-01") {
        Ok(id) => id,
        Err(_) => test_attachment_id(),
    }
}

#[cfg(test)]
fn test_instance_id() -> BackendInstanceId {
    match BackendInstanceId::new("backend-01") {
        Ok(id) => id,
        Err(_) => test_instance_id(),
    }
}

#[cfg(test)]
fn test_backend_generation() -> BackendGeneration {
    match BackendGeneration::new("bg-3") {
        Ok(generation) => generation,
        Err(_) => test_backend_generation(),
    }
}

#[cfg(test)]
fn test_adapter_id() -> AdapterId {
    match AdapterId::new("adapter-01") {
        Ok(id) => id,
        Err(_) => test_adapter_id(),
    }
}

#[cfg(test)]
fn test_adapter_generation() -> AdapterGeneration {
    match AdapterGeneration::new("ag-2") {
        Ok(generation) => generation,
        Err(_) => test_adapter_generation(),
    }
}

#[cfg(test)]
fn test_attachment() -> AttachmentTuple {
    AttachmentTuple::new(
        test_attachment_id(),
        test_instance_id(),
        test_backend_generation(),
        test_adapter_id(),
        test_adapter_generation(),
    )
}

#[cfg(test)]
fn test_generation(value: &str) -> Generation {
    match Generation::new(value) {
        Ok(generation) => generation,
        Err(_) => test_generation("test-generation"),
    }
}

#[cfg(test)]
fn test_operation_id(value: &str) -> OperationId {
    match OperationId::new(value) {
        Ok(id) => id,
        Err(_) => test_operation_id("test-operation"),
    }
}

#[cfg(test)]
fn test_lease(value: &str) -> LeaseId {
    match LeaseId::new(value) {
        Ok(lease) => lease,
        Err(_) => test_lease("test-lease"),
    }
}

#[cfg(test)]
impl Kernel {
    fn new_test() -> Self {
        Self::new(
            KernelConfig::default(),
            test_attachment(),
            test_generation("gen-1"),
            HandshakeState { complete: true },
        )
    }

    fn new_test_with(config: KernelConfig) -> Self {
        Self::new(
            config,
            test_attachment(),
            test_generation("gen-1"),
            HandshakeState { complete: true },
        )
    }
}

#[cfg(test)]
impl KernelInput {
    fn admit_test_op(id: &str, generation: &str, timeout_ms: u64) -> Self {
        Self::Admit {
            operation_id: test_operation_id(id),
            owner: test_lease("owner-test"),
            attachment: test_attachment(),
            generation: test_generation(generation),
            timeout_ms,
        }
    }

    fn dispatch_test_op(id: &str, generation: &str) -> Self {
        Self::Dispatch {
            operation_id: test_operation_id(id),
            generation: test_generation(generation),
        }
    }

    fn complete_test_op(id: &str, generation: &str) -> Self {
        Self::Complete {
            operation_id: test_operation_id(id),
            generation: test_generation(generation),
            contender: Contender {
                ingress_ordinal: 1,
                kind: ContenderKind::Success,
                valid: true,
            },
        }
    }

    fn complete_with(
        id: &str,
        generation: &str,
        ordinal: u64,
        kind: ContenderKind,
        valid: bool,
    ) -> Self {
        Self::Complete {
            operation_id: test_operation_id(id),
            generation: test_generation(generation),
            contender: Contender {
                ingress_ordinal: ordinal,
                kind,
                valid,
            },
        }
    }

    fn cancel_test_op(id: &str, generation: &str) -> Self {
        Self::Cancel {
            operation_id: test_operation_id(id),
            generation: test_generation(generation),
        }
    }
}

#[cfg(test)]
mod red_probes {
    use super::{EffectBatch, Kernel, KernelInput};

    #[test]
    fn stale_generation_completion_is_rejected() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(8);
        let outcome = kernel.handle(
            KernelInput::admit_test_op("op-1", "gen-1", 1_000),
            0,
            &mut out,
        );
        assert!(outcome.is_ok());
        let rejected = kernel.handle(KernelInput::complete_test_op("op-1", "gen-0"), 10, &mut out);
        assert!(rejected.is_err());
    }

    #[test]
    fn duplicate_completion_is_suppressed() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(8);
        let admitted = kernel.handle(
            KernelInput::admit_test_op("op-2", "gen-1", 1_000),
            0,
            &mut out,
        );
        assert!(admitted.is_ok());
        let first = kernel.handle(KernelInput::complete_test_op("op-2", "gen-1"), 10, &mut out);
        assert!(first.is_ok());
        let effects_after_first = out.len();
        let second = kernel.handle(KernelInput::complete_test_op("op-2", "gen-1"), 11, &mut out);
        assert!(second.is_ok());
        assert_eq!(out.len(), effects_after_first);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CancelPhase, CleanupRecord, CleanupState, ContenderKind, EffectBatch, EffectKind,
        HandleOutcome, Kernel, KernelConfig, KernelInput, OpStateView, OwnershipDecision,
        SettlePhase, test_generation, test_lease, test_operation_id,
    };
    use crate::check;
    use crate::contracts::{
        BleErrorCode, CommitState, Contender, HandshakeState, OperationTerminalKind,
    };

    #[test]
    fn admit_dispatch_complete_success_receipt() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        match kernel.handle(
            KernelInput::admit_test_op("op-1", "gen-1", 1_000),
            0,
            &mut out,
        ) {
            Ok(HandleOutcome::Admitted { deadline }) => assert_eq!(deadline, 1_000),
            _ => check(false, "admit must succeed"),
        }
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Queued)
        );
        assert!(
            kernel
                .handle(KernelInput::dispatch_test_op("op-1", "gen-1"), 10, &mut out)
                .is_ok()
        );
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Dispatched)
        );
        match kernel.handle(KernelInput::complete_test_op("op-1", "gen-1"), 20, &mut out) {
            Ok(HandleOutcome::Settled { receipt }) => {
                assert_eq!(receipt.kind(), OperationTerminalKind::Succeeded);
                assert_eq!(receipt.cause(), None);
                assert_eq!(receipt.phase(), SettlePhase::Contended);
                assert!(receipt.reached_radio());
                assert!(!receipt.paths_invalid_before_settlement());
                assert_eq!(receipt.commit_state(), CommitState::Committed);
                assert_eq!(receipt.ingress_ordinal(), 1);
                assert_eq!(receipt.started_at(), 0);
                assert_eq!(receipt.settled_at(), 20);
                match receipt.terminal_record() {
                    Ok(record) => {
                        assert_eq!(record.kind(), OperationTerminalKind::Succeeded);
                        assert_eq!(record.cause(), None);
                    }
                    Err(_) => check(false, "receipt must project to a record"),
                }
            }
            _ => check(false, "complete must settle"),
        }
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Terminal(OperationTerminalKind::Succeeded))
        );
        // Admit (1) + dispatch (2) + settle (3) effects.
        assert_eq!(out.len(), 6);
        assert_effect_kinds(&out);
    }

    fn assert_effect_kinds(batch: &EffectBatch) {
        let mut seen: [bool; 6] = [false; 6];
        for effect in batch.effects() {
            match effect.kind() {
                EffectKind::TimerSchedule => seen[0] = true,
                EffectKind::RadioDispatch => seen[1] = true,
                EffectKind::StatePublish => seen[2] = true,
                EffectKind::TimerCancel => seen[3] = true,
                EffectKind::CleanupRelease => seen[4] = true,
                EffectKind::ObservationDeliver => seen[5] = true,
            }
        }
        assert!(seen[0] && seen[1] && seen[2] && seen[3] && seen[4]);
        assert!(!seen[5]);
    }

    #[test]
    fn success_before_dispatch_does_not_reach_radio() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(KernelInput::complete_test_op("op-1", "gen-1"), 5, &mut out) {
            Ok(HandleOutcome::Settled { receipt }) => {
                assert_eq!(receipt.kind(), OperationTerminalKind::Succeeded);
                assert!(!receipt.reached_radio());
                assert_eq!(receipt.commit_state(), CommitState::Committed);
            }
            _ => check(false, "queued success must settle"),
        }
    }

    #[test]
    fn cancel_before_dispatch_receipt() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(KernelInput::cancel_test_op("op-1", "gen-1"), 7, &mut out) {
            Ok(HandleOutcome::Cancelled { receipt }) => {
                assert_eq!(receipt.kind(), OperationTerminalKind::Aborted);
                assert_eq!(receipt.cause(), Some(BleErrorCode::OperationAborted));
                assert_eq!(
                    receipt.phase(),
                    SettlePhase::Cancelled(CancelPhase::BeforeDispatch)
                );
                assert!(!receipt.reached_radio());
                assert_eq!(receipt.commit_state(), CommitState::NotDispatched);
            }
            _ => check(false, "cancel must settle"),
        }
    }

    #[test]
    fn cancel_after_dispatch_receipt() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::dispatch_test_op("op-1", "gen-1"), 3, &mut out)
                .is_ok()
        );
        match kernel.handle(KernelInput::cancel_test_op("op-1", "gen-1"), 9, &mut out) {
            Ok(HandleOutcome::Cancelled { receipt }) => {
                assert_eq!(
                    receipt.phase(),
                    SettlePhase::Cancelled(CancelPhase::AfterDispatch)
                );
                // C-UBM arbitration: an abort contender never counts as
                // reaching the radio, even after dispatch.
                assert!(!receipt.reached_radio());
                assert_eq!(receipt.commit_state(), CommitState::Released);
            }
            _ => check(false, "cancel after dispatch must settle"),
        }
    }

    #[test]
    fn terminal_operations_suppress_late_callbacks() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-1", "gen-1"), 5, &mut out)
                .is_ok()
        );
        let effects = out.len();
        match kernel.handle(KernelInput::complete_test_op("op-1", "gen-1"), 6, &mut out) {
            Ok(HandleOutcome::DuplicateSuppressed) => {}
            _ => check(false, "late completion must suppress"),
        }
        match kernel.handle(KernelInput::cancel_test_op("op-1", "gen-1"), 7, &mut out) {
            Ok(HandleOutcome::DuplicateSuppressed) => {}
            _ => check(false, "late cancel must suppress"),
        }
        assert_eq!(out.len(), effects);
        assert_eq!(kernel.suppressed_count(&test_operation_id("op-1")), Some(2));
    }

    #[test]
    fn invalid_contender_is_ignored() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(
            KernelInput::complete_with("op-1", "gen-1", 4, ContenderKind::Success, false),
            5,
            &mut out,
        ) {
            Ok(HandleOutcome::ContenderIgnored) => {}
            _ => check(false, "invalid contender must not contend"),
        }
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Queued)
        );
    }

    #[test]
    fn unknown_operations_are_rejected() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::dispatch_test_op("missing", "gen-1"),
                    0,
                    &mut out
                )
                .is_err()
        );
        assert!(
            kernel
                .handle(
                    KernelInput::complete_test_op("missing", "gen-1"),
                    0,
                    &mut out
                )
                .is_err()
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("missing", "gen-1"), 0, &mut out)
                .is_err()
        );
        match kernel.handle(
            super::KernelInput::ReleaseReport {
                operation_id: test_operation_id("missing"),
                ok: true,
                code: None,
            },
            0,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::ArgumentInvalid),
            Ok(_) => check(false, "unknown release must fail"),
        }
    }

    #[test]
    fn duplicate_admit_is_rejected() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(
            KernelInput::admit_test_op("op-1", "gen-1", 1_000),
            1,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::ArgumentInvalid),
            Ok(_) => check(false, "duplicate id must fail"),
        }
    }

    #[test]
    fn stale_generations_never_mutate() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        // Admit under a stale generation fails before allocation.
        match kernel.handle(
            KernelInput::admit_test_op("op-1", "gen-0", 1_000),
            0,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::ConnectionStale),
            Ok(_) => check(false, "stale admit must fail"),
        }
        assert_eq!(kernel.live_operation_count(), 0);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::dispatch_test_op("op-1", "gen-0"), 1, &mut out)
                .is_err()
        );
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Queued)
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-1", "gen-0"), 2, &mut out)
                .is_err()
        );
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Queued)
        );
    }

    #[test]
    fn foreign_attachment_is_rejected_before_effects() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(16);
        let effects = out.len();
        let mut foreign = super::test_attachment();
        foreign = foreign_moved(foreign);
        let input = super::KernelInput::Admit {
            operation_id: test_operation_id("op-foreign"),
            owner: test_lease("owner-test"),
            attachment: foreign,
            generation: test_generation("gen-1"),
            timeout_ms: 1_000,
        };
        match kernel.handle(input, 0, &mut out) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::ConnectionStale),
            Ok(_) => check(false, "foreign attachment must be stale"),
        }
        assert_eq!(out.len(), effects);
        assert_eq!(kernel.live_operation_count(), 0);
    }

    fn foreign_moved(attachment: super::AttachmentTuple) -> super::AttachmentTuple {
        // Re-scope the tuple to a different backend generation so equality
        // fails on exactly one field.
        super::AttachmentTuple::new(
            super_attachment_id(attachment.attachment_id().as_str()),
            super_instance_id(attachment.backend_instance_id().as_str()),
            super_backend_generation("bg-other"),
            super_adapter_id(attachment.adapter_id().as_str()),
            super_adapter_generation(attachment.adapter_generation().as_str()),
        )
    }

    fn super_attachment_id(value: &str) -> super::AttachmentId {
        use crate::contracts::AttachmentId;
        match AttachmentId::new(value) {
            Ok(id) => id,
            Err(_) => super_attachment_id("attach-01"),
        }
    }

    fn super_instance_id(value: &str) -> super::BackendInstanceId {
        use crate::contracts::BackendInstanceId;
        match BackendInstanceId::new(value) {
            Ok(id) => id,
            Err(_) => super_instance_id("backend-01"),
        }
    }

    fn super_backend_generation(value: &str) -> super::BackendGeneration {
        use crate::contracts::BackendGeneration;
        match BackendGeneration::new(value) {
            Ok(generation) => generation,
            Err(_) => super_backend_generation("bg-3"),
        }
    }

    fn super_adapter_id(value: &str) -> super::AdapterId {
        use crate::contracts::AdapterId;
        match AdapterId::new(value) {
            Ok(id) => id,
            Err(_) => super_adapter_id("adapter-01"),
        }
    }

    fn super_adapter_generation(value: &str) -> super::AdapterGeneration {
        use crate::contracts::AdapterGeneration;
        match AdapterGeneration::new(value) {
            Ok(generation) => generation,
            Err(_) => super_adapter_generation("ag-2"),
        }
    }

    #[test]
    fn admission_bound_is_enforced() {
        let config = match KernelConfig::new(1, 16, 8) {
            Ok(config) => config,
            Err(_) => {
                check(false, "test config must validate");
                return;
            }
        };
        let mut kernel = super::Kernel::new_test_with(config);
        let mut out = EffectBatch::new(16);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(
            KernelInput::admit_test_op("op-2", "gen-1", 1_000),
            0,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "map bound must hold"),
        }
        assert_eq!(kernel.live_operation_count(), 1);
    }

    #[test]
    fn per_owner_lease_bound_is_enforced() {
        // Map room for four, but only two live operations per owner lease.
        let config = match KernelConfig::new(4, 32, 2) {
            Ok(config) => config,
            Err(_) => {
                check(false, "test config must validate");
                return;
            }
        };
        let mut kernel = super::Kernel::new_test_with(config);
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-2", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        // Same owner a third time: ownership denied even with map room.
        match kernel.handle(
            KernelInput::admit_test_op("op-3", "gen-1", 1_000),
            0,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::OwnershipDenied),
            Ok(_) => check(false, "owner bound must hold"),
        }
        assert_eq!(kernel.live_operation_count(), 2);
    }

    #[test]
    fn full_effect_batch_blocks_mutation() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(0);
        match kernel.handle(
            KernelInput::admit_test_op("op-1", "gen-1", 1_000),
            0,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "full batch must block admit"),
        }
        assert_eq!(kernel.live_operation_count(), 0);
    }

    #[test]
    fn expiry_sweep_settles_timed_out() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 100),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(super::KernelInput::ExpireSweep, 50, &mut out) {
            Ok(HandleOutcome::Swept { settled, truncated }) => {
                assert_eq!(settled, 0);
                assert!(!truncated);
            }
            _ => check(false, "early sweep settles nothing"),
        }
        match kernel.handle(super::KernelInput::ExpireSweep, 100, &mut out) {
            Ok(HandleOutcome::Swept { settled, truncated }) => {
                assert_eq!(settled, 1);
                assert!(!truncated);
            }
            _ => check(false, "deadline sweep must settle"),
        }
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Terminal(OperationTerminalKind::TimedOut))
        );
    }

    #[test]
    fn sweep_truncation_resumes() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(KernelInput::admit_test_op("op-1", "gen-1", 10), 0, &mut out)
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::admit_test_op("op-2", "gen-1", 10), 0, &mut out)
                .is_ok()
        );
        // Two staged admits; room for exactly one 2-effect settlement.
        let mut tight = EffectBatch::new(out.len() + 2);
        for effect in out.drain() {
            let _ = tight.push(effect);
        }
        match kernel.handle(super::KernelInput::ExpireSweep, 50, &mut tight) {
            Ok(HandleOutcome::Swept { settled, truncated }) => {
                assert_eq!(settled, 1);
                assert!(truncated);
            }
            _ => check(false, "tight sweep must truncate"),
        }
        let mut roomy = EffectBatch::new(8);
        match kernel.handle(super::KernelInput::ExpireSweep, 50, &mut roomy) {
            Ok(HandleOutcome::Swept { settled, truncated }) => {
                assert_eq!(settled, 1);
                assert!(!truncated);
            }
            _ => check(false, "resumed sweep must finish"),
        }
    }

    #[test]
    fn disconnect_paths_invalidate_before_settlement() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::dispatch_test_op("op-1", "gen-1"), 5, &mut out)
                .is_ok()
        );
        match kernel.handle(
            KernelInput::complete_with("op-1", "gen-1", 9, ContenderKind::Disconnect, true),
            10,
            &mut out,
        ) {
            Ok(HandleOutcome::Settled { receipt }) => {
                assert_eq!(receipt.kind(), OperationTerminalKind::Disconnected);
                assert_eq!(receipt.cause(), Some(BleErrorCode::OperationDisconnected));
                assert!(receipt.paths_invalid_before_settlement());
                assert!(receipt.reached_radio());
                assert_eq!(receipt.commit_state(), CommitState::Released);
            }
            _ => check(false, "disconnect must settle"),
        }
    }

    #[test]
    fn timeout_after_dispatch_is_unknown_commit() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::dispatch_test_op("op-1", "gen-1"), 5, &mut out)
                .is_ok()
        );
        match kernel.handle(
            KernelInput::complete_with("op-1", "gen-1", 9, ContenderKind::Timeout, true),
            60,
            &mut out,
        ) {
            Ok(HandleOutcome::Settled { receipt }) => {
                assert_eq!(receipt.kind(), OperationTerminalKind::TimedOut);
                assert_eq!(receipt.commit_state(), CommitState::Unknown);
                assert!(receipt.reached_radio());
            }
            _ => check(false, "timeout must settle unknown"),
        }
    }

    #[test]
    fn shutdown_sequence_retains_cleanup() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(64);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 5_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-2", "gen-1", 5_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::dispatch_test_op("op-2", "gen-1"), 1, &mut out)
                .is_ok()
        );
        match kernel.handle(super::KernelInput::Shutdown, 2, &mut out) {
            Ok(HandleOutcome::ShutDown {
                settled_queued,
                retained,
                truncated,
            }) => {
                assert_eq!(settled_queued, 1);
                assert_eq!(retained, 1);
                assert!(!truncated);
            }
            _ => check(false, "shutdown must sweep queued work"),
        }
        assert!(!kernel.admission_open());
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-1")),
            Some(OpStateView::Terminal(OperationTerminalKind::Destroyed))
        );
        // Dispatched work stays owned until the host answers; admission is
        // closed for newcomers.
        assert_eq!(
            kernel.operation_state(&test_operation_id("op-2")),
            Some(OpStateView::Dispatched)
        );
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-3", "gen-1", 1_000),
                    3,
                    &mut out
                )
                .is_err()
        );
        // Settle the dispatched remainder, then report both releases.
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-2", "gen-1"), 4, &mut out)
                .is_ok()
        );
        match kernel.handle(
            super::KernelInput::ReleaseReport {
                operation_id: test_operation_id("op-1"),
                ok: true,
                code: None,
            },
            5,
            &mut out,
        ) {
            Ok(HandleOutcome::ReleaseRecorded { reaped }) => assert!(reaped),
            _ => check(false, "release must reap"),
        }
        assert!(
            kernel
                .handle(
                    super::KernelInput::ReleaseReport {
                        operation_id: test_operation_id("op-2"),
                        ok: true,
                        code: None,
                    },
                    5,
                    &mut out,
                )
                .is_ok()
        );
        assert_eq!(kernel.live_operation_count(), 0);
        // One retained record from the shutdown sweep.
        assert_eq!(kernel.retained_cleanup_count(), 1);
        let drained = kernel.drain_cleanup(8);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].state(), CleanupState::Released);
        assert!(drained[0].failures().is_empty());
        assert_eq!(kernel.retained_cleanup_count(), 0);
    }

    #[test]
    fn failed_release_is_retained_never_swallowed() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-1", "gen-1"), 5, &mut out)
                .is_ok()
        );
        match kernel.handle(
            super::KernelInput::ReleaseReport {
                operation_id: test_operation_id("op-1"),
                ok: false,
                code: Some(BleErrorCode::PlatformTransport),
            },
            6,
            &mut out,
        ) {
            Ok(HandleOutcome::ReleaseRecorded { reaped }) => assert!(reaped),
            _ => check(false, "failed release must still reap"),
        }
        assert_eq!(kernel.retained_cleanup_count(), 1);
        let drained = kernel.drain_cleanup(8);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].state(), CleanupState::ReleaseFailed);
        assert_eq!(drained[0].failures().len(), 1);
        assert_eq!(
            drained[0].failures()[0].code(),
            BleErrorCode::PlatformTransport
        );
        // A failure report without a cause is malformed.
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-2", "gen-1", 1_000),
                    7,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-2", "gen-1"), 8, &mut out)
                .is_ok()
        );
        match kernel.handle(
            super::KernelInput::ReleaseReport {
                operation_id: test_operation_id("op-2"),
                ok: false,
                code: None,
            },
            9,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::ArgumentInvalid),
            Ok(_) => check(false, "causeless failure must fail"),
        }
    }

    #[test]
    fn live_release_is_rejected() {
        let mut kernel = Kernel::new_test();
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        match kernel.handle(
            super::KernelInput::ReleaseReport {
                operation_id: test_operation_id("op-1"),
                ok: true,
                code: None,
            },
            1,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::LifecycleInvalidState),
            Ok(_) => check(false, "live release must fail"),
        }
    }

    #[test]
    fn retained_backpressure_forces_drain() {
        let config = match KernelConfig::new(1, 16, 8) {
            Ok(config) => config,
            Err(_) => {
                check(false, "test config must validate");
                return;
            }
        };
        let mut kernel = super::Kernel::new_test_with(config);
        let mut out = EffectBatch::new(32);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-1", "gen-1", 1_000),
                    0,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-1", "gen-1"), 1, &mut out)
                .is_ok()
        );
        assert!(
            kernel
                .handle(
                    super::KernelInput::ReleaseReport {
                        operation_id: test_operation_id("op-1"),
                        ok: false,
                        code: Some(BleErrorCode::PlatformFailure),
                    },
                    2,
                    &mut out,
                )
                .is_ok()
        );
        assert_eq!(kernel.retained_cleanup_count(), 1);
        assert!(
            kernel
                .handle(
                    KernelInput::admit_test_op("op-2", "gen-1", 1_000),
                    3,
                    &mut out
                )
                .is_ok()
        );
        assert!(
            kernel
                .handle(KernelInput::cancel_test_op("op-2", "gen-1"), 4, &mut out)
                .is_ok()
        );
        // Retained store is full: the report fails closed without reaping, so
        // no failure is lost.
        match kernel.handle(
            super::KernelInput::ReleaseReport {
                operation_id: test_operation_id("op-2"),
                ok: false,
                code: Some(BleErrorCode::PlatformFailure),
            },
            5,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "full retention must backpressure"),
        }
        assert_eq!(kernel.live_operation_count(), 1);
        let drained = kernel.drain_cleanup(4);
        assert_eq!(drained.len(), 1);
        assert!(
            kernel
                .handle(
                    super::KernelInput::ReleaseReport {
                        operation_id: test_operation_id("op-2"),
                        ok: false,
                        code: Some(BleErrorCode::PlatformFailure),
                    },
                    6,
                    &mut out,
                )
                .is_ok()
        );
    }

    #[test]
    fn handshake_gate_blocks_admission() {
        let kernel = super::Kernel::new(
            KernelConfig::default(),
            super::test_attachment(),
            test_generation("gen-1"),
            HandshakeState { complete: false },
        );
        let mut kernel = kernel;
        let mut out = EffectBatch::new(8);
        match kernel.handle(
            KernelInput::admit_test_op("op-1", "gen-1", 1_000),
            0,
            &mut out,
        ) {
            Err(error) => assert_eq!(error.code(), BleErrorCode::LifecycleInvalidState),
            Ok(_) => check(false, "handshake must gate admission"),
        }
        assert_eq!(kernel.live_operation_count(), 0);
    }

    #[test]
    fn scan_and_connection_arbitration() {
        match super::arbitrate_scan_request(false, None) {
            OwnershipDecision::GrantPhysical => {}
            _ => check(false, "idle scanner grants physical"),
        }
        match super::arbitrate_scan_request(true, Some("share-a")) {
            OwnershipDecision::GrantLease { lease } => assert_eq!(lease, "scan-share:share-a"),
            _ => check(false, "shared scan grants a lease"),
        }
        match super::arbitrate_scan_request(true, None) {
            OwnershipDecision::Reject { code } => {
                assert_eq!(code, BleErrorCode::ScanAlreadyActive);
            }
            _ => check(false, "second physical scan is rejected"),
        }
        match super::arbitrate_scan_request(true, Some("")) {
            OwnershipDecision::Reject { code } => {
                assert_eq!(code, BleErrorCode::ScanAlreadyActive);
            }
            _ => check(false, "empty share token is not shared"),
        }
        match super::arbitrate_connection_request(false, 0) {
            OwnershipDecision::GrantPhysical => {}
            _ => check(false, "first connection grants physical"),
        }
        match super::arbitrate_connection_request(true, 1) {
            OwnershipDecision::GrantLease { lease } => assert_eq!(lease, "connection-lease:2"),
            _ => check(false, "shared link grants a lease"),
        }
        match super::arbitrate_connection_request(false, 1) {
            OwnershipDecision::Reject { code } => {
                assert_eq!(code, BleErrorCode::ConnectionAlreadyOwned);
            }
            _ => check(false, "exclusive link rejects a second owner"),
        }
    }

    #[test]
    fn cleanup_records_reject_mixed_states() {
        assert!(CleanupRecord::new(None, CleanupState::Released, Vec::new()).is_ok());
        assert!(
            CleanupRecord::new(
                None,
                CleanupState::Released,
                Vec::from([super::CleanupFailure {
                    resource_kind: String::from("operation"),
                    code: BleErrorCode::PlatformFailure,
                }]),
            )
            .is_err()
        );
        assert!(CleanupRecord::new(None, CleanupState::ReleaseFailed, Vec::new()).is_err());
    }

    #[test]
    fn kernel_config_rejects_zero_bounds() {
        assert!(KernelConfig::new(0, 8, 8).is_err());
        assert!(KernelConfig::new(8, 0, 8).is_err());
        assert!(KernelConfig::new(8, 8, 0).is_err());
        assert!(KernelConfig::new(8, 8, 8).is_ok());
    }

    #[test]
    fn contender_and_effect_wire_strings() {
        let contender = Contender {
            ingress_ordinal: 3,
            kind: ContenderKind::SessionStop,
            valid: true,
        };
        assert_eq!(contender.kind.as_str(), "session-stop");
        assert_eq!(super::EffectKind::RadioDispatch.as_str(), "radio.dispatch");
        assert_eq!(super::EffectKind::TimerSchedule.as_str(), "timer.schedule");
        assert_eq!(super::EffectKind::TimerCancel.as_str(), "timer.cancel");
        assert_eq!(super::EffectKind::StatePublish.as_str(), "state.publish");
        assert_eq!(
            super::EffectKind::CleanupRelease.as_str(),
            "cleanup.release"
        );
        assert_eq!(
            super::EffectKind::ObservationDeliver.as_str(),
            "observation.deliver"
        );
        assert_eq!(
            super::CancelPhase::BeforeDispatch.as_str(),
            "before-dispatch"
        );
        assert_eq!(super::CancelPhase::AfterDispatch.as_str(), "after-dispatch");
        let batch = EffectBatch::new(4);
        assert!(batch.is_empty());
        assert_eq!(batch.capacity(), 4);
        assert_eq!(batch.remaining(), 4);
    }
}
