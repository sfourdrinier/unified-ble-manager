//! Per-operation control for [`crate::DesktopCentral`]: the caller budget
//! and the cancellation ticket (PR210-05/06).
//!
//! Every operation takes one [`OpControl`]:
//!
//! * [`Budget`] is the caller's deadline, fixed from the moment the host
//!   received the call ([`Budget::from_ms_at`]). Queueing time before the
//!   core sees the operation therefore counts against it. A caller budget
//!   is the only bound on the operation; the named liveness backstops
//!   ([`LIVENESS_OP`], [`LIVENESS_CLEANUP`], [`LIVENESS_SCAN_START`]) apply
//!   only when the caller gave no budget ([`Budget::unbounded`]), and a
//!   backstop expiry reports `operation.timed-out` with the detail
//!   [`LIVENESS_BACKSTOP_DETAIL`], never the caller's deadline.
//! * [`OpTicket`] receives the core operation id inside the core admission
//!   critical section (`OpTicket::publish`, called with the core lock
//!   held, no await between admission and publication). A cancel request
//!   that arrives before publication is recorded on the ticket, and the
//!   admission then cancels the op before any radio call. A cancel after
//!   publication targets exactly that one core operation.
//!
//! Lock order: `OpTicket::publish` takes the ticket lock inside the core
//! lock; [`OpTicket::request_cancel`] takes only the ticket lock and
//! releases it before anything takes the core lock. No path holds the
//! ticket lock while waiting for the core lock.

use std::sync::{
    Arc, Mutex as StdMutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;
use ubm_core::contracts::{MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, OperationId};

/// Liveness backstop for connect, discover and GATT verbs when the caller
/// gave no budget. Not a caller deadline.
pub const LIVENESS_OP: Duration = Duration::from_secs(120);
/// Liveness backstop for cleanup (scan stop, unsubscribe, disconnect) when
/// the caller gave no budget.
pub const LIVENESS_CLEANUP: Duration = Duration::from_secs(10);
/// Liveness backstop for the OS scan start when the caller gave no budget.
/// It bounds the start only: a started scan runs until stopped.
pub const LIVENESS_SCAN_START: Duration = Duration::from_secs(30);
/// Bound for background compensation (a half-open link after a failed or
/// abandoned connect, a late enable nobody owns). Compensation never
/// stalls the already-settled operation it cleans up for.
pub const COMPENSATION_TIMEOUT: Duration = Duration::from_secs(1);
/// Error detail of an `operation.timed-out` raised by a liveness backstop
/// rather than by the caller's own budget.
pub const LIVENESS_BACKSTOP_DETAIL: &str = "liveness-backstop";

/// The caller's deadline for one operation. `deadline == None` means the
/// caller gave no budget; the operation is then bounded by the liveness
/// backstop that fits its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    deadline: Option<Instant>,
}

impl Budget {
    /// No caller budget: only the liveness backstop bounds the operation.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self { deadline: None }
    }

    /// A caller budget of `ms` milliseconds measured from `start`, the
    /// instant the host received the call. Relative budgets never depend
    /// on another process's clock.
    #[must_use]
    pub fn from_ms_at(start: Instant, ms: u64) -> Self {
        Self {
            deadline: Some(
                start
                    .checked_add(Duration::from_millis(ms))
                    .unwrap_or_else(|| start + Duration::from_millis(MAX_TIMEOUT_MS)),
            ),
        }
    }

    /// A caller budget of `ms` milliseconds measured from now.
    #[must_use]
    pub fn from_ms(ms: u64) -> Self {
        Self::from_ms_at(Instant::now(), ms)
    }

    /// The caller deadline, if the caller gave one.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Time left on the caller budget (`Some(ZERO)` once expired), or
    /// `None` without a caller budget.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Whether the caller budget has run out. Never true without one.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// The bound one wait gets: the remaining caller budget when there is
    /// one, else `liveness`.
    #[must_use]
    pub fn bound(&self, liveness: Duration) -> Duration {
        self.remaining().unwrap_or(liveness)
    }

    /// The absolute deadline one whole operation runs under, fixed once so
    /// multi-step operations (MTU lookup then write) share one window.
    pub(crate) fn window(&self, liveness: Duration) -> Window {
        match self.deadline {
            Some(at) => Window {
                at: Some(at),
                backstop: false,
            },
            None => Window {
                at: Some(Instant::now() + liveness),
                backstop: true,
            },
        }
    }

    /// The window of an operation that waits as long as the OS does when
    /// the caller gave no budget (finding 112: a connect, like the legacy
    /// pending CoreBluetooth connect and Android `autoConnect`). The
    /// caller's budget, when given, is still the deadline; a cancel always
    /// ends the wait.
    pub(crate) fn window_without_backstop(&self) -> Window {
        Window {
            at: self.deadline,
            backstop: false,
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::unbounded()
    }
}

/// One operation's effective deadline: the caller's, or a backstop's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    /// `None`: no deadline; only a cancel or the OS ends the wait.
    pub(crate) at: Option<Instant>,
    /// True when the deadline is a liveness backstop, not the caller's.
    pub(crate) backstop: bool,
}

impl Window {
    /// The window's remaining time as a kernel admission timeout, clamped
    /// to the kernel's accepted range (an expired window still admits with
    /// the minimum; the admission path refuses expired budgets first).
    /// A window without a deadline admits with the kernel's largest
    /// timeout; the kernel is never swept on this host, so the admission
    /// timeout never ends the wait by itself.
    pub(crate) fn core_timeout_ms(&self) -> u64 {
        let Some(at) = self.at else {
            return MAX_TIMEOUT_MS;
        };
        let remaining = at.saturating_duration_since(Instant::now());
        u64::try_from(remaining.as_millis())
            .unwrap_or(MAX_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TicketState {
    Pending {
        cancel_requested: bool,
    },
    Admitted {
        id: OperationId,
        cancel_requested: bool,
    },
    Settled,
}

#[derive(Debug)]
struct TicketInner {
    state: StdMutex<TicketState>,
    notify: Notify,
    /// The adapter was reset under this operation (finding 57): its
    /// driver stops waiting and answers `operation.reset`.
    reset: AtomicBool,
}

/// What a cancel request found on the ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelRequest {
    /// The operation has no core id yet. The request is recorded: the
    /// admission cancels the operation before any radio call.
    RecordedBeforeAdmission,
    /// The operation holds this core id; cancel exactly that operation.
    Forward(OperationId),
    /// The operation already finished; nothing to cancel.
    AlreadySettled,
}

/// Publication refused: the ticket was cancelled before the operation was
/// admitted, so the admission must cancel it before any radio call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelledBeforeAdmission;

/// Cancellation handle for one operation. Clone it to keep a handle the
/// host can cancel through while the operation runs.
#[derive(Debug, Clone)]
pub struct OpTicket(Arc<TicketInner>);

impl Default for OpTicket {
    fn default() -> Self {
        Self::new()
    }
}

impl OpTicket {
    /// A fresh ticket for one operation.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(TicketInner {
            state: StdMutex::new(TicketState::Pending {
                cancel_requested: false,
            }),
            notify: Notify::new(),
            reset: AtomicBool::new(false),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TicketState> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a cancel request and wake the operation's driver.
    pub fn request_cancel(&self) -> CancelRequest {
        let request = self.record_cancel();
        self.wake();
        request
    }

    /// Record a cancel request without waking the driver: the central
    /// cancels the core operation first, then wakes the driver, so the
    /// caller's own cancel is the one the core settles.
    pub(crate) fn record_cancel(&self) -> CancelRequest {
        let mut state = self.lock();
        match &mut *state {
            TicketState::Pending { cancel_requested } => {
                *cancel_requested = true;
                CancelRequest::RecordedBeforeAdmission
            }
            TicketState::Admitted {
                id,
                cancel_requested,
            } => {
                *cancel_requested = true;
                CancelRequest::Forward(id.clone())
            }
            TicketState::Settled => CancelRequest::AlreadySettled,
        }
    }

    /// Wake every task waiting in [`OpTicket::cancelled`].
    pub(crate) fn wake(&self) {
        self.0.notify.notify_waiters();
    }

    /// Whether a cancel has been requested and the operation is not yet
    /// settled.
    #[must_use]
    pub fn is_cancel_requested(&self) -> bool {
        matches!(
            &*self.lock(),
            TicketState::Pending {
                cancel_requested: true
            } | TicketState::Admitted {
                cancel_requested: true,
                ..
            }
        )
    }

    /// The core operation id, once published.
    #[must_use]
    pub fn operation_id(&self) -> Option<OperationId> {
        match &*self.lock() {
            TicketState::Admitted { id, .. } => Some(id.clone()),
            TicketState::Pending { .. } | TicketState::Settled => None,
        }
    }

    /// Whether the operation has finished (the id is no longer live here).
    #[must_use]
    pub fn is_settled(&self) -> bool {
        matches!(&*self.lock(), TicketState::Settled)
    }

    /// Resolve once a cancel is requested or the adapter was reset under
    /// the operation. Never resolves otherwise.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before checking so a request between the check and
            // the await still wakes this waiter.
            notified.as_mut().enable();
            if self.is_cancel_requested() || self.is_reset() {
                return;
            }
            notified.await;
        }
    }

    /// Record that the adapter was reset under this operation and wake its
    /// driver (finding 57). A settled operation is left alone.
    pub(crate) fn mark_reset(&self) {
        if self.is_settled() {
            return;
        }
        self.0.reset.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Whether the adapter was reset under this operation.
    #[must_use]
    pub fn is_reset(&self) -> bool {
        self.0.reset.load(Ordering::SeqCst)
    }

    /// The error a driver woken by [`OpTicket::cancelled`] answers:
    /// `operation.reset` when the adapter was reset under the operation,
    /// else `operation.aborted`.
    #[must_use]
    pub fn interruption(&self, operation: &str) -> crate::errors::DesktopError {
        if self.is_reset() {
            crate::errors::DesktopError::new(
                ubm_core::contracts::BleErrorCode::OperationReset,
                ubm_core::contracts::BleErrorDomain::Connection,
                operation,
            )
            .with_detail("the adapter was reset under the operation")
        } else {
            crate::errors::DesktopError::cancelled(operation)
        }
    }

    /// Publish the core id. Call with the core lock held, right after the
    /// core admitted the operation. Refused when the ticket was cancelled
    /// before admission; the caller then cancels the admitted op itself.
    pub(crate) fn publish(&self, id: &OperationId) -> Result<(), CancelledBeforeAdmission> {
        let mut state = self.lock();
        match &*state {
            TicketState::Pending {
                cancel_requested: false,
            } => {
                *state = TicketState::Admitted {
                    id: id.clone(),
                    cancel_requested: false,
                };
                Ok(())
            }
            TicketState::Pending {
                cancel_requested: true,
            } => {
                *state = TicketState::Admitted {
                    id: id.clone(),
                    cancel_requested: true,
                };
                Err(CancelledBeforeAdmission)
            }
            // A ticket drives exactly one operation: a second publication
            // (ticket reuse) or a publication after settlement is refused
            // the same way, so no second op ever runs under one handle.
            TicketState::Admitted { .. } | TicketState::Settled => Err(CancelledBeforeAdmission),
        }
    }

    /// Mark the operation finished. Later cancel requests report
    /// [`CancelRequest::AlreadySettled`].
    pub(crate) fn settle(&self) {
        *self.lock() = TicketState::Settled;
    }
}

/// Settles the ticket when the operation's future ends, on every path
/// (return, error, drop).
pub(crate) struct SettleOnDrop<'a>(pub(crate) &'a OpTicket);

impl Drop for SettleOnDrop<'_> {
    fn drop(&mut self) {
        self.0.settle();
    }
}

/// Budget plus ticket for one operation.
#[derive(Debug, Clone, Default)]
pub struct OpControl {
    /// The caller's deadline.
    pub budget: Budget,
    /// The operation's cancellation handle.
    pub ticket: OpTicket,
}

impl OpControl {
    /// Control from an explicit budget and ticket.
    #[must_use]
    pub fn new(budget: Budget, ticket: OpTicket) -> Self {
        Self { budget, ticket }
    }

    /// No caller budget, fresh ticket.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::default()
    }

    /// A caller budget of `ms` milliseconds from now, fresh ticket.
    #[must_use]
    pub fn budget_ms(ms: u64) -> Self {
        Self::new(Budget::from_ms(ms), OpTicket::new())
    }
}

/// What [`crate::DesktopCentral::cancel`] did with a cancel request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelAck {
    /// Recorded before admission: the operation will end
    /// `operation.aborted` without reaching the radio. Operations that never
    /// hold a core id (discover, disconnect, scan stop, unsubscribe) also
    /// answer this: their driver abandons its wait.
    RecordedBeforeAdmission,
    /// Forwarded to exactly this core operation; `outcome` is the core's
    /// answer to the cancel.
    Forwarded {
        /// The one core operation the cancel targeted.
        operation: OperationId,
        /// The core's settlement answer.
        outcome: ubm_core::central::CompletionOutcome,
    },
    /// The operation had already finished.
    AlreadySettled,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;
    use ubm_core::contracts::OperationId;

    use super::{
        Budget, CancelRequest, CancelledBeforeAdmission, LIVENESS_OP, OpControl, OpTicket,
        SettleOnDrop,
    };

    fn op(id: &str) -> OperationId {
        OperationId::new(id).expect("op id")
    }

    #[tokio::test(start_paused = true)]
    async fn budget_counts_from_its_start_instant() {
        let start = Instant::now();
        tokio::time::advance(Duration::from_millis(30)).await;
        let budget = Budget::from_ms_at(start, 50);
        assert_eq!(budget.remaining(), Some(Duration::from_millis(20)));
        assert!(!budget.is_expired());
        tokio::time::advance(Duration::from_millis(20)).await;
        assert!(budget.is_expired(), "queueing time counts against it");
        assert_eq!(budget.remaining(), Some(Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn caller_budget_is_the_only_bound_when_given() {
        let long = Budget::from_ms(200_000);
        assert_eq!(long.bound(LIVENESS_OP), Duration::from_millis(200_000));
        let window = long.window(LIVENESS_OP);
        assert!(!window.backstop, "caller budget is not a backstop");
        let unbounded = Budget::unbounded();
        assert_eq!(unbounded.bound(LIVENESS_OP), LIVENESS_OP);
        assert!(unbounded.window(LIVENESS_OP).backstop);
        assert!(!unbounded.is_expired(), "no budget never expires");
    }

    #[tokio::test(start_paused = true)]
    async fn window_core_timeout_clamps_to_the_kernel_range() {
        let expired = Budget::from_ms(0).window(LIVENESS_OP);
        assert_eq!(expired.core_timeout_ms(), 1, "kernel minimum");
        let normal = Budget::from_ms(5000).window(LIVENESS_OP);
        assert_eq!(normal.core_timeout_ms(), 5000);
    }

    #[test]
    fn cancel_before_publication_is_recorded_and_refuses_publication() {
        let ticket = OpTicket::new();
        assert_eq!(
            ticket.request_cancel(),
            CancelRequest::RecordedBeforeAdmission
        );
        assert!(ticket.is_cancel_requested());
        assert_eq!(ticket.publish(&op("op-1")), Err(CancelledBeforeAdmission));
        assert_eq!(
            ticket.operation_id(),
            Some(op("op-1")),
            "the refused op id stays attributable"
        );
    }

    #[test]
    fn cancel_after_publication_forwards_the_exact_id() {
        let ticket = OpTicket::new();
        ticket.publish(&op("op-7")).expect("publish");
        assert_eq!(ticket.request_cancel(), CancelRequest::Forward(op("op-7")));
        ticket.settle();
        assert_eq!(ticket.request_cancel(), CancelRequest::AlreadySettled);
        assert!(
            !ticket.is_cancel_requested(),
            "settled is not pending cancel"
        );
    }

    #[test]
    fn a_ticket_publishes_exactly_once() {
        let ticket = OpTicket::new();
        ticket.publish(&op("op-1")).expect("first");
        assert_eq!(ticket.publish(&op("op-2")), Err(CancelledBeforeAdmission));
        assert_eq!(ticket.operation_id(), Some(op("op-1")), "first id kept");
    }

    #[test]
    fn settle_guard_settles_on_every_exit() {
        let ticket = OpTicket::new();
        {
            let _guard = SettleOnDrop(&ticket);
        }
        assert!(ticket.is_settled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_wakes_on_request_from_another_task() {
        let control = OpControl::unbounded();
        let waiter = tokio::spawn({
            let ticket = control.ticket.clone();
            async move { ticket.cancelled().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        control.ticket.request_cancel();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("cancel wakes the waiter")
            .expect("waiter joins");
    }

    #[tokio::test]
    async fn cancelled_resolves_immediately_when_already_requested() {
        let ticket = OpTicket::new();
        ticket.request_cancel();
        tokio::time::timeout(Duration::from_millis(50), ticket.cancelled())
            .await
            .expect("already-cancelled ticket resolves without a wake");
    }
}
