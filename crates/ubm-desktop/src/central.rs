//! Host-neutral desktop central adapter (HOST-DESKTOP).
//!
//! [`DesktopCentral`] drives the real [`ubm_core::central::Central`] over a
//! [`RadioBoundary`](crate::boundary::RadioBoundary): validation, ownership,
//! generations, and subscription sharing stay in the core; this layer
//! translates radio outcomes into core settlements with contract error
//! identities. Scan cleanup, partial discovery failures, descriptor paths,
//! and cancellation are handled here, never in the radio backend.
//!
//! Execution: one shared desktop executor for the process
//! ([`crate::executor`]); this type never builds a runtime. All async work
//! must run on the shared handle. There is no BLE hardware on the
//! qualification host: physical proof stays queued (see `PARITY_GAPS.md`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, watch};
use ubm_core::central::{
    Central, CentralEffectKind, CompletionOutcome, PathSelector, StoredPath, canonical_uuid,
    validate_scan_request,
};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, BleErrorCode, BleErrorDomain, ContenderKind, Generation, OperationId,
    OperationTerminalKind,
};
use ubm_core::ownership::{CleanupRecord, EffectBatch};

use crate::boundary::{
    InstanceKey, PeerSnapshot, RadioBoundary, RadioCloseFailure, RadioEvent, ScanFilterSpec,
};
use crate::errors::DesktopError;

/// Effect batch capacity per core call (matches the core's own default).
const EFFECT_BATCH_CAP: usize = 64;
/// Safety bound, not host policy: a btleplug disconnect that never resolves
/// (peripheral already dropped from the OS map) becomes a bounded,
/// classified outcome instead of hanging the caller.
const DISCONNECT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);
/// Default subscription stream bounds (items, bytes).
const DEFAULT_SUB_ITEM_CAP: u64 = 64;
const DEFAULT_SUB_BYTE_CAP: u64 = 8192;
/// ATT protocol ceiling for one write (ATT_MTU max 512 minus 3 bytes of
/// opcode/handle). A protocol constant, not a measurement: the OS-measured
/// MTU still gates every write through [`Central::maximum_write_length`].
const ATT_MAX_WRITE: u64 = 509;
/// Bound for the per-central scan-observation queue (F22): every
/// advertisement the event loop ingests stays pollable FIFO until the host
/// takes it. Beyond this the oldest observation evicts and every eviction
/// counts through [`DesktopCentral::advertisement_overflow_count`], never
/// silently: a host that polls slower than the radio sees the loss
/// explicitly instead of an unbounded queue.
const ADVERTISEMENT_CAP: usize = 256;

static EPOCH: OnceLock<Instant> = OnceLock::new();
static OPEN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Monotonic milliseconds for core calls.
fn now_ms() -> u64 {
    let start = EPOCH.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn batch() -> EffectBatch {
    EffectBatch::new(EFFECT_BATCH_CAP)
}

/// Drain typed observations to recycle ledger capacity (F02). Observations are
/// not hidden: callers needing them (F11) inspect the staged suffix before
/// this drain runs.
fn recycle_observations(core: &mut Central) {
    let _ = core.drain_typed_effects();
}

/// Report the actual host release for a terminal op (F02). Live ops stay live
/// (a release would fail with `live`); already-reaped ops are ignored so the
/// canceller and the op driver may both attempt the report idempotently.
fn report_terminal_release(
    core: &mut Central,
    op: &OperationId,
    ok: bool,
    code: Option<BleErrorCode>,
) {
    let terminal = matches!(
        core.operation_state(op),
        Some(ubm_core::ownership::OpStateView::Terminal(_))
    );
    if !terminal {
        return;
    }
    let _ = if ok {
        core.report_release_success(op)
    } else {
        core.report_release_failure(op, code.unwrap_or(BleErrorCode::PlatformFailure))
    };
}

/// Map an authoritative core terminal to a caller error (F03). The radio
/// result lost the race; the core outcome is the only caller result.
fn terminal_to_error(kind: OperationTerminalKind, operation: &'static str) -> DesktopError {
    match kind {
        OperationTerminalKind::Succeeded => contract_error(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
            operation,
        )
        .with_detail("core succeeded without a radio value"),
        OperationTerminalKind::Failed => contract_error(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Core,
            operation,
        ),
        OperationTerminalKind::Aborted => DesktopError::cancelled(operation),
        OperationTerminalKind::TimedOut => contract_error(
            BleErrorCode::OperationTimedOut,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::Disconnected => contract_error(
            BleErrorCode::OperationDisconnected,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::Reset => contract_error(
            BleErrorCode::OperationReset,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::AdapterUnavailable => contract_error(
            BleErrorCode::OperationAdapterUnavailable,
            BleErrorDomain::Connection,
            operation,
        ),
        OperationTerminalKind::Destroyed => contract_error(
            BleErrorCode::LifecycleDestroyed,
            BleErrorDomain::Core,
            operation,
        ),
    }
}

/// Observe the current terminal kind for an op already settled (duplicate
/// settlement path). Returns `None` when the op is live or already reaped.
fn terminal_kind_of(core: &Central, op: &OperationId) -> Option<OperationTerminalKind> {
    match core.operation_state(op) {
        Some(ubm_core::ownership::OpStateView::Terminal(kind)) => Some(kind),
        // Shutdown-reaped while this driver still awaited its radio: the
        // tombstone keeps the genuine winner observable (F15).
        _ => core.shutdown_terminal_kind(op),
    }
}

/// Settle one op with a genuine valid contender, recycle observations, and
/// report the actual host release (F02/F25). Genuine radio outcomes are always
/// valid contenders; `invalid` is reserved for stale generations the core must
/// ignore. Returns the authoritative settlement outcome. A fresh settlement
/// releases immediately; a duplicate leaves the terminal in place so the caller
/// can observe the winning kind before reporting the release itself.
fn settle_and_release(
    core: &mut Central,
    op: &OperationId,
    kind: ContenderKind,
    cleanup_ok: bool,
    cleanup_code: Option<BleErrorCode>,
    out: &mut EffectBatch,
) -> Result<CompletionOutcome, DesktopError> {
    let outcome = core
        .settle_op(op, kind, true, 0, now_ms(), out)
        .map_err(DesktopError::from)?;
    let _ = out.drain();
    recycle_observations(core);
    if matches!(outcome, CompletionOutcome::Settled { .. }) {
        report_terminal_release(core, op, cleanup_ok, cleanup_code);
    }
    Ok(outcome)
}

/// Settle one op, then report the duplicate path's release after the caller
/// observes the winning terminal (F03). Returns the pre-release terminal kind.
fn release_duplicate(
    core: &mut Central,
    op: &OperationId,
    cleanup_ok: bool,
    cleanup_code: Option<BleErrorCode>,
) -> Option<OperationTerminalKind> {
    let kind = terminal_kind_of(core, op);
    report_terminal_release(core, op, cleanup_ok, cleanup_code);
    recycle_observations(core);
    kind
}

/// Per-op detached cleanup for a dropped caller future (F03).
#[derive(Debug)]
enum DropCleanup {
    /// Plain ops (read/write/descriptors): cancel + reap.
    Op,
    /// Connect: also record peer loss + a compensating disconnect (mirrors
    /// the timeout arm, aborted instead of timed out).
    Connect { peer_id: String, peer_key: String },
    /// Subscribe: also fail the shared enable + remove routing + sweep
    /// (mirrors the timeout arm, aborted instead of timed out).
    Subscribe { key: InstanceKey, path_index: usize },
}

/// Cancels + reaps a dispatched op when its awaiting caller future is
/// dropped (task abort, select loss). Without this the core keeps the op
/// dispatched and live forever: the driver that would have settled it is
/// gone. Normal completion defuses the guard; only the drop path fires.
///
/// Abort cleanup runs on the runtime, so `Drop` can spawn the detached
/// task. An unpolled future dropped off-runtime has no in-flight radio to
/// reap, so the guard stays silent there instead of panicking in `Drop`.
struct CancelOnDrop<B: RadioBoundary> {
    central: Option<DesktopCentral<B>>,
    operation: Option<OperationId>,
    cleanup: Option<DropCleanup>,
}

impl<B: RadioBoundary> CancelOnDrop<B> {
    fn armed(central: &DesktopCentral<B>, operation: OperationId, cleanup: DropCleanup) -> Self {
        Self {
            central: Some(central.clone()),
            operation: Some(operation),
            cleanup: Some(cleanup),
        }
    }

    fn defuse(&mut self) {
        self.central.take();
    }
}

impl<B: RadioBoundary> Drop for CancelOnDrop<B> {
    fn drop(&mut self) {
        let (Some(central), Some(operation), Some(cleanup)) = (
            self.central.take(),
            self.operation.take(),
            self.cleanup.take(),
        ) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(run_drop_cleanup(central, operation, cleanup));
    }
}

/// Report a terminal op's release when the driver is gone (drop path only).
/// Unlike `release_duplicate`, this never reports a live op.
async fn reap_if_terminal<B>(central: &DesktopCentral<B>, operation: &OperationId) {
    let mut core = central.inner.core.lock().await;
    if terminal_kind_of(&core, operation).is_some() {
        report_terminal_release(&mut core, operation, true, None);
        recycle_observations(&mut core);
    }
}

async fn run_drop_cleanup<B: RadioBoundary>(
    central: DesktopCentral<B>,
    operation: OperationId,
    cleanup: DropCleanup,
) {
    match cleanup {
        DropCleanup::Op => {
            let _ = central.cancel_operation(&operation).await;
            reap_if_terminal(&central, &operation).await;
        }
        DropCleanup::Connect { peer_id, peer_key } => {
            {
                let mut core = central.inner.core.lock().await;
                let mut out = batch();
                let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                let _ = out.drain();
            }
            let _ = central.cancel_operation(&operation).await;
            reap_if_terminal(&central, &operation).await;
            // A half-open OS link must not linger ownerless. Bounded and
            // outside the core lock per F24.
            match tokio::time::timeout(
                DISCONNECT_COMPLETION_TIMEOUT,
                central.inner.boundary.disconnect(&peer_id),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let mut core = central.inner.core.lock().await;
                    let _ = core.report_disconnect_failure(&peer_key, error.code());
                }
                Err(_) => {
                    let mut core = central.inner.core.lock().await;
                    let _ =
                        core.report_disconnect_failure(&peer_key, BleErrorCode::OperationTimedOut);
                }
            }
        }
        DropCleanup::Subscribe { key, path_index } => {
            central.inner.subscriptions.lock().await.remove(&key);
            let _ = central.cancel_operation(&operation).await;
            {
                let mut core = central.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
            }
            reap_if_terminal(&central, &operation).await;
        }
    }
}

/// Release every terminal op whose cleanup already succeeded (F02): joiners
/// settled by a shared enable, immediate-success shares on an enabled hub,
/// and disable tickets settled by `settle_subscribe_disable`. Each has no
/// per-op OS resource beyond the shared CCCD the driver already handled, so
/// success is the truthful receipt. Ops with failed cleanup are reported by
/// their own driver with failure and are already gone from this list.
fn sweep_terminal_successes(core: &mut Central) {
    for id in core.terminal_operation_ids() {
        let _ = core.report_release_success(&id);
    }
    recycle_observations(core);
}

fn contract_error(code: BleErrorCode, domain: BleErrorDomain, operation: &str) -> DesktopError {
    DesktopError::new(code, domain, operation)
}

/// Frozen wire string for a core error code (skipped-path reporting keeps
/// identities, not debug strings).
fn core_code_str(error: &ubm_core::contracts::CoreError) -> &'static str {
    error.code().as_str()
}

/// One live scan owned by this central (the core arbitrates one physical
/// scan; a second start fails with `scan.already-active`).
#[derive(Debug, Clone)]
pub struct ScanSession {
    id: OperationId,
}

impl ScanSession {
    /// Core operation id backing this scan.
    #[must_use]
    pub fn operation_id(&self) -> &OperationId {
        &self.id
    }
}

/// Handle for one connected peer.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    /// Session peer key in the core.
    pub peer_key: String,
    /// Connection generation minted by the core.
    pub connection_generation: Option<String>,
}

/// Partial-failure report for discovery: usable paths register, unusable
/// entries are skipped with their identities instead of failing the whole
/// snapshot.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    /// Paths registered (service, characteristic, and descriptor levels).
    pub paths_registered: usize,
    /// `(uuid, code)` for every skipped entry, in discovery order.
    pub skipped: Vec<(String, String)>,
}

/// One registered discovery path in the current snapshot (F01). Consumers
/// build [`PathSelector`] values and render databases from this whole-tree
/// read; `discover` alone returns counts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPath {
    /// Canonical service UUID.
    pub service_uuid: String,
    /// Service occurrence among duplicate UUIDs.
    pub service_occurrence: u64,
    /// Characteristic UUID (`None` = service-level path).
    pub characteristic_uuid: Option<String>,
    /// Characteristic occurrence (`None` = service-level path).
    pub characteristic_occurrence: Option<u64>,
    /// Descriptor UUID (`None` = no descriptor level).
    pub descriptor_uuid: Option<String>,
    /// Descriptor occurrence (`None` = no descriptor level).
    pub descriptor_occurrence: Option<u64>,
    /// GATT property bits (`GATT_PROP_*`, characteristic level only).
    pub properties: u8,
}

/// Authoritative per-central shutdown outcome (F14/F15): the final
/// cleanup record plus every close-time release failure. A clean shutdown
/// reports `Released` with no radio failures; anything else names exactly
/// what did not release.
#[derive(Debug)]
pub struct ShutdownReport {
    /// Final core cleanup record, taken only after every queued op settled,
    /// every dispatched remainder was answered, and every terminal release
    /// was acknowledged (F15). `Released` only when disconnect failures and
    /// retained release failures are all absent; otherwise `ReleaseFailed`
    /// with every failure preserved. `Err` only when the destroy drive
    /// itself failed (a core invariant violation), never for radio faults —
    /// those land in the record or in `radio_close_failures`.
    pub record: Result<CleanupRecord, DesktopError>,
    /// Close-time native release failures drained from the radio (F14
    /// receipts): one entry per characteristic scope whose unsubscribe did
    /// not complete. Empty means every live scope released.
    pub radio_close_failures: Vec<RadioCloseFailure>,
    /// Incremental destroy passes executed (F15): more than one when the
    /// destroy workload exceeds one effect batch.
    pub destroy_steps: usize,
}

/// Typed notification poll outcome (F17): empty-but-live is distinct from
/// terminal/closed, and exactly one overflow terminal stays observable even
/// when data capacity is exhausted. Values drain before the terminal; the
/// terminal is observed once, then the stream reports closed, never
/// live-empty again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationPoll {
    /// One buffered value (FIFO arrival order).
    Value(Vec<u8>),
    /// No value waiting, stream still live.
    Empty,
    /// Overflow terminal with loss details (exactly once).
    Terminal(ubm_core::central::SubscriptionTerminal),
    /// Hub invalidated (service change/disconnect): stale, resubscribe.
    Invalidated,
    /// Consumer removed/closed or failed terminal already consumed.
    Closed,
}

struct ActiveScan {
    id: OperationId,
}

/// Build the per-instance radio address from the resolved stored path
/// (F18). A level the selector leaves unspecified still resolved to
/// exactly one candidate, but that candidate is not necessarily occurrence
/// 0 — a characteristic can live only under the second instance of a
/// duplicated service — so the stored path carries the addressed
/// instance, never the request. `characteristic` is the caller-validated
/// characteristic UUID from that same stored path.
fn instance_key(peer_id: &str, stored: &StoredPath, characteristic: &str) -> InstanceKey {
    (
        peer_id.to_owned(),
        stored.service_uuid().to_owned(),
        stored.service_occurrence(),
        characteristic.to_owned(),
        stored.characteristic_occurrence().unwrap_or(0),
    )
}

struct Inner<B> {
    core: Mutex<Central>,
    boundary: B,
    scan: Mutex<Option<ActiveScan>>,
    /// Radio peripheral id -> core session peer key.
    peers: Mutex<HashMap<String, String>>,
    /// Per-instance subscription routing: (peer, service uuid, service
    /// occurrence, characteristic uuid, characteristic occurrence) ->
    /// (core path, subscription epoch at install). Duplicate UUIDs never
    /// share routing, and a queued value whose epoch no longer matches the
    /// live routing never delivers (F10).
    subscriptions: Mutex<HashMap<InstanceKey, (usize, u64)>>,
    /// Current subscription epoch per radio peer. Bumped every time the
    /// peer's routing invalidates (disconnect, loss, service change), so
    /// forwarders installed before the bump stamp a dead generation.
    epochs: Mutex<HashMap<String, u64>>,
    /// Per-instance keys whose physical disable failed and is pending
    /// retry through `unsubscribe`. A pending key fails new subscribes
    /// closed until the disable completes.
    failed_disables: Mutex<HashSet<InstanceKey>>,
    /// Scan observations ingested by the event loop, FIFO (F22): the full
    /// [`PeerSnapshot`] facts (name, services, manufacturer data, RSSI)
    /// the host matcher consumes. Bounded by [`ADVERTISEMENT_CAP`]; the
    /// oldest evicts past the cap and every eviction counts in
    /// `advertisement_drops`.
    advertisements: Mutex<VecDeque<PeerSnapshot>>,
    /// Observations evicted past [`ADVERTISEMENT_CAP`] (F22): bounded
    /// ingress never grows memory, and drops are counted, never silent.
    advertisement_drops: AtomicU64,
    shut_down: AtomicBool,
    /// Stop signal for the central-lifetime event loop.
    loop_stop: watch::Sender<bool>,
    /// Event-loop worker, joined at shutdown so no advertisement can race
    /// cleanup after the central is gone.
    loop_done: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Host-neutral desktop central: the real core over a mockable radio.
pub struct DesktopCentral<B> {
    inner: Arc<Inner<B>>,
}

/// Clone shares one central (one attachment scope, one scan owner).
impl<B> Clone for DesktopCentral<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Open a central over `boundary` with a fresh attachment scope.
    /// `owner` labels the attachment (host identity, e.g. `"node"`).
    ///
    /// Must be called on the shared desktop executor: the central-lifetime
    /// event loop spawns on the ambient runtime, and per-manager runtimes
    /// are forbidden.
    pub async fn open(boundary: B, owner: &str) -> Result<Self, DesktopError> {
        // L4: a shut-down executor admits no new centrals — a post-shutdown
        // open fails closed instead of building a zombie central whose ops
        // refuse admission.
        if crate::executor::is_desktop_runtime_shut_down() {
            return Err(contract_error(
                BleErrorCode::AdapterUnavailable,
                BleErrorDomain::Adapter,
                "desktop.open",
            ));
        }
        if owner.is_empty() {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "desktop.owner",
            ));
        }
        // L6: never synthesize an adapter identity — a withheld readout
        // fails the open instead of labelling the adapter "unknown".
        let adapter_label = boundary.adapter_name().await?;
        let ordinal = OPEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let attachment = AttachmentTuple::new(
            AttachmentId::new(format!("desktop-attachment-{ordinal}"))
                .map_err(DesktopError::from)?,
            BackendInstanceId::new(format!("ubm-desktop-btleplug-{owner}"))
                .map_err(DesktopError::from)?,
            BackendGeneration::new(format!("desktop-backend-gen-{ordinal}"))
                .map_err(DesktopError::from)?,
            AdapterId::new(adapter_label).map_err(DesktopError::from)?,
            AdapterGeneration::new(format!("desktop-adapter-gen-{ordinal}"))
                .map_err(DesktopError::from)?,
        );
        let generation =
            Generation::new(format!("desktop-kernel-gen-{ordinal}")).map_err(DesktopError::from)?;
        let mut core = Central::new(
            attachment,
            generation,
            ubm_core::central::CentralConfig::default(),
        )
        .map_err(DesktopError::from)?;
        // M4: project desktop capability truth into the live core so
        // runtime gates match the parity report row for row.
        crate::capabilities::register_desktop_capabilities(&mut core)
            .map_err(DesktopError::from)?;
        let (loop_stop, loop_stop_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            core: Mutex::new(core),
            boundary,
            scan: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            epochs: Mutex::new(HashMap::new()),
            failed_disables: Mutex::new(HashSet::new()),
            advertisements: Mutex::new(VecDeque::new()),
            advertisement_drops: AtomicU64::new(0),
            shut_down: AtomicBool::new(false),
            loop_stop,
            loop_done: Mutex::new(None),
        });
        let worker = tokio::spawn(scan_loop(Arc::clone(&inner), loop_stop_rx));
        *inner.loop_done.lock().await = Some(worker);
        Ok(Self { inner })
    }

    /// Borrow the radio boundary (event injection in tests runs through the
    /// boundary handle, not the central).
    #[must_use]
    pub fn boundary(&self) -> &B {
        &self.inner.boundary
    }

    fn admit(&self, operation: &str) -> Result<(), DesktopError> {
        if self.inner.shut_down.load(Ordering::SeqCst)
            || crate::executor::is_desktop_runtime_shut_down()
        {
            return Err(DesktopError::adapter_unavailable(operation));
        }
        Ok(())
    }

    /// Settle a dispatched op that hit its end-to-end deadline (F03) and map
    /// the authoritative outcome to the caller error. A duplicate (already
    /// terminal via cancel/disconnect) returns the winning terminal, never a
    /// synthesized timeout.
    async fn settle_timeout(&self, operation: &OperationId, op_name: &'static str) -> DesktopError {
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let outcome = settle_and_release(
            &mut core,
            operation,
            ContenderKind::Timeout,
            true,
            None,
            &mut out,
        );
        match outcome {
            Ok(CompletionOutcome::Settled {
                kind: OperationTerminalKind::TimedOut,
                ..
            })
            | Ok(CompletionOutcome::ContenderIgnored)
            | Err(_) => contract_error(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Connection,
                op_name,
            ),
            Ok(CompletionOutcome::Settled { kind, .. }) => terminal_to_error(kind, op_name),
            Ok(CompletionOutcome::DuplicateSuppressed { .. }) => {
                let kind = release_duplicate(&mut core, operation, true, None);
                match kind {
                    Some(winner) => terminal_to_error(winner, op_name),
                    None => contract_error(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Connection,
                        op_name,
                    ),
                }
            }
        }
    }

    /// Whether explicit shutdown has been recorded.
    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.inner.shut_down.load(Ordering::SeqCst)
    }

    /// Whether a scan session is currently owned.
    pub async fn has_active_scan(&self) -> bool {
        self.inner.scan.lock().await.is_some()
    }

    /// Core session peer key for a radio peripheral id, if resolved.
    pub async fn peer_key_for(&self, peer_id: &str) -> Option<String> {
        self.inner.peers.lock().await.get(peer_id).cloned()
    }

    /// Take one ingested scan observation, FIFO arrival order (F22): the
    /// full [`PeerSnapshot`] facts (name, services, manufacturer data,
    /// RSSI) the host matcher consumes. `None` means no observation is
    /// waiting. Observations queue from central open onward (the boundary
    /// only emits while its radio is active). Shutdown seals the queue —
    /// the event loop is joined, so no new observation can arrive — and
    /// already-queued observations stay drainable, so a racing host never
    /// loses the terminal sighting.
    pub async fn take_advertisement(&self) -> Option<PeerSnapshot> {
        self.inner.advertisements.lock().await.pop_front()
    }

    /// Observations evicted past the queue cap (F22): a host that polls
    /// slower than the radio sees the loss explicitly here, never as
    /// silent memory growth or silent drops.
    #[must_use]
    pub fn advertisement_overflow_count(&self) -> u64 {
        self.inner.advertisement_drops.load(Ordering::Relaxed)
    }

    /// Current subscription epoch for a radio peer (F10): the generation
    /// a forwarder installed now would capture. Fresh peers start at 0;
    /// every routing invalidation bumps it.
    async fn routing_epoch(&self, peer_id: &str) -> u64 {
        *self
            .inner
            .epochs
            .lock()
            .await
            .entry(peer_id.to_owned())
            .or_insert(0)
    }

    /// Build a validated path selector with canonical UUIDs. Occurrence
    /// disambiguates duplicate UUIDs; UUID alone never identifies a path.
    pub fn selector(
        service_uuid: &str,
        service_occurrence: Option<u64>,
        characteristic_uuid: Option<&str>,
        characteristic_occurrence: Option<u64>,
        descriptor_uuid: Option<&str>,
        descriptor_occurrence: Option<u64>,
    ) -> Result<PathSelector, DesktopError> {
        let characteristic_uuid = characteristic_uuid
            .map(canonical_uuid)
            .transpose()
            .map_err(DesktopError::from)?;
        let descriptor_uuid = descriptor_uuid
            .map(canonical_uuid)
            .transpose()
            .map_err(DesktopError::from)?;
        Ok(PathSelector {
            service_uuid: canonical_uuid(service_uuid).map_err(DesktopError::from)?,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        })
    }

    /// Start a scan: validate first (no radio effect on rejection), admit in
    /// the core, then start the OS scan. A radio failure settles the core
    /// session as failed and releases the scan owner — a failed start never
    /// wedges later scans.
    pub async fn start_scan(
        &self,
        owner: &str,
        service_uuids: &[&str],
        timeout_ms: u64,
    ) -> Result<ScanSession, DesktopError> {
        self.admit("scan.start")?;
        let request = validate_scan_request(service_uuids, "all", "none", timeout_ms, true, &[])
            .map_err(DesktopError::from)?;
        let filter = ScanFilterSpec {
            service_uuids: request.service_uuids().to_vec(),
        };
        let id = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            core.start_scan(&request, None, owner, now_ms(), &mut out)
                .map_err(DesktopError::from)?
        };
        // F03: the OS start races the op deadline; a stuck start becomes a
        // start failure, not a hang.
        let start_outcome = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.inner.boundary.start_scan(filter),
        )
        .await;
        let start_outcome = match start_outcome {
            Ok(outcome) => outcome,
            Err(_) => Err(contract_error(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Connection,
                "scan.start",
            )),
        };
        if let Err(error) = start_outcome {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let _ = core.note_scan_platform(
                &id,
                ubm_core::central::ScanPlatformEvent::StartFailed,
                now_ms(),
                &mut out,
            );
            let _ = out.drain();
            let _ = core.settle_op(&id, ContenderKind::Failure, true, 0, now_ms(), &mut out);
            let _ = out.drain();
            report_terminal_release(&mut core, &id, true, None);
            recycle_observations(&mut core);
            return Err(error);
        }
        {
            let mut core = self.inner.core.lock().await;
            // `start_scan` returning `Ok` is the OS acknowledgement.
            let _ = core.platform_scan_started(&id);
        }
        {
            let mut scan = self.inner.scan.lock().await;
            *scan = Some(ActiveScan { id: id.clone() });
        }
        Ok(ScanSession { id })
    }

    /// Stop the owned scan (idempotent): move the core session to stopping,
    /// stop the OS scan, then settle the core session. An OS stop failure
    /// still settles the core session as failed and reports `scan.stop-failed`
    /// instead of swallowing cleanup. The central-lifetime event loop keeps
    /// running for connection events; it is joined only at
    /// [`DesktopCentral::shutdown`].
    pub async fn stop_scan(&self) -> Result<(), DesktopError> {
        let active = self.inner.scan.lock().await.take();
        let Some(active) = active else {
            return Ok(());
        };
        // Core first: Active -> Stopping, outside the radio await so a stuck
        // OS stop never holds the core lock. Best-effort: a session already
        // settled via source-close has nothing to stop.
        {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let _ = core.stop_scan(&active.id, now_ms(), &mut out);
            let _ = out.drain();
        }
        let stop_outcome = self.inner.boundary.stop_scan().await;
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        match stop_outcome {
            Ok(()) => {
                let _ = core.note_scan_platform(
                    &active.id,
                    ubm_core::central::ScanPlatformEvent::PlatformStopped,
                    now_ms(),
                    &mut out,
                );
                let _ = out.drain();
                // `note_scan_platform` already settled the kernel op for the
                // terminal session; the follow-up settle is a duplicate that
                // only observes, then the actual OS-stop success releases.
                let _ = core.settle_op(
                    &active.id,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                let _ = out.drain();
                report_terminal_release(&mut core, &active.id, true, None);
                recycle_observations(&mut core);
                Ok(())
            }
            Err(error) => {
                let _ = core.note_scan_platform(
                    &active.id,
                    ubm_core::central::ScanPlatformEvent::StopFailed,
                    now_ms(),
                    &mut out,
                );
                let _ = out.drain();
                let _ = core.settle_op(
                    &active.id,
                    ContenderKind::Failure,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                let _ = out.drain();
                report_terminal_release(&mut core, &active.id, false, Some(error.code()));
                recycle_observations(&mut core);
                Err(error)
            }
        }
    }

    /// Connect to a radio peer id (btleplug peripheral identity): resolve
    /// the peer, admit the connection, dispatch, then drive the radio. A
    /// radio failure marks peer loss (Connecting -> Lost, no resurrection)
    /// and settles the op as failed; the radio error takes precedence over
    /// compensation bookkeeping.
    pub async fn connect(
        &self,
        peer_id: &str,
        lease: &str,
        timeout_ms: u64,
    ) -> Result<ConnectionHandle, DesktopError> {
        self.admit("connection.connect")?;
        let peer_key = {
            let mut core = self.inner.core.lock().await;
            core.resolve_peer("platform-guid", peer_id)
                .map_err(DesktopError::from)?
        };
        // The peer is known from here regardless of the link outcome, so a
        // failed connect still leaves a peer loss the host can observe.
        self.inner
            .peers
            .lock()
            .await
            .insert(peer_id.to_owned(), peer_key.clone());
        let operation = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let id = core
                .connect(&peer_key, lease, timeout_ms, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            id
        };
        let mut drop_guard = CancelOnDrop::armed(
            self,
            operation.clone(),
            DropCleanup::Connect {
                peer_id: peer_id.to_owned(),
                peer_key: peer_key.clone(),
            },
        );
        let deadline = Duration::from_millis(timeout_ms);
        let result = match tokio::time::timeout(deadline, self.inner.boundary.connect(peer_id))
            .await
        {
            Ok(Ok(())) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                // Best-effort: the event loop may have recorded the
                // DeviceConnected event first.
                let _ = core.note_link_established(&peer_key);
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Success,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled {
                        kind: OperationTerminalKind::Succeeded,
                        ..
                    } => {
                        let connection_generation = core.connection_generation(&peer_key);
                        Ok(ConnectionHandle {
                            peer_key,
                            connection_generation,
                        })
                    }
                    CompletionOutcome::Settled { kind, .. } => {
                        Err(terminal_to_error(kind, "connection.connect"))
                    }
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Succeeded) => {
                                let connection_generation = core.connection_generation(&peer_key);
                                Ok(ConnectionHandle {
                                    peer_key,
                                    connection_generation,
                                })
                            }
                            Some(winner) => Err(terminal_to_error(winner, "connection.connect")),
                            None => Err(DesktopError::cancelled("connection.connect")),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "connection.connect",
                    )),
                }
            }
            Ok(Err(error)) => {
                // Settle under the lock, then drop the guard before awaiting
                // the compensating radio disconnect (F24): a stuck cleanup
                // must never block unrelated peers behind the core lock.
                let error_code = error.code();
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    let _ = out.drain();
                    let _ = settle_and_release(
                        &mut core,
                        &operation,
                        ContenderKind::Failure,
                        true,
                        None,
                        &mut out,
                    );
                }
                // Partial-failure cleanup: a half-opened OS link must not
                // linger without an owner. Bounded by the same 1 s
                // discipline as explicit disconnect (L5): a stuck radio
                // wait never hangs the failing connect. The core lock is not
                // held across this await.
                let cleanup = tokio::time::timeout(
                    DISCONNECT_COMPLETION_TIMEOUT,
                    self.inner.boundary.disconnect(peer_id),
                )
                .await;
                // Feed the actual compensation outcome back: a failed
                // half-open cleanup retains a failure receipt instead of a
                // clean release. The op already settled as failed above; when
                // compensation failed, retain that fact.
                if !matches!(cleanup, Ok(Ok(()))) {
                    let mut core = self.inner.core.lock().await;
                    // The op was already released as success above; retain the
                    // compensation failure as a disconnect failure so it is
                    // visible, never hidden.
                    let _ = core.report_disconnect_failure(&peer_key, error_code);
                }
                Err(error)
            }
            Err(_) => {
                // Deadline won before the radio answered: mark loss, settle
                // as timeout, and clean the half-open link outside the lock.
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                    let _ = out.drain();
                }
                let _ = tokio::time::timeout(
                    DISCONNECT_COMPLETION_TIMEOUT,
                    self.inner.boundary.disconnect(peer_id),
                )
                .await;
                Err(self.settle_timeout(&operation, "connection.connect").await)
            }
        };
        drop_guard.defuse();
        result
    }

    /// Explicit disconnect with a bounded radio wait: the link releases on
    /// OS confirmation, and a radio failure is retained as a cleanup failure
    /// (`report_disconnect_failure`) rather than reported as a good release.
    pub async fn disconnect(&self, peer_id: &str, lease: &str) -> Result<(), DesktopError> {
        self.admit("connection.disconnect")?;
        let peer_key = self
            .inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })?;
        {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            core.disconnect(&peer_key, lease, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
        }
        let outcome = tokio::time::timeout(
            DISCONNECT_COMPLETION_TIMEOUT,
            self.inner.boundary.disconnect(peer_id),
        )
        .await;
        // Late radio completions must not resurrect the link: drop local
        // subscription routing for this peer now; the core already
        // invalidated its hubs at disconnect.
        self.drop_peer_subscriptions(peer_id).await;
        let mut core = self.inner.core.lock().await;
        match outcome {
            Ok(Ok(())) => {
                core.note_link_released(&peer_key)
                    .map_err(DesktopError::from)?;
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = core.report_disconnect_failure(&peer_key, error.code());
                Err(error)
            }
            Err(_) => {
                let _ = core.report_disconnect_failure(&peer_key, BleErrorCode::OperationTimedOut);
                Err(contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    "connection.disconnect",
                )
                .with_detail("disconnect completion deadline exceeded"))
            }
        }
    }

    /// Radio-observed link loss (event loop or host): exactly one terminal,
    /// no double release (CLN-02).
    pub async fn remote_peer_loss(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peer_key = self
            .inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })?;
        self.drop_peer_subscriptions(peer_id).await;
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        core.note_peer_loss(&peer_key, now_ms(), &mut out)
            .map_err(DesktopError::from)?;
        Ok(())
    }

    /// Discover services and register service/characteristic/descriptor
    /// paths with per-UUID occurrence identity. Unusable entries are
    /// skipped with their identities (partial failure); an empty usable
    /// snapshot fails discovery instead of completing an empty database.
    pub async fn discover(
        &self,
        peer_id: &str,
        lease: &str,
    ) -> Result<DiscoveryReport, DesktopError> {
        self.admit("discovery.complete")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        {
            let mut core = self.inner.core.lock().await;
            core.begin_discovery(&peer_key)
                .map_err(DesktopError::from)?;
        }
        let services = match self.inner.boundary.discover(peer_id).await {
            Ok(services) => services,
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let _ = core.fail_discovery(&peer_key);
                return Err(error);
            }
        };
        let mut report = DiscoveryReport::default();
        {
            let mut core = self.inner.core.lock().await;
            // The core registers paths against a Current database: the
            // radio snapshot completing is what advances Discovering to
            // Current; entries then register one by one underneath it.
            core.complete_discovery(&peer_key)
                .map_err(DesktopError::from)?;
            let mut service_counts: HashMap<String, u64> = HashMap::new();
            for service in &services {
                let service_occurrence = service_counts.entry(service.uuid.clone()).or_insert(0);
                let occurrence = *service_occurrence;
                *service_occurrence += 1;
                match core.register_path(
                    &peer_key,
                    &service.uuid,
                    occurrence,
                    None,
                    None,
                    None,
                    None,
                    0,
                    lease,
                ) {
                    Ok(_) => report.paths_registered += 1,
                    Err(error) => {
                        report
                            .skipped
                            .push((service.uuid.clone(), core_code_str(&error).to_owned()));
                        continue;
                    }
                }
                let mut char_counts: HashMap<String, u64> = HashMap::new();
                for characteristic in &service.characteristics {
                    let char_occurrence =
                        char_counts.entry(characteristic.uuid.clone()).or_insert(0);
                    let char_occ = *char_occurrence;
                    *char_occurrence += 1;
                    let bits =
                        crate::btleplug_backend::core_property_bits(characteristic.properties);
                    match core.register_path(
                        &peer_key,
                        &service.uuid,
                        occurrence,
                        Some(&characteristic.uuid),
                        Some(char_occ),
                        None,
                        None,
                        bits,
                        lease,
                    ) {
                        Ok(_) => report.paths_registered += 1,
                        Err(error) => {
                            report.skipped.push((
                                characteristic.uuid.clone(),
                                core_code_str(&error).to_owned(),
                            ));
                            continue;
                        }
                    }
                    let mut desc_counts: HashMap<String, u64> = HashMap::new();
                    for descriptor in &characteristic.descriptors {
                        let desc_occurrence =
                            desc_counts.entry(descriptor.uuid.clone()).or_insert(0);
                        let desc_occ = *desc_occurrence;
                        *desc_occurrence += 1;
                        // Descriptor values travel explicit descriptor
                        // operations; the CCCD stays managed by
                        // subscribe/unsubscribe (core rejects direct CCCD
                        // writes with `gatt.cccd-managed`).
                        let descriptor_bits =
                            ubm_core::central::GATT_PROP_READ | ubm_core::central::GATT_PROP_WRITE;
                        match core.register_path(
                            &peer_key,
                            &service.uuid,
                            occurrence,
                            Some(&characteristic.uuid),
                            Some(char_occ),
                            Some(&descriptor.uuid),
                            Some(desc_occ),
                            descriptor_bits,
                            lease,
                        ) {
                            Ok(_) => report.paths_registered += 1,
                            Err(error) => report
                                .skipped
                                .push((descriptor.uuid.clone(), core_code_str(&error).to_owned())),
                        }
                    }
                }
            }
            if report.paths_registered == 0 {
                // Unwind the empty completion through valid transitions:
                // a Current database with no usable paths is not a usable
                // outcome, so mark changed and require rediscovery rather
                // than leaving a hollow Current behind.
                let first = report
                    .skipped
                    .first()
                    .map(|(uuid, code)| format!("{uuid}:{code}"))
                    .unwrap_or_else(|| "empty-snapshot".to_owned());
                let _ = core.services_changed(&peer_key);
                let _ = core.require_rediscovery(&peer_key);
                return Err(contract_error(
                    BleErrorCode::GattNotFound,
                    BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail(format!("no usable GATT paths in radio snapshot ({first})")));
            }
        }
        Ok(report)
    }

    /// Read the peer's current discovery tree in registration order (F01).
    /// Pure read like [`Self::peer_key_for`]: no admission, no radio. An
    /// unknown peer fails with `peer.not-found`; a peer off-`current`
    /// fails with `gatt.discovery-required` (or `gatt.stale-handle` after
    /// a service change) — never an empty list mistaken for an empty
    /// database.
    pub async fn discovered_paths(
        &self,
        peer_id: &str,
    ) -> Result<Vec<DiscoveredPath>, DesktopError> {
        let peer_key = self.known_peer_key(peer_id).await?;
        let core = self.inner.core.lock().await;
        let stored = core.snapshot_paths(&peer_key).map_err(DesktopError::from)?;
        Ok(stored
            .iter()
            .map(|path| DiscoveredPath {
                service_uuid: path.service_uuid().to_owned(),
                service_occurrence: path.service_occurrence(),
                characteristic_uuid: path.characteristic_uuid().map(str::to_owned),
                characteristic_occurrence: path.characteristic_occurrence(),
                descriptor_uuid: path.descriptor_uuid().map(str::to_owned),
                descriptor_occurrence: path.descriptor_occurrence(),
                properties: path.properties(),
            })
            .collect())
    }

    /// GATT read through a validated path: freshness, discovery, lease, and
    /// property checks run before kernel admission, so a stale path never
    /// dispatches to the radio.
    pub async fn read(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.admit("gatt.read")?;
        let (operation, key, peer_key) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.read",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.read",
                        )
                    })?;
            let id = core
                .start_read(index, timeout_ms, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (
                id,
                instance_key(peer_id, &stored, &characteristic),
                peer_key,
            )
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        // F03: the op timeout is an end-to-end deadline for dispatched work.
        // `expire_sweep` only covers queued ops, so dispatched reads race the
        // radio against their own deadline; the winning core outcome is the
        // only caller result.
        let deadline = Duration::from_millis(timeout_ms);
        let result = match tokio::time::timeout(
            deadline,
            self.inner
                .boundary
                .read_characteristic(peer_id, &key.1, key.2, &key.3, key.4),
        )
        .await
        {
            Ok(Ok(bytes)) => {
                let mut core = self.inner.core.lock().await;
                // F03: a link that died mid-read wins over the late radio
                // bytes. Generations alone cannot catch this (disconnect keeps
                // them), so the live link state competes explicitly.
                let link_live = matches!(
                    core.connection_state(&peer_key),
                    Some(ubm_core::central::ConnectionState::Connected)
                );
                if !link_live {
                    let mut out = batch();
                    let _ = settle_and_release(
                        &mut core,
                        &operation,
                        ContenderKind::Disconnect,
                        true,
                        None,
                        &mut out,
                    );
                    drop_guard.defuse();
                    return Err(contract_error(
                        BleErrorCode::OperationDisconnected,
                        BleErrorDomain::Connection,
                        "gatt.read",
                    ));
                }
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Success,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled {
                        kind: OperationTerminalKind::Succeeded,
                        ..
                    } => Ok(bytes),
                    CompletionOutcome::Settled { kind, cause, .. } => {
                        if cause == Some(BleErrorCode::GattStaleHandle) {
                            Err(contract_error(
                                BleErrorCode::GattStaleHandle,
                                BleErrorDomain::Gatt,
                                "gatt.read",
                            ))
                        } else {
                            Err(terminal_to_error(kind, "gatt.read"))
                        }
                    }
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Succeeded) => Ok(bytes),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.read")),
                            None => Err(DesktopError::cancelled("gatt.read")),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "gatt.read",
                    )),
                }
            }
            Ok(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Failure,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled { .. } => Err(error),
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Failed) => Err(error),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.read")),
                            None => Err(error),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(error),
                }
            }
            Err(_) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Timeout,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled {
                        kind: OperationTerminalKind::TimedOut,
                        ..
                    } => Err(contract_error(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Connection,
                        "gatt.read",
                    )),
                    CompletionOutcome::Settled { kind, .. } => {
                        Err(terminal_to_error(kind, "gatt.read"))
                    }
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(winner) => Err(terminal_to_error(winner, "gatt.read")),
                            None => Err(contract_error(
                                BleErrorCode::OperationTimedOut,
                                BleErrorDomain::Connection,
                                "gatt.read",
                            )),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(contract_error(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Connection,
                        "gatt.read",
                    )),
                }
            }
        };
        drop_guard.defuse();
        result
    }

    /// GATT write. `"long-write"` is rejected up front: prepared-write
    /// transactions have no btleplug radio path (see `PARITY_GAPS.md`),
    /// and a long value must never silently degrade to a single ATT write.
    pub async fn write(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        mode: &str,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        self.admit("gatt.write")?;
        if mode == "long-write" {
            return Err(contract_error(
                BleErrorCode::CapabilityLimited,
                BleErrorDomain::Capability,
                "gatt.write",
            )
            .with_detail("long-write needs a prepared-write radio path"));
        }
        let with_response = mode == "with-response";
        let value_len = value.len() as u64;
        // M1 + F03: the OS MTU lookup runs before the core lock (never stalls
        // the loop), but inside the op deadline (never escapes it). A stuck
        // D-Bus round trip becomes a timeout, not a hang.
        let total = Duration::from_millis(timeout_ms);
        let started = Instant::now();
        let measured_mtu = match tokio::time::timeout(total, self.inner.boundary.mtu(peer_id)).await
        {
            Ok(mtu) => mtu,
            Err(_) => {
                return Err(contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    "gatt.write",
                ));
            }
        };
        let remaining = total
            .checked_sub(started.elapsed())
            .unwrap_or(Duration::from_millis(1));
        let (operation, key) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let maximum = Self::write_maximum(&core, measured_mtu, "gatt.write")?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.write",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.write",
                        )
                    })?;
            let id = core
                .start_write(
                    index,
                    mode,
                    value_len,
                    Some(maximum),
                    true,
                    timeout_ms,
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, instance_key(peer_id, &stored, &characteristic))
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let result = match tokio::time::timeout(
            remaining,
            self.inner.boundary.write_characteristic(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                value,
                with_response,
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Success,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled {
                        kind: OperationTerminalKind::Succeeded,
                        ..
                    } => Ok(()),
                    CompletionOutcome::Settled { kind, cause, .. } => {
                        if cause == Some(BleErrorCode::GattStaleHandle) {
                            Err(contract_error(
                                BleErrorCode::GattStaleHandle,
                                BleErrorDomain::Gatt,
                                "gatt.write",
                            ))
                        } else {
                            Err(terminal_to_error(kind, "gatt.write"))
                        }
                    }
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Succeeded) => Ok(()),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.write")),
                            None => Err(DesktopError::cancelled("gatt.write")),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "gatt.write",
                    )),
                }
            }
            Ok(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Failure,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled { .. } => Err(error),
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Failed) => Err(error),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.write")),
                            None => Err(error),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(error),
                }
            }
            Err(_) => Err(self.settle_timeout(&operation, "gatt.write").await),
        };
        drop_guard.defuse();
        result
    }

    /// Descriptor read through a validated descriptor path.
    pub async fn read_descriptor(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.admit("gatt.read-descriptor")?;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "path.index",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.read-descriptor",
                        )
                    })?;
            let descriptor = stored.descriptor_uuid().map(str::to_owned).ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.read-descriptor",
                )
            })?;
            let id = core
                .start_read_descriptor(index, timeout_ms, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            let key = instance_key(peer_id, &stored, &characteristic);
            let descriptor_occurrence = stored.descriptor_occurrence().unwrap_or(0);
            (id, key, descriptor, descriptor_occurrence)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let deadline = Duration::from_millis(timeout_ms);
        let result = match tokio::time::timeout(
            deadline,
            self.inner.boundary.read_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
            ),
        )
        .await
        {
            Ok(Ok(bytes)) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Success,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled {
                        kind: OperationTerminalKind::Succeeded,
                        ..
                    } => Ok(bytes),
                    CompletionOutcome::Settled { kind, cause, .. } => {
                        if cause == Some(BleErrorCode::GattStaleHandle) {
                            Err(contract_error(
                                BleErrorCode::GattStaleHandle,
                                BleErrorDomain::Gatt,
                                "gatt.read-descriptor",
                            ))
                        } else {
                            Err(terminal_to_error(kind, "gatt.read-descriptor"))
                        }
                    }
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Succeeded) => Ok(bytes),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.read-descriptor")),
                            None => Err(DesktopError::cancelled("gatt.read-descriptor")),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "gatt.read-descriptor",
                    )),
                }
            }
            Ok(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Failure,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled { .. } => Err(error),
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Failed) => Err(error),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.read-descriptor")),
                            None => Err(error),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(error),
                }
            }
            Err(_) => Err(self
                .settle_timeout(&operation, "gatt.read-descriptor")
                .await),
        };
        drop_guard.defuse();
        result
    }

    /// Descriptor write through a validated descriptor path. Direct CCCD
    /// writes fail closed in the core with `gatt.cccd-managed`: sharing
    /// rules stay with subscribe/unsubscribe.
    pub async fn write_descriptor(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        self.admit("gatt.write-descriptor")?;
        let value_len = value.len() as u64;
        // M1 + F03: MTU inside the op deadline (see `write`).
        let total = Duration::from_millis(timeout_ms);
        let started = Instant::now();
        let measured_mtu = match tokio::time::timeout(total, self.inner.boundary.mtu(peer_id)).await
        {
            Ok(mtu) => mtu,
            Err(_) => {
                return Err(contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    "gatt.write-descriptor",
                ));
            }
        };
        let remaining = total
            .checked_sub(started.elapsed())
            .unwrap_or(Duration::from_millis(1));
        let (operation, key, descriptor, descriptor_occurrence) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "path.index",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.write-descriptor",
                        )
                    })?;
            let descriptor = stored.descriptor_uuid().map(str::to_owned).ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.write-descriptor",
                )
            })?;
            let maximum = Self::write_maximum(&core, measured_mtu, "gatt.write-descriptor")?;
            let id = core
                .start_write_descriptor(
                    index,
                    value_len,
                    Some(maximum),
                    timeout_ms,
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            let key = instance_key(peer_id, &stored, &characteristic);
            let descriptor_occurrence = stored.descriptor_occurrence().unwrap_or(0);
            (id, key, descriptor, descriptor_occurrence)
        };
        let mut drop_guard = CancelOnDrop::armed(self, operation.clone(), DropCleanup::Op);
        let result = match tokio::time::timeout(
            remaining,
            self.inner.boundary.write_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
                value,
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Success,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled {
                        kind: OperationTerminalKind::Succeeded,
                        ..
                    } => Ok(()),
                    CompletionOutcome::Settled { kind, cause, .. } => {
                        if cause == Some(BleErrorCode::GattStaleHandle) {
                            Err(contract_error(
                                BleErrorCode::GattStaleHandle,
                                BleErrorDomain::Gatt,
                                "gatt.write-descriptor",
                            ))
                        } else {
                            Err(terminal_to_error(kind, "gatt.write-descriptor"))
                        }
                    }
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Succeeded) => Ok(()),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.write-descriptor")),
                            None => Err(DesktopError::cancelled("gatt.write-descriptor")),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(contract_error(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "gatt.write-descriptor",
                    )),
                }
            }
            Ok(Err(error)) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let outcome = settle_and_release(
                    &mut core,
                    &operation,
                    ContenderKind::Failure,
                    true,
                    None,
                    &mut out,
                )?;
                match outcome {
                    CompletionOutcome::Settled { .. } => Err(error),
                    CompletionOutcome::DuplicateSuppressed { .. } => {
                        let kind = release_duplicate(&mut core, &operation, true, None);
                        match kind {
                            Some(OperationTerminalKind::Failed) => Err(error),
                            Some(winner) => Err(terminal_to_error(winner, "gatt.write-descriptor")),
                            None => Err(error),
                        }
                    }
                    CompletionOutcome::ContenderIgnored => Err(error),
                }
            }
            Err(_) => Err(self
                .settle_timeout(&operation, "gatt.write-descriptor")
                .await),
        };
        drop_guard.defuse();
        result
    }

    /// Subscribe one consumer: admit in the core, route early values through
    /// the hub (pre-ready values quarantine per GATT-04), then enable the
    /// physical CCCD. A radio failure settles the enablement as failed and
    /// removes routing — a failed subscribe never leaves a live CCCD.
    pub async fn subscribe(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        self.admit("gatt.subscribe")?;
        // Resolve the instance first (pure read, no side effects) so the
        // pending-disable check below never nests locks: no path here ever
        // holds two mutexes at once.
        let key = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let core = self.inner.core.lock().await;
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.subscribe",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.subscribe",
                        )
                    })?;
            instance_key(peer_id, &stored, &characteristic)
        };
        // L7 resubscribe semantics: a pending failed disable fails the
        // resubscribe closed — complete the disable with `unsubscribe`
        // first instead of racing it with an enable.
        if self.inner.failed_disables.lock().await.contains(&key) {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "gatt.subscribe",
            )
            .with_detail("physical disable pending; complete it with unsubscribe"));
        }
        let (operation, path_index, drive_enable) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.subscribe",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.subscribe",
                        )
                    })?;
            debug_assert_eq!(key, instance_key(peer_id, &stored, &characteristic));
            // The physical enable is driven exactly once, by the caller
            // whose subscribe staged the core's explicit enable effect
            // (F11). A ready CCCD shares without an effect, a pending
            // enablement joins it, and a repeated request reuses its op:
            // none of them may touch the radio. Inferring any of this
            // from `physical_cccd_enabled` would re-drive the enable for
            // every joiner that arrives mid-flight.
            let effects_before = core.typed_effects().len();
            let id = core
                .subscribe(
                    index,
                    "error",
                    DEFAULT_SUB_ITEM_CAP,
                    DEFAULT_SUB_BYTE_CAP,
                    consumer,
                    timeout_ms,
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            let drive_enable = core.typed_effects()[effects_before..].iter().any(|effect| {
                effect.kind() == CentralEffectKind::SubscribeEnable && effect.operation_id() == &id
            });
            // Dispatch only a freshly queued op: a joined consumer's op is
            // already complete, and a re-subscribed in-flight op is already
            // dispatched. Dispatching either again would fail closed.
            let dispatchable = matches!(
                core.operation_state(&id),
                Some(ubm_core::ownership::OpStateView::Queued)
            );
            if dispatchable {
                core.dispatch_op(&id, &mut out)
                    .map_err(DesktopError::from)?;
            }
            (id, index, drive_enable)
        };
        let mut drop_guard = CancelOnDrop::armed(
            self,
            operation.clone(),
            DropCleanup::Subscribe {
                key: key.clone(),
                path_index,
            },
        );
        // Route before the physical enable so values arriving mid-enable
        // quarantine in the hub instead of dropping on the floor. The
        // routing carries the epoch the forwarder is about to capture, so
        // a value queued under a dead generation never matches (F10).
        let epoch = self.routing_epoch(peer_id).await;
        self.inner
            .subscriptions
            .lock()
            .await
            .insert(key.clone(), (path_index, epoch));
        if !drive_enable {
            // Joiners never touch the radio (F11): an immediate-success share
            // on an enabled hub is already terminal and releases now; a
            // pending join stays live for the enabler to settle and sweep.
            let mut core = self.inner.core.lock().await;
            report_terminal_release(&mut core, &operation, true, None);
            recycle_observations(&mut core);
            drop_guard.defuse();
            return Ok(());
        }
        let deadline = Duration::from_millis(timeout_ms);
        let result = match tokio::time::timeout(
            deadline,
            self.inner
                .boundary
                .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, true, epoch),
        )
        .await
        {
            Ok(Ok(())) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let effects_before = core.typed_effects().len();
                let _ = core.settle_subscribe_enable(path_index, true, now_ms(), &mut out);
                let _ = out.drain();
                // A late success with nobody eligible stages a compensating
                // physical disable (F12): the OS enable is live with no
                // owner, so it must be torn down, not leaked.
                let compensating = core.typed_effects()[effects_before..]
                    .iter()
                    .any(|effect| effect.kind() == CentralEffectKind::SubscribeDisable);
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                let _ = out.drain();
                let own_kind = terminal_kind_of(&core, &operation);
                sweep_terminal_successes(&mut core);
                if compensating {
                    // Drop the core lock before the compensating radio
                    // disable; re-lock to settle it.
                    drop(core);
                    let _ = self
                        .inner
                        .boundary
                        .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false, epoch)
                        .await;
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                    let _ = out.drain();
                    sweep_terminal_successes(&mut core);
                    self.inner.subscriptions.lock().await.remove(&key);
                    drop_guard.defuse();
                    return Err(DesktopError::cancelled("gatt.subscribe"));
                }
                match own_kind {
                    Some(OperationTerminalKind::Succeeded) | None => Ok(()),
                    Some(winner) => Err(terminal_to_error(winner, "gatt.subscribe")),
                }
            }
            Ok(Err(error)) => {
                self.inner.subscriptions.lock().await.remove(&key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
                let _ = out.drain();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                Err(error)
            }
            Err(_) => {
                // Deadline won: settle our own op as timeout first (so it
                // stays TimedOut, not Failed), then fail the shared enable
                // for the hub and any joiners.
                self.inner.subscriptions.lock().await.remove(&key);
                {
                    let mut core = self.inner.core.lock().await;
                    let mut out = batch();
                    let _ = settle_and_release(
                        &mut core,
                        &operation,
                        ContenderKind::Timeout,
                        true,
                        None,
                        &mut out,
                    );
                }
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                Err(contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    "gatt.subscribe",
                ))
            }
        };
        drop_guard.defuse();
        result
    }

    /// Remove one consumer. Removing one consumer never disables another
    /// consumer's live CCCD: the physical disable fires only when the core
    /// reports the last removal issuing it. Returns whether the physical
    /// CCCD was disabled.
    ///
    /// Disable-failure semantics (L7): when the radio refuses the physical
    /// disable, routing stays in place so values keep flowing (no silent
    /// drops), the hub truthfully stays `Disabling`, and the key parks in
    /// the pending-disable set. A later `unsubscribe` retries the disable;
    /// a `subscribe` on the same instance fails closed until the disable
    /// completes.
    pub async fn unsubscribe(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<bool, DesktopError> {
        self.admit("gatt.unsubscribe")?;
        let (disable_physical, path_index, key) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).cloned().ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.unsubscribe",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.unsubscribe",
                        )
                    })?;
            let disable = core
                .unsubscribe(index, consumer, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            (
                disable,
                index,
                instance_key(peer_id, &stored, &characteristic),
            )
        };
        if !disable_physical {
            return self.retry_failed_disable(peer_id, &key, path_index).await;
        }
        // Teardown is keyed by instance: the boundary ignores the epoch on
        // disable, so the current generation is passed through untouched.
        let epoch = self.routing_epoch(peer_id).await;
        match self
            .inner
            .boundary
            .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false, epoch)
            .await
        {
            Ok(()) => {
                self.inner.subscriptions.lock().await.remove(&key);
                self.inner.failed_disables.lock().await.remove(&key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                Ok(true)
            }
            Err(error) => {
                // Routing stays: the CCCD is still live, so values must
                // still reach the hub. The pending-disable set routes the
                // next `unsubscribe` into a retry.
                self.inner.failed_disables.lock().await.insert(key);
                Err(error)
            }
        }
    }

    /// Retry a previously failed physical disable. Returns `Ok(true)` when
    /// the retry completes the disable, `Ok(false)` when no disable is
    /// pending, and the radio error when the retry fails again.
    async fn retry_failed_disable(
        &self,
        peer_id: &str,
        key: &InstanceKey,
        path_index: usize,
    ) -> Result<bool, DesktopError> {
        if !self.inner.failed_disables.lock().await.contains(key) {
            // No radio work: still recycle any terminal shares (e.g. an
            // immediate-success join that released elsewhere) so the ledger
            // never grows across unsubscribe-only cycles.
            let mut core = self.inner.core.lock().await;
            recycle_observations(&mut core);
            return Ok(false);
        }
        let epoch = self.routing_epoch(peer_id).await;
        match self
            .inner
            .boundary
            .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false, epoch)
            .await
        {
            Ok(()) => {
                self.inner.subscriptions.lock().await.remove(key);
                self.inner.failed_disables.lock().await.remove(key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                let _ = out.drain();
                sweep_terminal_successes(&mut core);
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    /// Take one buffered notification value for a consumer (M2, FIFO
    /// arrival order). Values buffer from subscription onward and stay
    /// observable after the radio delivers them; `None` means no value is
    /// waiting. Only the hub's admitted bytes cross here: values the
    /// radio never delivered are never synthesized.
    ///
    /// Note: `None` hides whether the stream is live-empty, terminal, or
    /// closed. Prefer [`DesktopCentral::poll_notification`] (F17), which
    /// distinguishes all three plus invalidation.
    pub async fn take_notification(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<Option<Vec<u8>>, DesktopError> {
        self.admit("gatt.take-notification")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let mut core = self.inner.core.lock().await;
        let index = core
            .resolve_path(&peer_key, selector)
            .map_err(DesktopError::from)?;
        Ok(core.take_notification_value(index, consumer))
    }

    /// Poll one consumer's stream with a typed outcome (F17): value, live
    /// empty, overflow terminal (exactly once, with loss details),
    /// invalidation (service change/disconnect), or closure. Values drain
    /// before the terminal; after the terminal is observed the stream
    /// reports closed, never live-empty again.
    pub async fn poll_notification(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<NotificationPoll, DesktopError> {
        self.admit("gatt.take-notification")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let mut core = self.inner.core.lock().await;
        let index = match core.resolve_path(&peer_key, selector) {
            Ok(index) => index,
            Err(error) => {
                // A selector that no longer resolves while the database is
                // off-current is stale invalidation (service change,
                // disconnect, rediscovery required), not a missing path.
                let current = matches!(
                    core.database_state(&peer_key),
                    Some(ubm_core::central::DatabaseState::Current)
                );
                if current {
                    return Err(DesktopError::from(error));
                }
                return Ok(NotificationPoll::Invalidated);
            }
        };
        if let Some(value) = core.take_notification_value(index, consumer) {
            return Ok(NotificationPoll::Value(value));
        }
        if let Some(terminal) = core.take_terminal(index, consumer) {
            return Ok(NotificationPoll::Terminal(terminal));
        }
        match core.consumer_state(index, consumer) {
            Some(
                ubm_core::central::ConsumerState::Ready
                | ubm_core::central::ConsumerState::Enabling
                | ubm_core::central::ConsumerState::Removing,
            ) => Ok(NotificationPoll::Empty),
            Some(ubm_core::central::ConsumerState::Invalid) => Ok(NotificationPoll::Invalidated),
            Some(
                ubm_core::central::ConsumerState::Failed
                | ubm_core::central::ConsumerState::Removed,
            )
            | None => Ok(NotificationPoll::Closed),
        }
    }

    /// Cancel one admitted operation (`operation.aborted` discipline in the
    /// core). Mid-flight radio abort is an OS gap — btleplug exposes no
    /// abort — so cancellation settles core-side while an in-flight radio
    /// call runs to its own (bounded) completion; see `PARITY_GAPS.md`. A
    /// queued cancellation releases immediately (no radio work exists); a
    /// dispatched cancellation leaves the release to the in-flight driver,
    /// which observes the abort as the winning outcome and reports it.
    pub async fn cancel_operation(
        &self,
        operation: &OperationId,
    ) -> Result<ubm_core::central::CompletionOutcome, DesktopError> {
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let outcome = core
            .cancel_op(operation, now_ms(), &mut out)
            .map_err(DesktopError::from)?;
        let _ = out.drain();
        recycle_observations(&mut core);
        if let CompletionOutcome::Settled { commit, .. } = &outcome {
            // `NotDispatched` proves no radio work exists: release now.
            // `Released` (dispatched abort) stays for the driver, which will
            // see `DuplicateSuppressed` and report after observing the win.
            if *commit == ubm_core::contracts::CommitState::NotDispatched {
                report_terminal_release(&mut core, operation, true, None);
            }
        }
        Ok(outcome)
    }

    /// Per-central shutdown (F14/F15): close this attachment's admission
    /// first so no new work races cleanup, stop the owned scan, release
    /// owned radio subscriptions through the boundary teardown hook, release
    /// owned OS links with per-link receipts, join the event loop so nothing
    /// races teardown, then drive incremental destruction to acknowledged
    /// completion and return the authoritative report. Idempotent. Other
    /// centrals keep working and new centrals can open; process-executor
    /// shutdown is a separate explicit process-owner step
    /// ([`crate::executor::shutdown_desktop_runtime`]), never implied here.
    pub async fn shutdown(&self) -> ShutdownReport {
        // F14: admission closes before any cleanup starts, so a racing
        // starter cannot slip work in behind the scan stop.
        self.inner.shut_down.store(true, Ordering::SeqCst);
        let _ = self.stop_scan().await;
        // M3: abort forwarders and best-effort release OS-side CCCDs so no
        // live subscription outlives the central. Per-scope release failures
        // are drained as receipts (F14), never swallowed.
        self.inner.boundary.close().await;
        let radio_close_failures = self.inner.boundary.take_close_failures();
        // F14: release owned OS links with per-link receipts before the
        // owner is destroyed.
        self.release_owned_links().await;
        // F03: cancel remaining in-flight ops so late radio work cannot
        // resurrect after teardown — queued releases now, dispatched
        // observes the abort as the winning outcome when its radio finishes.
        let live: Vec<OperationId> = {
            let core = self.inner.core.lock().await;
            core.live_operation_ids()
        };
        for id in live {
            let _ = self.cancel_operation(&id).await;
        }
        let worker = self.inner.loop_done.lock().await.take();
        let _ = self.inner.loop_stop.send(true);
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        // F15: the final record is taken only after every destroy pass
        // executed, every dispatched remainder was answered, and every
        // terminal release was acknowledged — never from the legacy
        // unacknowledged `destroy()`.
        let (record, destroy_steps) = self.drive_destroy().await;
        ShutdownReport {
            record,
            radio_close_failures,
            destroy_steps,
        }
    }

    /// Release every owned OS link (F14): each peer whose core connection is
    /// still live gets one bounded radio disconnect. Success confirms link
    /// release (`Disconnected`); a radio failure or deadline marks the link
    /// `Disconnecting` and records a disconnect failure, so the final
    /// destroy record names it (receipt) instead of claiming a clean
    /// release. Skips peers that already released, so repeat shutdowns stay
    /// quiet and idempotent.
    async fn release_owned_links(&self) {
        let peers: Vec<(String, String)> = {
            let peers = self.inner.peers.lock().await;
            peers
                .iter()
                .map(|(peer_id, peer_key)| (peer_id.clone(), peer_key.clone()))
                .collect()
        };
        for (peer_id, peer_key) in peers {
            let live = {
                let core = self.inner.core.lock().await;
                matches!(
                    core.connection_state(&peer_key),
                    Some(
                        ubm_core::central::ConnectionState::Connected
                            | ubm_core::central::ConnectionState::Connecting
                            | ubm_core::central::ConnectionState::Disconnecting
                    )
                )
            };
            if !live {
                continue;
            }
            let outcome = tokio::time::timeout(
                DISCONNECT_COMPLETION_TIMEOUT,
                self.inner.boundary.disconnect(&peer_id),
            )
            .await;
            // Late radio completions must not resurrect the link: drop local
            // subscription routing for this peer now.
            self.drop_peer_subscriptions(&peer_id).await;
            let mut core = self.inner.core.lock().await;
            match outcome {
                Ok(Ok(())) => {
                    let _ = core.shutdown_release_link(&peer_key);
                }
                Ok(Err(error)) => {
                    core.note_shutdown_disconnect_failed(&peer_key, error.code());
                }
                Err(_) => {
                    core.note_shutdown_disconnect_failed(
                        &peer_key,
                        BleErrorCode::OperationTimedOut,
                    );
                }
            }
        }
    }

    /// Drive incremental destruction to acknowledged completion (F15): one
    /// `destroy_step` pass per loop iteration, executing each pass's effects,
    /// settling dispatched remainders as destroyed, and acknowledging every
    /// terminal release — the host protocol `destroy_step` requires. The
    /// final record comes from `destroy_record`, which refuses while work
    /// pends and merges disconnect and release failures. Bounded: the pass
    /// budget is one per live op plus a final pass, so a wedged kernel
    /// surfaces `central.destroy.truncated` instead of looping forever.
    async fn drive_destroy(&self) -> (Result<CleanupRecord, DesktopError>, usize) {
        let mut steps = 0usize;
        let mut budget = {
            let core = self.inner.core.lock().await;
            core.live_operation_ids().len().saturating_add(2)
        };
        loop {
            let progress = {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let progress = match core.destroy_step(&mut out) {
                    Ok(progress) => progress,
                    Err(error) => return (Err(DesktopError::from(error)), steps),
                };
                // Execute the pass outside radio work: shutdown emits logical
                // StatePublish/CleanupRelease effects (OS releases already ran
                // above), so draining executes the batch.
                let _ = out.drain();
                // Settle dispatched remainders as destroyed before acking.
                for id in core.live_operation_ids() {
                    if core.operation_state(&id)
                        == Some(ubm_core::ownership::OpStateView::Dispatched)
                    {
                        let mut settle_out = batch();
                        let _ = core.settle_op(
                            &id,
                            ContenderKind::Destroy,
                            true,
                            0,
                            now_ms(),
                            &mut settle_out,
                        );
                        let _ = settle_out.drain();
                    }
                }
                // Ack every terminal: the host did release its tracking for
                // each (OS CCCDs via `close`, OS links via
                // `release_owned_links`), so success is the truthful report.
                // Shutdown acks keep tombstones: racing callers that settle
                // after the reap still observe their winning terminal.
                for id in core.terminal_operation_ids() {
                    let _ = core.report_shutdown_release(&id);
                }
                recycle_observations(&mut core);
                progress
            };
            steps += 1;
            if progress.done {
                break;
            }
            if budget == 0 {
                return (
                    Err(contract_error(
                        BleErrorCode::StreamQuota,
                        BleErrorDomain::Stream,
                        "central.destroy.truncated",
                    )),
                    steps,
                );
            }
            budget -= 1;
        }
        let record = {
            let mut core = self.inner.core.lock().await;
            core.destroy_record().map_err(DesktopError::from)
        };
        (record, steps)
    }

    /// Compose the effective single-write maximum for one peer: the ATT
    /// protocol ceiling plus the OS-measured MTU as both the negotiated
    /// and the backend limit (btleplug submits one ATT operation per
    /// write; the OS enforces the negotiated MTU). An unmeasured MTU fails
    /// closed with `capability.unavailable`, never a guessed 23.
    /// Pure computation over a pre-fetched MTU: callers fetch
    /// `boundary.mtu()` before locking the core (M1), so this never
    /// awaits under the lock.
    fn write_maximum(
        core: &Central,
        measured_mtu: Option<u16>,
        operation: &'static str,
    ) -> Result<u64, DesktopError> {
        let directional = measured_mtu
            .map(|mtu| u64::from(mtu).saturating_sub(3))
            .filter(|limit| *limit > 0);
        core.maximum_write_length(Some(ATT_MAX_WRITE), directional, directional, operation)
            .map_err(DesktopError::from)
    }

    async fn known_peer_key(&self, peer_id: &str) -> Result<String, DesktopError> {
        self.inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })
    }

    async fn drop_peer_subscriptions(&self, peer_id: &str) {
        clear_peer_routing(&self.inner, peer_id).await;
    }
}

/// Drop subscription routing and pending-disable retries for one peer,
/// and bump the peer's subscription epoch (F10). Late radio completions
/// must not resurrect the link: the core already invalidated its hubs,
/// the adapter drops its routing alongside, and values still queued under
/// the dead generation fail the routing check after resubscribe.
async fn clear_peer_routing<B>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let mut subscriptions = inner.subscriptions.lock().await;
    subscriptions.retain(|key, _| key.0 != peer_id);
    drop(subscriptions);
    let mut failed = inner.failed_disables.lock().await;
    failed.retain(|key| key.0 != peer_id);
    drop(failed);
    let mut epochs = inner.epochs.lock().await;
    let epoch = epochs.entry(peer_id.to_owned()).or_insert(0);
    *epoch = epoch.saturating_add(1);
}

/// Drive one central lifetime: resolve advertisements to platform-guid
/// peers, reconcile connection events with the core, and route
/// notifications to subscribed hubs. Every core settlement here is
/// best-effort — the explicit op paths own authoritative transitions, and a
/// racing explicit op must not fail because the loop saw the event first.
/// The loop owns core settlement only on the event-source-closed path;
/// [`DesktopCentral::stop_scan`] owns the stop path and
/// [`DesktopCentral::shutdown`] joins this worker, so cleanup cannot race.
async fn scan_loop<B: RadioBoundary>(inner: Arc<Inner<B>>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                let _ = changed;
                break;
            }
            event = inner.boundary.next_event() => {
                match event {
                    None => {
                        // Event source closed: settle an owned scan session
                        // as source-closed so the owner is released even
                        // when the OS never confirms a stop.
                        let id = inner.scan.lock().await.as_ref().map(|active| active.id.clone());
                        if let Some(id) = id {
                            let mut core = inner.core.lock().await;
                            let mut out = batch();
                            let _ = core.note_scan_platform(
                                &id,
                                ubm_core::central::ScanPlatformEvent::SourceClosed,
                                now_ms(),
                                &mut out,
                            );
                            let _ = out.drain();
                            let _ = core.settle_op(
                                &id,
                                ContenderKind::Success,
                                true,
                                0,
                                now_ms(),
                                &mut out,
                            );
                            let _ = out.drain();
                            report_terminal_release(&mut core, &id, true, None);
                            recycle_observations(&mut core);
                        }
                        break;
                    }
                    Some(RadioEvent::Advertisement(snapshot)) => {
                        ingest_advertisement(&inner, &snapshot).await;
                    }
                    Some(RadioEvent::Connected(peer_id)) => {
                        reconcile_connected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::Disconnected(peer_id)) => {
                        reconcile_disconnected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::ServicesChanged(peer_id)) => {
                        services_changed_invalidated(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::Notification {
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        epoch,
                        value,
                    }) => {
                        deliver(
                            &inner,
                            &peer_id,
                            &service_uuid,
                            service_occurrence,
                            &characteristic_uuid,
                            characteristic_occurrence,
                            epoch,
                            value,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

async fn ingest_advertisement<B: RadioBoundary>(inner: &Arc<Inner<B>>, snapshot: &PeerSnapshot) {
    // L8: resolve under the core lock, then drop the guard before the map
    // insert — the central lock is never held across the peers await, and
    // the insert stays out of the core critical section.
    let peer_key = {
        let mut core = inner.core.lock().await;
        // Platform-guid is the desktop peer identity: btleplug exposes the
        // address type opaquely per platform, so address targeting stays a
        // narrow-OS-adapter gap rather than a guessed domain.
        core.resolve_peer("platform-guid", &snapshot.id).ok()
    };
    if let Some(peer_key) = peer_key {
        inner
            .peers
            .lock()
            .await
            .insert(snapshot.id.clone(), peer_key);
    }
    // F22: every advertisement stays pollable with its full facts until the
    // host takes it. Bounded FIFO: past the cap the oldest evicts and the
    // eviction counts, so a slow host sees loss explicitly.
    let mut queue = inner.advertisements.lock().await;
    if queue.len() >= ADVERTISEMENT_CAP {
        queue.pop_front();
        inner.advertisement_drops.fetch_add(1, Ordering::Relaxed);
    }
    queue.push_back(snapshot.clone());
}

async fn reconcile_connected<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    if let Some(peer_key) = peer_key {
        let mut core = inner.core.lock().await;
        let _ = core.note_link_established(&peer_key);
    }
}

async fn reconcile_disconnected<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    if let Some(peer_key) = peer_key {
        clear_peer_routing(inner, peer_id).await;
        let mut core = inner.core.lock().await;
        let mut out = batch();
        let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
    }
}

/// Route one radio notification into its per-instance hub with the full
/// value bytes (M2). Events without routing (unknown or unsubscribed
/// instance) drop on the floor: the hub never receives unattributable
/// bytes. Events whose install-time epoch no longer matches the live
/// routing are stale queue drainage from before a disconnect or service
/// change, and drop the same way even when the instance key matches again
/// (F10): the epoch is captured at forwarder install, never minted here.
#[allow(clippy::too_many_arguments)]
async fn deliver<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
    epoch: u64,
    value: Vec<u8>,
) {
    let key = (
        peer_id.to_owned(),
        service_uuid.to_owned(),
        service_occurrence,
        characteristic_uuid.to_owned(),
        characteristic_occurrence,
    );
    let routed = inner.subscriptions.lock().await.get(&key).copied();
    let Some((path_index, routing_epoch)) = routed else {
        return;
    };
    if epoch != routing_epoch {
        return;
    }
    let mut core = inner.core.lock().await;
    let _ = core.deliver_notification_value(path_index, &value);
}

/// Invalidate generations when the OS reports a changed GATT database
/// (L6): stale paths must fail closed and require rediscovery instead of
/// serving re-reads through dead handles. Routing drops alongside the
/// core hubs so late values cannot reach invalidated consumers.
async fn services_changed_invalidated<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    clear_peer_routing(inner, peer_id).await;
    let mut core = inner.core.lock().await;
    let _ = core.services_changed(&peer_key);
    let _ = core.require_rediscovery(&peer_key);
}

/// Adapter behavior over the mocked boundary: scan ownership and cleanup,
/// peer/connection/GATT mapping with contract identities, partial
/// discovery failures, descriptor paths, subscription sharing, and
/// cancellation. No radio is touched; the fake boundary is the only
/// evidence source on this host.
#[cfg(test)]
mod adapter_tests {
    use std::time::Duration;

    use ubm_core::central::{ConnectionState, ConsumerState, ScanSessionState};

    use crate::boundary::{
        CharacteristicSnapshot, DescriptorSnapshot, FakeRadio, FaultOp, PeerSnapshot,
        PropertyFlags, RadioEvent, ServiceSnapshot,
    };

    use super::DesktopCentral;

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
    const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";
    const BODY_SENSOR_LOCATION: &str = "00002a38-0000-1000-8000-00805f9b34fb";
    const HEART_RATE_CONTROL_POINT: &str = "00002a39-0000-1000-8000-00805f9b34fb";

    fn notify_props() -> PropertyFlags {
        PropertyFlags {
            read: true,
            write: false,
            write_without_response: false,
            notify: true,
            indicate: false,
        }
    }

    fn rw_props() -> PropertyFlags {
        PropertyFlags {
            read: true,
            write: true,
            write_without_response: true,
            notify: false,
            indicate: false,
        }
    }

    fn hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: notify_props(),
                descriptors: vec![DescriptorSnapshot {
                    uuid: USER_DESCRIPTION.to_owned(),
                    occurrence: 0,
                }],
            }],
        }
    }

    /// One service carrying two same-UUID notify characteristics (wrist +
    /// chest strap): occurrence is the only instance identity.
    fn duplicate_hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: notify_props(),
                    descriptors: Vec::new(),
                },
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 1,
                    properties: notify_props(),
                    descriptors: Vec::new(),
                },
            ],
        }
    }

    fn battery_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: BATTERY_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: BATTERY_LEVEL.to_owned(),
                occurrence: 0,
                properties: rw_props(),
                descriptors: Vec::new(),
            }],
        }
    }

    fn advertisement(peer_id: &str) -> RadioEvent {
        RadioEvent::Advertisement(PeerSnapshot {
            id: peer_id.to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
        })
    }

    async fn open() -> DesktopCentral<FakeRadio> {
        DesktopCentral::open(FakeRadio::new(), "test-host")
            .await
            .expect("open central")
    }

    async fn wait_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
        for _ in 0..200 {
            if central.peer_key_for(peer_id).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for peer {peer_id}");
    }

    fn hrm_selector(service_occurrence: u64) -> crate::central::PathSelector {
        hrm_instance_selector(service_occurrence, 0)
    }

    fn hrm_instance_selector(
        service_occurrence: u64,
        characteristic_occurrence: u64,
    ) -> crate::central::PathSelector {
        DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(service_occurrence),
            Some(HRM_MEASUREMENT),
            Some(characteristic_occurrence),
            None,
            None,
        )
        .expect("selector")
    }

    fn notification(peer_id: &str, characteristic_occurrence: u64, value: Vec<u8>) -> RadioEvent {
        notification_epoch(peer_id, characteristic_occurrence, value, 0)
    }

    fn notification_epoch(
        peer_id: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        epoch: u64,
    ) -> RadioEvent {
        RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HRM_MEASUREMENT.to_owned(),
            characteristic_occurrence,
            epoch,
            value,
        }
    }

    #[tokio::test]
    async fn scan_ingests_advertisements_and_cleans_up() {
        let central = open().await;
        central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("start scan");
        assert!(central.has_active_scan().await);
        central.boundary().push_event(advertisement("peer-1"));
        wait_peer(&central, "peer-1").await;
        central.stop_scan().await.expect("stop scan");
        assert!(!central.has_active_scan().await);
        assert!(
            !central.boundary().scan_active(),
            "OS scan stopped on cleanup"
        );
        let calls = central.boundary().calls();
        let start = calls
            .iter()
            .position(|call| call == "start_scan")
            .expect("start recorded");
        let stop = calls
            .iter()
            .position(|call| call == "stop_scan")
            .expect("stop recorded");
        assert!(start < stop, "stop follows start");
        assert_eq!(
            calls.iter().filter(|call| *call == "stop_scan").count(),
            1,
            "cleanup stops the OS scan exactly once"
        );
    }

    #[tokio::test]
    async fn failed_scan_start_releases_the_owner() {
        let central = open().await;
        central
            .boundary()
            .fail_next(FaultOp::StartScan, "os denied");
        let error = central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect_err("scripted start failure");
        assert_eq!(error.code_str(), "scan.start-failed");
        assert!(!central.has_active_scan().await);
        assert!(
            !central.boundary().calls().contains(&"stop_scan".to_owned()),
            "no stop without a start"
        );
        // The owner is released: a second start succeeds.
        central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("retry after failed start");
        central.stop_scan().await.expect("stop");
    }

    #[tokio::test]
    async fn second_scan_owner_is_rejected_without_radio_effect() {
        let central = open().await;
        central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("first");
        let error = central
            .start_scan("owner-b", &[], 5000)
            .await
            .expect_err("second owner rejected");
        assert_eq!(error.code_str(), "scan.already-active");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "start_scan")
                .count(),
            1,
            "rejected arbitration never reaches the radio"
        );
        central.stop_scan().await.expect("stop");
    }

    #[tokio::test]
    async fn event_source_close_settles_the_owned_scan() {
        let central = open().await;
        let session = central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("start");
        central.boundary().close_events();
        for _ in 0..200 {
            let state = central
                .with_core(|core| core.scan_session_state(session.operation_id()))
                .await;
            if state == Some(ScanSessionState::Stopped) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            central
                .with_core(|core| core.scan_session_state(session.operation_id()))
                .await,
            Some(ScanSessionState::Stopped),
            "source close releases the owner"
        );
        // Late stop stays a safe no-op cleanup, not a second settlement.
        central.stop_scan().await.expect("late stop");
    }

    #[tokio::test]
    async fn connect_does_not_share_without_rule() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-1"));
        let handle = central
            .connect("peer-1", "lease-a", 5000)
            .await
            .expect("connect");
        assert!(!handle.peer_key.is_empty());
        assert!(handle.connection_generation.is_some());
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(state, Some(ConnectionState::Connected));
        // No sharing support: a second lease is rejected before any radio call.
        let before = central.boundary().calls().len();
        let error = central
            .connect("peer-1", "lease-b", 5000)
            .await
            .expect_err("second lease rejected");
        assert_eq!(error.code_str(), "connection.already-owned");
        assert_eq!(
            central.boundary().calls().len(),
            before,
            "no radio on rejection"
        );
        central
            .disconnect("peer-1", "lease-a")
            .await
            .expect("disconnect");
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(state, Some(ConnectionState::Disconnected));
    }

    #[tokio::test]
    async fn connect_failure_marks_loss_and_cleans_half_open_link() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-9"));
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let error = central
            .connect("peer-9", "lease-a", 5000)
            .await
            .expect_err("scripted connect failure");
        assert_eq!(error.code_str(), "connection.failed");
        let peer_key = central.peer_key_for("peer-9").await.expect("peer known");
        let state = central
            .with_core(|core| core.connection_state(&peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Lost),
            "Connecting -> Lost, no resurrection"
        );
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "half-open OS link cleaned up"
        );
    }

    #[tokio::test]
    async fn disconnect_radio_failure_stays_disconnecting() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-2"));
        let handle = central
            .connect("peer-2", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "os stuck");
        let error = central
            .disconnect("peer-2", "lease-a")
            .await
            .expect_err("scripted disconnect failure");
        assert_eq!(error.operation(), "connection.disconnect");
        // Not reported clean: the link never reaches Disconnected.
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Disconnecting),
            "failed cleanup retains ownership"
        );
    }

    #[tokio::test]
    async fn discovery_registers_duplicate_uuids_by_occurrence() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-3"));
        central
            .connect("peer-3", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-3", vec![hrm_service(), hrm_service()]);
        let report = central
            .discover("peer-3", "lease-a")
            .await
            .expect("discover");
        // Two services x (service + characteristic + descriptor).
        assert_eq!(report.paths_registered, 6);
        assert!(report.skipped.is_empty());
        // Same UUID twice: occurrence selects each instance.
        let peer_key = central.peer_key_for("peer-3").await.expect("peer");
        for occurrence in [0u64, 1u64] {
            let selector = hrm_selector(occurrence);
            central
                .with_core(|core| core.resolve_path(&peer_key, &selector))
                .await
                .expect("occurrence resolves");
        }
        // Descriptor path reads through the validated descriptor level.
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let value = central
            .read_descriptor("peer-3", &descriptor_selector, 5000)
            .await
            .expect("descriptor read");
        assert_eq!(value, vec![0x01]);
        // Characteristic read returns the fake payload.
        let value = central
            .read("peer-3", &hrm_selector(1), 5000)
            .await
            .expect("read");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn read_after_peer_loss_fails_without_radio() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-4"));
        central
            .connect("peer-4", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-4", vec![battery_service()]);
        central
            .discover("peer-4", "lease-a")
            .await
            .expect("discover");
        central.remote_peer_loss("peer-4").await.expect("loss");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let error = central
            .read("peer-4", &selector, 5000)
            .await
            .expect_err("stale path never dispatches");
        // The link gate fails the read closed (`lifecycle.invalid-state`:
        // the Invalid database never reaches property validation). What the
        // adapter pins is the fail-closed discipline, not the core's code
        // choice: no radio dispatch, attributed error.
        assert_eq!(error.code_str(), "lifecycle.invalid-state");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"read_characteristic".to_owned()),
            "no radio call for a stale path"
        );
    }

    #[tokio::test]
    async fn write_without_measured_mtu_fails_closed() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-8"));
        central
            .connect("peer-8", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-8", vec![battery_service()]);
        central
            .discover("peer-8", "lease-a")
            .await
            .expect("discover");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        // No MTU scripted: the maximum is unmeasured, never guessed.
        let error = central
            .write("peer-8", &selector, vec![1], "with-response", 5000)
            .await
            .expect_err("unmeasured maximum fails closed");
        assert_eq!(error.code_str(), "capability.unavailable");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "no radio call without a measured maximum"
        );
    }

    #[tokio::test]
    async fn long_write_is_rejected_up_front() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-5"));
        central
            .connect("peer-5", "lease-a", 5000)
            .await
            .expect("connect");
        central.boundary().set_mtu("peer-5", 23);
        central
            .boundary()
            .set_services("peer-5", vec![battery_service()]);
        central
            .discover("peer-5", "lease-a")
            .await
            .expect("discover");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let error = central
            .write("peer-5", &selector, vec![1, 2, 3], "long-write", 5000)
            .await
            .expect_err("long-write has no radio path");
        assert_eq!(error.code_str(), "capability.limited");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "never silently single-written"
        );
        // Ordinary write modes still flow.
        central
            .write("peer-5", &selector, vec![1, 2, 3], "with-response", 5000)
            .await
            .expect("plain write");
    }

    #[tokio::test]
    async fn subscription_sharing_keeps_one_cccd() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-6"));
        central
            .connect("peer-6", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-6", vec![hrm_service()]);
        central
            .discover("peer-6", "lease-a")
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .subscribe("peer-6", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe a");
        central
            .subscribe("peer-6", &selector, "consumer-b", 5000)
            .await
            .expect("subscribe b");
        let peer_key = central.peer_key_for("peer-6").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert!(
            central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "physical CCCD enabled"
        );
        assert_eq!(
            central
                .with_core(|core| core.consumer_state(path_index, "consumer-b"))
                .await,
            Some(ConsumerState::Ready),
            "second consumer shares the physical enablement"
        );
        // First removal keeps the other consumer's live CCCD.
        let disabled = central
            .unsubscribe("peer-6", &selector, "consumer-a")
            .await
            .expect("unsubscribe a");
        assert!(!disabled, "CCCD stays for consumer-b");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            1,
            "no physical toggle while a consumer remains"
        );
        // Last removal disables the physical CCCD.
        let disabled = central
            .unsubscribe("peer-6", &selector, "consumer-b")
            .await
            .expect("unsubscribe b");
        assert!(disabled, "last removal issues the physical disable");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, disable once"
        );
    }

    #[tokio::test]
    async fn notifications_reach_the_hub_stream() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-10"));
        central
            .connect("peer-10", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-10", vec![hrm_service()]);
        central
            .discover("peer-10", "lease-a")
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .subscribe("peer-10", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        let peer_key = central.peer_key_for("peer-10").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        // Overflow past the item bound under the error policy surfaces
        // exactly one terminal: values flow radio -> hub -> stream.
        for _ in 0..70 {
            central
                .boundary()
                .push_event(notification("peer-10", 0, vec![0x06, 0x40]));
        }
        for _ in 0..400 {
            let terminal = central
                .with_core_mut(|core| core.take_terminal(path_index, "consumer-a"))
                .await;
            if terminal.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            central
                .with_core_mut(|core| core.take_terminal(path_index, "consumer-a"))
                .await
                .is_none(),
            "terminal surfaces exactly once"
        );
        // M2: admitted values buffered before the overflow stay observable
        // through the take API — payloads are delivered, not dropped.
        let first = central
            .take_notification("peer-10", &selector, "consumer-a")
            .await
            .expect("take");
        assert_eq!(first, Some(vec![0x06, 0x40]), "buffered value observable");
        // M5 orphan-disable: removing the terminal (post-overflow Failed)
        // consumer releases the live CCCD instead of leaking it.
        let disabled = central
            .unsubscribe("peer-10", &selector, "consumer-a")
            .await
            .expect("unsubscribe");
        assert!(disabled, "terminal removal issues the orphan disable");
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "orphan CCCD released"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, orphan-disable once"
        );
    }

    #[tokio::test]
    async fn failed_subscribe_leaves_no_live_cccd() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-7"));
        central
            .connect("peer-7", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-7", vec![hrm_service()]);
        central
            .discover("peer-7", "lease-a")
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .boundary()
            .fail_next(FaultOp::Subscribe, "cccd refused");
        let error = central
            .subscribe("peer-7", &selector, "consumer-a", 5000)
            .await
            .expect_err("scripted subscribe failure");
        assert_eq!(error.code_str(), "gatt.subscribe-failed");
        let peer_key = central.peer_key_for("peer-7").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "no live CCCD after failed subscribe"
        );
    }

    #[tokio::test]
    async fn cancel_unknown_operation_fails_closed() {
        use ubm_core::contracts::OperationId;
        let central = open().await;
        let unknown = OperationId::new("central-op-9999").expect("id");
        let error = central
            .cancel_operation(&unknown)
            .await
            .expect_err("unknown op cannot cancel");
        assert!(!error.operation().is_empty(), "attributed error");
    }

    /// Connect, discover, and leave `peer_id` ready for GATT ops.
    async fn ready_peer(
        central: &DesktopCentral<FakeRadio>,
        peer_id: &str,
        services: Vec<ServiceSnapshot>,
    ) {
        central.boundary().push_event(advertisement(peer_id));
        central
            .connect(peer_id, "lease-a", 5000)
            .await
            .expect("connect");
        central.boundary().set_services(peer_id, services);
        central
            .discover(peer_id, "lease-a")
            .await
            .expect("discover");
    }

    #[tokio::test]
    async fn h1_duplicate_uuid_instances_route_per_instance() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-h1", vec![duplicate_hrm_service()]).await;
        // Per-instance payloads: wrist on occurrence 0, chest on 1.
        central.boundary().set_characteristic_value(
            "peer-h1",
            HRM_SERVICE,
            0,
            HRM_MEASUREMENT,
            0,
            vec![0x77],
        );
        central.boundary().set_characteristic_value(
            "peer-h1",
            HRM_SERVICE,
            0,
            HRM_MEASUREMENT,
            1,
            vec![0xc4, 0x35],
        );
        let wrist = central
            .read("peer-h1", &hrm_instance_selector(0, 0), 5000)
            .await
            .expect("read wrist");
        assert_eq!(wrist, vec![0x77], "occurrence 0 reads instance 0");
        let chest = central
            .read("peer-h1", &hrm_instance_selector(0, 1), 5000)
            .await
            .expect("read chest");
        assert_eq!(
            chest,
            vec![0xc4, 0x35],
            "occurrence 1 reads instance 1, not instance 0"
        );
        // Two same-UUID subscriptions keep distinct routing: each
        // instance's values reach only its own consumer.
        central
            .subscribe("peer-h1", &hrm_instance_selector(0, 0), "wrist-app", 5000)
            .await
            .expect("subscribe wrist");
        central
            .subscribe("peer-h1", &hrm_instance_selector(0, 1), "chest-app", 5000)
            .await
            .expect("subscribe chest");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "two instances enable twice, never one shared forwarder"
        );
        central
            .boundary()
            .push_event(notification("peer-h1", 0, vec![0x01]));
        central
            .boundary()
            .push_event(notification("peer-h1", 1, vec![0x02]));
        let mut wrist_value = None;
        let mut chest_value = None;
        for _ in 0..200 {
            if wrist_value.is_none() {
                wrist_value = central
                    .take_notification("peer-h1", &hrm_instance_selector(0, 0), "wrist-app")
                    .await
                    .expect("take wrist");
            }
            if chest_value.is_none() {
                chest_value = central
                    .take_notification("peer-h1", &hrm_instance_selector(0, 1), "chest-app")
                    .await
                    .expect("take chest");
            }
            if wrist_value.is_some() && chest_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(wrist_value, Some(vec![0x01]), "wrist values reach wrist");
        assert_eq!(chest_value, Some(vec![0x02]), "chest values reach chest");
        let peer_key = central.peer_key_for("peer-h1").await.expect("peer");
        for (occurrence, consumer) in [(0u64, "wrist-app"), (1u64, "chest-app")] {
            let path = central
                .with_core(|core| {
                    core.resolve_path(&peer_key, &hrm_instance_selector(0, occurrence))
                        .expect("path")
                })
                .await;
            assert_eq!(
                central
                    .with_core(|core| core.consumer_state(path, consumer))
                    .await,
                Some(ConsumerState::Ready),
                "both consumers live on their own hub"
            );
        }
    }

    #[tokio::test]
    async fn m1_write_does_not_hold_core_lock_across_mtu() {
        let central = open().await;
        ready_peer(&central, "peer-m1", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-m1", 23);
        central.boundary().block_op(FaultOp::Mtu);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let writer = central.clone();
        let pending_write = tokio::spawn(async move {
            writer
                .write("peer-m1", &selector, vec![1], "with-response", 5000)
                .await
        });
        // Let the write reach the gated MTU lookup, then prove the core
        // lock is free: link loss still completes while the write pends.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !pending_write.is_finished(),
            "write pends on the stuck MTU lookup"
        );
        tokio::time::timeout(Duration::from_secs(5), central.remote_peer_loss("peer-m1"))
            .await
            .expect("link loss never queues behind the write")
            .expect("loss recorded");
        central.boundary().unblock_op(FaultOp::Mtu);
        let outcome = tokio::time::timeout(Duration::from_secs(5), pending_write)
            .await
            .expect("write completes after unblock");
        let error = outcome
            .expect("write task")
            .expect_err("stale path fails closed");
        assert_eq!(error.code_str(), "lifecycle.invalid-state");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "no radio call for the invalidated path"
        );
    }

    #[tokio::test]
    async fn m2_values_observable_after_subscribe() {
        let central = open().await;
        ready_peer(&central, "peer-m2", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-m2", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        // Nothing before the radio delivers: no synthesized values.
        assert_eq!(
            central
                .take_notification("peer-m2", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "no values before delivery"
        );
        central
            .boundary()
            .push_event(notification("peer-m2", 0, vec![0xde, 0xad]));
        central
            .boundary()
            .push_event(notification("peer-m2", 0, vec![0xbe, 0xef]));
        let mut first = None;
        let mut second = None;
        for _ in 0..200 {
            if first.is_none() {
                first = central
                    .take_notification("peer-m2", &selector, "consumer-a")
                    .await
                    .expect("take");
            } else if second.is_none() {
                second = central
                    .take_notification("peer-m2", &selector, "consumer-a")
                    .await
                    .expect("take");
            }
            if first.is_some() && second.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(first, Some(vec![0xde, 0xad]), "first value FIFO");
        assert_eq!(second, Some(vec![0xbe, 0xef]), "second value FIFO");
        assert_eq!(
            central
                .take_notification("peer-m2", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "drained exactly, nothing invented"
        );
    }

    #[tokio::test]
    async fn m4_open_projects_desktop_capabilities() {
        use ubm_core::central::CapabilityAdmission;
        use ubm_core::contracts::BleErrorCode;

        let central = open().await;
        // A provided row gates open with its limitation...
        assert!(
            matches!(
                central
                    .with_core(
                        |core| core.check_capability("peer:resolve-reference", "desktop.probe")
                    )
                    .await,
                Ok(CapabilityAdmission::ProceedWithLimitation)
            ),
            "resolve-reference projects as provided-with-limitation"
        );
        // ...while open adapter work stays closed.
        let error = central
            .with_core(|core| core.check_capability("peer:address-targeting", "desktop.probe"))
            .await
            .expect_err("adapter work gates closed");
        assert_eq!(error.code(), BleErrorCode::CapabilityUnsupported);
    }

    #[tokio::test]
    async fn l5_connect_failure_cleanup_is_bounded() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-l5"));
        central.boundary().block_op(FaultOp::Disconnect);
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            central.connect("peer-l5", "lease-a", 5000),
        )
        .await
        .expect("failing connect never hangs on cleanup")
        .expect_err("scripted connect failure");
        assert_eq!(error.code_str(), "connection.failed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cleanup bounded by the 1 s discipline"
        );
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "half-open link cleanup attempted despite the stuck radio"
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
    }

    #[tokio::test]
    async fn l6_services_changed_invalidates_paths() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-l6", vec![battery_service()]).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        // A read works before the change...
        central
            .read("peer-l6", &selector, 5000)
            .await
            .expect("read before change");
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-l6".to_owned()));
        let peer_key = central.peer_key_for("peer-l6").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            let state = central
                .with_core(|core| core.database_state(&peer_key))
                .await;
            if state == Some(DatabaseState::Undiscovered) {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "database requires rediscovery after change");
        // ...and fails closed after it, with no radio dispatch through
        // the stale handle.
        let reads_before = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "read_characteristic")
            .count();
        central
            .read("peer-l6", &selector, 5000)
            .await
            .expect_err("stale handle never dispatches");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "read_characteristic")
                .count(),
            reads_before,
            "no radio call for the invalidated path"
        );
    }

    #[tokio::test]
    async fn l6_disconnect_processed_under_notification_flood() {
        use ubm_core::central::ConnectionState;

        let central = open().await;
        ready_peer(&central, "peer-flood", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-flood", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        for _ in 0..100 {
            central
                .boundary()
                .push_event(notification("peer-flood", 0, vec![0x01]));
        }
        central
            .boundary()
            .push_event(RadioEvent::Disconnected("peer-flood".to_owned()));
        for _ in 0..100 {
            central
                .boundary()
                .push_event(notification("peer-flood", 0, vec![0x02]));
        }
        // The disconnect reconciles even though notifications surround it.
        let peer_key = central.peer_key_for("peer-flood").await.expect("peer");
        let mut lost = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.connection_state(&peer_key))
                .await
                == Some(ConnectionState::Lost)
            {
                lost = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(lost, "disconnect reconciled under flood");
        // Every flood value was accounted (bounded terminal or buffer),
        // never wedged behind the disconnect.
        let mut terminal_seen = false;
        for _ in 0..200 {
            let terminal = central
                .with_core_mut(|core| {
                    let path = core.resolve_path(&peer_key, &hrm_selector(0)).ok()?;
                    core.take_terminal(path, "consumer-a")
                })
                .await;
            if terminal.is_some() {
                terminal_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(terminal_seen, "flood accounted with a bounded terminal");
    }

    #[tokio::test]
    async fn l7_cancel_live_scan_operation() {
        use ubm_core::central::CompletionOutcome;
        use ubm_core::contracts::OperationTerminalKind;

        let central = open().await;
        let session = central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("start scan");
        let outcome = central
            .cancel_operation(session.operation_id())
            .await
            .expect("cancel live scan");
        match outcome {
            CompletionOutcome::Settled { kind, .. } => {
                assert_eq!(kind, OperationTerminalKind::Aborted, "live cancel aborts");
            }
            CompletionOutcome::DuplicateSuppressed { .. } | CompletionOutcome::ContenderIgnored => {
                panic!("live cancel must settle the operation, got {outcome:?}");
            }
        }
        // Cleanup after cancel stays safe: the late stop settles nothing
        // twice.
        central.stop_scan().await.expect("late stop");
    }

    #[tokio::test]
    async fn l7_pre_ready_values_quarantine_before_enable() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-q", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Subscribe);
        let subscriber = central.clone();
        let pending_subscribe = tokio::spawn(async move {
            subscriber
                .subscribe("peer-q", &selector, "consumer-a", 5000)
                .await
        });
        // Wait until the enable reaches the (gated) radio...
        let mut reached_radio = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"set_notifications".to_owned())
            {
                reached_radio = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(reached_radio, "enable attempted before quarantine check");
        // ...then prove values arriving mid-enable quarantine instead of
        // delivering or dropping.
        central
            .boundary()
            .push_event(notification("peer-q", 0, vec![0x09]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let peer_key = central.peer_key_for("peer-q").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert_eq!(
            central
                .with_core(|core| core.quarantined_count(path_index, "consumer-a"))
                .await,
            Some(1),
            "pre-ready value quarantined (GATT-04 ordering)"
        );
        assert_eq!(
            central
                .take_notification("peer-q", &hrm_selector(0), "consumer-a")
                .await
                .expect("take"),
            None,
            "quarantined values never deliver early"
        );
        central.boundary().unblock_op(FaultOp::Subscribe);
        pending_subscribe
            .await
            .expect("subscribe task")
            .expect("subscribe completes after unblock");
        assert_eq!(
            central
                .with_core(|core| core.consumer_state(path_index, "consumer-a"))
                .await,
            Some(ConsumerState::Ready),
            "consumer ready after enable settles"
        );
    }

    #[tokio::test]
    async fn l7_unsubscribe_disable_failure_retries() {
        let central = open().await;
        ready_peer(&central, "peer-ud", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-ud", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "cccd stuck");
        let error = central
            .unsubscribe("peer-ud", &selector, "consumer-a")
            .await
            .expect_err("scripted disable failure");
        assert_eq!(error.code_str(), "gatt.subscribe-failed");
        // The CCCD is still live, so routing stays: values keep flowing
        // instead of dropping silently.
        let peer_key = central.peer_key_for("peer-ud").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        central
            .boundary()
            .push_event(notification("peer-ud", 0, vec![0x05]));
        let mut still_flows = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.pending_value_count(path_index, "consumer-a"))
                .await
                == Some(1)
            {
                still_flows = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            still_flows,
            "values still flow while the disable is pending"
        );
        // Resubscribing on the same instance fails closed until the
        // pending disable completes...
        let resubscribe = central
            .subscribe("peer-ud", &selector, "consumer-a", 5000)
            .await
            .expect_err("resubscribe races pending disable");
        assert_eq!(resubscribe.code_str(), "lifecycle.invalid-state");
        // ...and a later unsubscribe retries the disable to completion.
        let disabled = central
            .unsubscribe("peer-ud", &selector, "consumer-a")
            .await
            .expect("retry");
        assert!(disabled, "retry completes the pending disable");
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "CCCD released after retry"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            3,
            "enable, failed disable, retry disable"
        );
    }

    /// One service with four characteristics covering every combination of
    /// the two write capabilities (F08). Occurrences are 0 throughout:
    /// distinct UUIDs keep the addressing out of the picture.
    fn write_matrix_service() -> ServiceSnapshot {
        let characteristic =
            |uuid: &str, write: bool, without_response: bool| CharacteristicSnapshot {
                uuid: uuid.to_owned(),
                occurrence: 0,
                properties: PropertyFlags {
                    read: true,
                    write,
                    write_without_response: without_response,
                    notify: false,
                    indicate: false,
                },
                descriptors: Vec::new(),
            };
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                characteristic(HRM_MEASUREMENT, false, false),
                characteristic(BATTERY_LEVEL, true, false),
                characteristic(BODY_SENSOR_LOCATION, false, true),
                characteristic(HEART_RATE_CONTROL_POINT, true, true),
            ],
        }
    }

    fn write_matrix_selector(characteristic: &str) -> crate::central::PathSelector {
        DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(characteristic),
            Some(0),
            None,
            None,
        )
        .expect("selector")
    }

    #[tokio::test]
    async fn f08_write_property_bits_gate_by_mode() {
        use ubm_core::central::{GATT_PROP_READ, GATT_PROP_WRITE, GATT_PROP_WRITE_NO_RESPONSE};

        let central = open().await;
        ready_peer(&central, "peer-f08", vec![write_matrix_service()]).await;
        central.boundary().set_mtu("peer-f08", 23);
        let peer_key = central.peer_key_for("peer-f08").await.expect("peer");

        // The mapped bitmask itself: each write capability lands on its own
        // core bit, independently.
        for (characteristic, expected) in [
            (HRM_MEASUREMENT, GATT_PROP_READ),
            (BATTERY_LEVEL, GATT_PROP_READ | GATT_PROP_WRITE),
            (
                BODY_SENSOR_LOCATION,
                GATT_PROP_READ | GATT_PROP_WRITE_NO_RESPONSE,
            ),
            (
                HEART_RATE_CONTROL_POINT,
                GATT_PROP_READ | GATT_PROP_WRITE | GATT_PROP_WRITE_NO_RESPONSE,
            ),
        ] {
            let selector = write_matrix_selector(characteristic);
            let index = central
                .with_core(|core| core.resolve_path(&peer_key, &selector).expect("path"))
                .await;
            assert_eq!(
                central
                    .with_core(|core| core.stored_path(index).map(|path| path.properties()))
                    .await,
                Some(expected),
                "mapped bitmask for {characteristic}"
            );
        }

        // A command-only characteristic accepts without-response and rejects
        // with-response before dispatch; a request-only characteristic does
        // the reverse.
        central
            .write(
                "peer-f08",
                &write_matrix_selector(BODY_SENSOR_LOCATION),
                vec![1],
                "without-response",
                5000,
            )
            .await
            .expect("command write on command-only");
        assert_eq!(
            central.boundary().writes().as_slice(),
            &[(
                (
                    "peer-f08".to_owned(),
                    HRM_SERVICE.to_owned(),
                    0,
                    BODY_SENSOR_LOCATION.to_owned(),
                    0,
                ),
                false,
            )],
            "command write reaches the boundary without response"
        );
        let error = central
            .write(
                "peer-f08",
                &write_matrix_selector(BODY_SENSOR_LOCATION),
                vec![1],
                "with-response",
                5000,
            )
            .await
            .expect_err("request write on command-only fails closed");
        assert_eq!(error.code_str(), "gatt.property-not-supported");
        assert_eq!(
            central.boundary().writes().len(),
            1,
            "rejected write never dispatches to the radio"
        );

        central
            .write(
                "peer-f08",
                &write_matrix_selector(BATTERY_LEVEL),
                vec![1],
                "with-response",
                5000,
            )
            .await
            .expect("request write on request-only");
        assert_eq!(
            central.boundary().writes().len(),
            2,
            "admitted write dispatches exactly once"
        );
        assert!(
            central.boundary().writes()[1].1,
            "request write reaches the boundary with response"
        );
        let error = central
            .write(
                "peer-f08",
                &write_matrix_selector(BATTERY_LEVEL),
                vec![1],
                "without-response",
                5000,
            )
            .await
            .expect_err("command write on request-only fails closed");
        assert_eq!(error.code_str(), "gatt.property-not-supported");
        assert_eq!(
            central.boundary().writes().len(),
            2,
            "rejected write never dispatches to the radio"
        );

        // Both capabilities: both modes dispatch with their own mode.
        for (mode, with_response) in [("with-response", true), ("without-response", false)] {
            central
                .write(
                    "peer-f08",
                    &write_matrix_selector(HEART_RATE_CONTROL_POINT),
                    vec![1],
                    mode,
                    5000,
                )
                .await
                .expect("write on dual-capability");
            assert_eq!(
                central.boundary().writes().last().map(|write| write.1),
                Some(with_response),
                "boundary mode follows the requested mode ({mode})"
            );
        }
        // Neither capability: both modes fail closed before dispatch.
        let writes_before = central.boundary().writes().len();
        for mode in ["with-response", "without-response"] {
            let error = central
                .write(
                    "peer-f08",
                    &write_matrix_selector(HRM_MEASUREMENT),
                    vec![1],
                    mode,
                    5000,
                )
                .await
                .expect_err("write on read-only fails closed");
            assert_eq!(error.code_str(), "gatt.property-not-supported");
        }
        assert_eq!(
            central.boundary().writes().len(),
            writes_before,
            "no dispatch for the read-only characteristic"
        );
    }

    /// Duplicated service UUID where the target characteristic lives only
    /// under the second instance (F18): occurrence-0 addressing can never
    /// reach it, while an omitted service occurrence still resolves
    /// unambiguously.
    fn second_instance_service_pair() -> Vec<ServiceSnapshot> {
        vec![
            ServiceSnapshot {
                uuid: HRM_SERVICE.to_owned(),
                occurrence: 0,
                characteristics: vec![CharacteristicSnapshot {
                    uuid: BATTERY_LEVEL.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: true,
                        write: false,
                        write_without_response: false,
                        notify: false,
                        indicate: false,
                    },
                    descriptors: Vec::new(),
                }],
            },
            ServiceSnapshot {
                uuid: HRM_SERVICE.to_owned(),
                occurrence: 1,
                characteristics: vec![CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: true,
                        write: true,
                        write_without_response: false,
                        notify: true,
                        indicate: false,
                    },
                    descriptors: vec![DescriptorSnapshot {
                        uuid: USER_DESCRIPTION.to_owned(),
                        occurrence: 0,
                    }],
                }],
            },
        ]
    }

    fn second_instance_notification(
        peer_id: &str,
        service_occurrence: u64,
        value: Vec<u8>,
    ) -> RadioEvent {
        RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence,
            characteristic_uuid: HRM_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
            epoch: 0,
            value,
        }
    }

    #[tokio::test]
    async fn f18_omitted_occurrence_addresses_resolved_instance() {
        let central = open().await;
        ready_peer(&central, "peer-f18", second_instance_service_pair()).await;
        central.boundary().set_mtu("peer-f18", 23);
        // Instance 1 answers distinctly; instance 0 has no such
        // characteristic, so the canned default would expose misaddressing.
        central.boundary().set_characteristic_value(
            "peer-f18",
            HRM_SERVICE,
            1,
            HRM_MEASUREMENT,
            0,
            vec![0xc4, 0x35],
        );
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            None,
            Some(HRM_MEASUREMENT),
            None,
            None,
            None,
        )
        .expect("selector");
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            None,
            Some(HRM_MEASUREMENT),
            None,
            Some(USER_DESCRIPTION),
            None,
        )
        .expect("descriptor selector");

        // Read resolves the unique path and addresses instance 1.
        let value = central
            .read("peer-f18", &selector, 5000)
            .await
            .expect("read");
        assert_eq!(
            value,
            vec![0xc4, 0x35],
            "read addresses service occurrence 1, never the guessed 0"
        );

        // Write addresses instance 1 with the requested mode.
        central
            .write("peer-f18", &selector, vec![1], "with-response", 5000)
            .await
            .expect("write");
        assert_eq!(
            central.boundary().writes().as_slice(),
            &[(
                (
                    "peer-f18".to_owned(),
                    HRM_SERVICE.to_owned(),
                    1,
                    HRM_MEASUREMENT.to_owned(),
                    0,
                ),
                true,
            )],
            "write addresses service occurrence 1"
        );

        // Descriptor operations address instance 1 as well.
        central
            .read_descriptor("peer-f18", &descriptor_selector, 5000)
            .await
            .expect("descriptor read");
        assert_eq!(
            central.boundary().descriptor_reads().as_slice(),
            &[(
                (
                    "peer-f18".to_owned(),
                    HRM_SERVICE.to_owned(),
                    1,
                    HRM_MEASUREMENT.to_owned(),
                    0,
                ),
                USER_DESCRIPTION.to_owned(),
                0,
            )],
            "descriptor read addresses service occurrence 1"
        );
        central
            .write_descriptor("peer-f18", &descriptor_selector, vec![1], 5000)
            .await
            .expect("descriptor write");
        assert_eq!(
            central.boundary().descriptor_writes().len(),
            1,
            "descriptor write dispatches exactly once"
        );
        assert_eq!(
            central.boundary().descriptor_writes()[0].0.2,
            1,
            "descriptor write addresses service occurrence 1"
        );

        // Subscription routing registers under instance 1: its values
        // deliver, while instance-0 values for the same UUIDs do not.
        central
            .subscribe("peer-f18", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        central
            .boundary()
            .push_event(second_instance_notification("peer-f18", 1, vec![0x09]));
        let mut delivered = None;
        for _ in 0..200 {
            delivered = central
                .take_notification("peer-f18", &selector, "consumer-a")
                .await
                .expect("take");
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            delivered,
            Some(vec![0x09]),
            "instance-1 values route to the subscriber"
        );
        central
            .boundary()
            .push_event(second_instance_notification("peer-f18", 0, vec![0x08]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central
                .take_notification("peer-f18", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "instance-0 values never reach the instance-1 routing"
        );
    }

    #[tokio::test]
    async fn f11_concurrent_subscribe_enables_once() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-f11", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        // Pause the first native enable behind the radio gate.
        central.boundary().block_op(FaultOp::Subscribe);
        let first = central.clone();
        let selector_clone = selector.clone();
        let pending_first = tokio::spawn(async move {
            first
                .subscribe("peer-f11", &selector_clone, "consumer-a", 5000)
                .await
        });
        let mut reached_radio = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"set_notifications".to_owned())
            {
                reached_radio = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(reached_radio, "first enable attempted before joining");
        assert!(
            !pending_first.is_finished(),
            "first subscribe pends on the stuck enable"
        );

        // A second consumer — and a repeat of the first — join the pending
        // enablement instead of driving their own native enable.
        let second = central.clone();
        let selector_clone = selector.clone();
        let pending_second = tokio::spawn(async move {
            second
                .subscribe("peer-f11", &selector_clone, "consumer-b", 5000)
                .await
        });
        let repeat = central.clone();
        let selector_clone = selector.clone();
        let pending_repeat = tokio::spawn(async move {
            repeat
                .subscribe("peer-f11", &selector_clone, "consumer-a", 5000)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            pending_second.is_finished(),
            "joining consumer never waits on the radio"
        );
        assert!(
            pending_repeat.is_finished(),
            "repeated request never re-drives the radio"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            1,
            "exactly one native enable for all concurrent subscribers"
        );

        // One release completes every waiter with consistent readiness.
        central.boundary().unblock_op(FaultOp::Subscribe);
        pending_first
            .await
            .expect("first task")
            .expect("first subscribe");
        pending_second
            .await
            .expect("second task")
            .expect("second subscribe");
        pending_repeat
            .await
            .expect("repeat task")
            .expect("repeat subscribe");
        let peer_key = central.peer_key_for("peer-f11").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        for consumer in ["consumer-a", "consumer-b"] {
            assert_eq!(
                central
                    .with_core(|core| core.consumer_state(path_index, consumer))
                    .await,
                Some(ConsumerState::Ready),
                "{consumer} shares the one enablement"
            );
        }
        // Both consumers observe the same live stream.
        central
            .boundary()
            .push_event(notification("peer-f11", 0, vec![0x07]));
        let mut first_value = None;
        let mut second_value = None;
        for _ in 0..200 {
            if first_value.is_none() {
                first_value = central
                    .take_notification("peer-f11", &selector, "consumer-a")
                    .await
                    .expect("take a");
            }
            if second_value.is_none() {
                second_value = central
                    .take_notification("peer-f11", &selector, "consumer-b")
                    .await
                    .expect("take b");
            }
            if first_value.is_some() && second_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(first_value, Some(vec![0x07]));
        assert_eq!(second_value, Some(vec![0x07]));
        // Teardown stays single too: the shared CCCD disables once.
        central
            .unsubscribe("peer-f11", &selector, "consumer-a")
            .await
            .expect("unsubscribe a");
        central
            .unsubscribe("peer-f11", &selector, "consumer-b")
            .await
            .expect("unsubscribe b");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, disable once"
        );
    }

    #[tokio::test]
    async fn f10_stale_notification_rejected_after_reconnect() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-f10", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-f10", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        // The forwarder install captured this epoch; the held event was
        // queued under it in native ingress and is released only later.
        let installed = central.boundary().enable_epochs();
        assert_eq!(installed.len(), 1, "one forwarder installed");
        let stale_epoch = installed[0].1;
        let stale = notification_epoch("peer-f10", 0, vec![0xAA], stale_epoch);

        // Disconnect, reconnect, rediscover, resubscribe: same peripheral
        // id, same instance key, new subscription under a new generation.
        central
            .disconnect("peer-f10", "lease-a")
            .await
            .expect("disconnect");
        central
            .connect("peer-f10", "lease-a", 5000)
            .await
            .expect("reconnect");
        central
            .discover("peer-f10", "lease-a")
            .await
            .expect("rediscover");
        central
            .subscribe("peer-f10", &selector, "consumer-b", 5000)
            .await
            .expect("resubscribe");
        let reinstalled = central.boundary().enable_epochs();
        assert_eq!(reinstalled.len(), 2, "resubscribe reinstalls the forwarder");
        let fresh_epoch = reinstalled[1].1;

        // Release the held old value: it must never enter the new stream,
        // even though the peripheral id and instance key match again.
        central.boundary().push_event(stale);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central
                .take_notification("peer-f10", &selector, "consumer-b")
                .await
                .expect("take"),
            None,
            "stale queued value never reaches the new consumer"
        );
        assert_ne!(
            fresh_epoch, stale_epoch,
            "reinstall captures a new generation"
        );
        // The new subscription is live: current-generation values deliver.
        central
            .boundary()
            .push_event(notification_epoch("peer-f10", 0, vec![0xBB], fresh_epoch));
        let mut delivered = None;
        for _ in 0..200 {
            delivered = central
                .take_notification("peer-f10", &selector, "consumer-b")
                .await
                .expect("take");
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(delivered, Some(vec![0xBB]), "fresh values still deliver");

        // Same protection across a service change: a value queued under
        // the old database never enters the post-rediscovery subscription.
        let pre_change = notification_epoch("peer-f10", 0, vec![0xCC], fresh_epoch);
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-f10".to_owned()));
        let peer_key = central.peer_key_for("peer-f10").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.database_state(&peer_key))
                .await
                == Some(DatabaseState::Undiscovered)
            {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "database requires rediscovery after change");
        central
            .discover("peer-f10", "lease-a")
            .await
            .expect("rediscover after change");
        central
            .subscribe("peer-f10", &selector, "consumer-c", 5000)
            .await
            .expect("subscribe after change");
        let current_epoch = central
            .boundary()
            .enable_epochs()
            .last()
            .map(|installed| installed.1)
            .expect("forwarder installed");
        assert_ne!(
            current_epoch, fresh_epoch,
            "post-change install captures a new generation"
        );
        central.boundary().push_event(pre_change);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central
                .take_notification("peer-f10", &selector, "consumer-c")
                .await
                .expect("take"),
            None,
            "pre-change value never reaches the new subscription"
        );
        central
            .boundary()
            .push_event(notification_epoch("peer-f10", 0, vec![0xDD], current_epoch));
        let mut redelivered = None;
        for _ in 0..200 {
            redelivered = central
                .take_notification("peer-f10", &selector, "consumer-c")
                .await
                .expect("take");
            if redelivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            redelivered,
            Some(vec![0xDD]),
            "post-change subscription stays live"
        );
    }

    #[tokio::test]
    async fn f25_failed_reads_settle_and_release() {
        let central = open().await;
        ready_peer(&central, "peer-f25", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        for i in 0..9u32 {
            central.boundary().fail_next(FaultOp::Read, "att error");
            let error = central
                .read("peer-f25", &selector, 5000)
                .await
                .expect_err("native failure surfaces");
            assert_eq!(
                error.code_str(),
                "gatt.read-failed",
                "iteration {i}: caller sees the radio failure, not a quota"
            );
            let live = central.with_core(|core| core.live_operation_count()).await;
            assert_eq!(
                live, baseline,
                "iteration {i}: failed op terminal and reaped, no live leak"
            );
        }
        let value = central
            .read("peer-f25", &selector, 5000)
            .await
            .expect("admission remains after failures");
        assert_eq!(value, vec![0x42]);
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "baseline restored"
        );
    }

    #[tokio::test]
    async fn f25_failed_writes_and_descriptors_settle_and_release() {
        let central = open().await;
        ready_peer(&central, "peer-f25w", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-f25w", 64);
        let write_selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        for i in 0..9u32 {
            central.boundary().fail_next(FaultOp::Write, "att error");
            let error = central
                .write(
                    "peer-f25w",
                    &write_selector,
                    vec![0x01],
                    "with-response",
                    5000,
                )
                .await
                .expect_err("native write failure surfaces");
            assert_eq!(
                error.code_str(),
                "gatt.write-failed",
                "write iteration {i}: radio failure, not quota"
            );
            assert_eq!(
                central.with_core(|core| core.live_operation_count()).await,
                baseline,
                "write iteration {i}: reaped"
            );
        }
        ready_peer(&central, "peer-f25d", vec![hrm_service()]).await;
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        for i in 0..9u32 {
            central.boundary().fail_next(FaultOp::Read, "att error");
            let error = central
                .read_descriptor("peer-f25d", &descriptor_selector, 5000)
                .await
                .expect_err("native descriptor failure surfaces");
            assert_eq!(
                error.code_str(),
                "gatt.read-failed",
                "descriptor iteration {i}: radio failure, not quota"
            );
            assert_eq!(
                central.with_core(|core| core.live_operation_count()).await,
                baseline,
                "descriptor iteration {i}: reaped"
            );
        }
        central
            .write(
                "peer-f25w",
                &write_selector,
                vec![0x01],
                "with-response",
                5000,
            )
            .await
            .expect("write admission remains");
    }

    #[tokio::test]
    async fn f03_read_timeout_is_end_to_end_deadline() {
        let central = open().await;
        ready_peer(&central, "peer-f03t", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        // The op deadline (100 ms) must settle the caller even though the
        // radio never resolves. The outer 5 s harness timeout only fails the
        // test fast on unfixed code; it must never fire on fixed code.
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            central.read("peer-f03t", &selector, 100),
        )
        .await
        .expect("deadline driver settles before the harness");
        let error = outcome.expect_err("timeout surfaces as an error");
        assert_eq!(error.code_str(), "operation.timed-out");
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "timed-out op terminal and reaped"
        );
        central.boundary().unblock_op(FaultOp::Read);
        // Late radio work cannot resurrect: the next read succeeds cleanly.
        let value = central
            .read("peer-f03t", &selector, 5000)
            .await
            .expect("admission remains after timeout");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn f03_cancel_then_native_success_returns_cancelled() {
        let central = open().await;
        ready_peer(&central, "peer-f03c", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending =
            tokio::spawn(async move { reader.read("peer-f03c", &selector_clone, 5000).await });
        let mut op_id = None;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_ids()).await;
            if !live.is_empty() {
                op_id = Some(live[0].clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let op_id = op_id.expect("in-flight read op");
        central
            .cancel_operation(&op_id)
            .await
            .expect("cancel in-flight read");
        central.boundary().unblock_op(FaultOp::Read);
        let outcome = pending.await.expect("read task");
        let error = outcome.expect_err("cancel wins over late radio success");
        assert_eq!(error.code_str(), "operation.aborted");
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "cancelled op reaped"
        );
    }

    #[tokio::test]
    async fn f03_service_change_during_read_returns_stale() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-f03s", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending =
            tokio::spawn(async move { reader.read("peer-f03s", &selector_clone, 5000).await });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-f03s".to_owned()));
        let peer_key = central.peer_key_for("peer-f03s").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.database_state(&peer_key))
                .await
                == Some(DatabaseState::Undiscovered)
            {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "service change invalidates");
        central.boundary().unblock_op(FaultOp::Read);
        let outcome = pending.await.expect("read task");
        let error = outcome.expect_err("stale wins over late radio success");
        assert_eq!(error.code_str(), "gatt.stale-handle");
    }

    #[tokio::test]
    async fn f03_mtu_lookup_inside_deadline() {
        let central = open().await;
        ready_peer(&central, "peer-f03m", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-f03m", 64);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Mtu);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            central.write("peer-f03m", &selector, vec![0x01], "with-response", 100),
        )
        .await
        .expect("mtu deadline settles");
        let error = outcome.expect_err("stuck mtu times out");
        assert_eq!(error.code_str(), "operation.timed-out");
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "no op leaked by mtu timeout"
        );
        central.boundary().unblock_op(FaultOp::Mtu);
        central
            .write("peer-f03m", &selector, vec![0x01], "with-response", 5000)
            .await
            .expect("write succeeds after mtu unblocks");
    }

    #[tokio::test]
    async fn f03_disconnect_during_read_returns_disconnected() {
        let central = open().await;
        ready_peer(&central, "peer-f03d", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending =
            tokio::spawn(async move { reader.read("peer-f03d", &selector_clone, 5000).await });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        central
            .remote_peer_loss("peer-f03d")
            .await
            .expect("peer loss reconciles");
        central.boundary().unblock_op(FaultOp::Read);
        let outcome = pending.await.expect("read task");
        let error = outcome.expect_err("disconnect wins over late radio success");
        assert!(
            error.code_str() == "operation.disconnected" || error.code_str() == "gatt.stale-handle",
            "disconnect or stale, got {}",
            error.code_str()
        );
    }

    #[tokio::test]
    async fn f17_overflow_terminal_distinguishable() {
        // Overflow an error-policy subscription (64 items), drain all values
        // through the typed poll, and require exactly one terminal with
        // accurate loss details. Repeated polls must not invent a second
        // terminal or imply recovery.
        let central = open().await;
        ready_peer(&central, "peer-f17", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-f17", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        for _ in 0..70u32 {
            central
                .boundary()
                .push_event(notification("peer-f17", 0, vec![0x01]));
        }
        // Wait for the overflow terminal to land.
        let peer_key = central.peer_key_for("peer-f17").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        let mut failed = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.consumer_state(path_index, "consumer-a"))
                .await
                == Some(ubm_core::central::ConsumerState::Failed)
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(failed, "overflow terminates the error-policy stream");
        // Drain through the typed protocol: values, then one terminal, then
        // closed — never a second terminal, never live-empty again.
        let mut values = 0usize;
        let mut terminals = 0usize;
        let mut terminal_details = None;
        for _ in 0..100u32 {
            match central
                .poll_notification("peer-f17", &selector, "consumer-a")
                .await
                .expect("poll")
            {
                crate::central::NotificationPoll::Value(_) => values += 1,
                crate::central::NotificationPoll::Terminal(terminal) => {
                    terminals += 1;
                    terminal_details = Some((
                        terminal.dropped_items(),
                        terminal.dropped_bytes(),
                        terminal.replaced_items(),
                    ));
                }
                crate::central::NotificationPoll::Closed => break,
                crate::central::NotificationPoll::Empty => {
                    panic!("failed stream must not report live-empty")
                }
                crate::central::NotificationPoll::Invalidated => {
                    panic!("overflow is terminal, not invalidated")
                }
            }
        }
        assert_eq!(values, 64, "all admitted values drain before the terminal");
        assert_eq!(terminals, 1, "exactly one terminal");
        assert_eq!(
            terminal_details,
            Some((1, 1, 0)),
            "accurate loss details for the rejected item"
        );
        // Second terminal poll stays closed, never a new terminal.
        assert!(
            matches!(
                central
                    .poll_notification("peer-f17", &selector, "consumer-a")
                    .await
                    .expect("poll"),
                crate::central::NotificationPoll::Closed
            ),
            "terminal observed exactly once"
        );
        // Resubscription after terminal consumption works: prune the observed
        // record, then subscribe fresh and deliver.
        central
            .unsubscribe("peer-f17", &selector, "consumer-a")
            .await
            .expect("unsubscribe prunes observed terminal");
        central
            .subscribe("peer-f17", &selector, "consumer-a", 5000)
            .await
            .expect("resubscribe after terminal");
        central
            .boundary()
            .push_event(notification("peer-f17", 0, vec![0x09]));
        let mut redelivered = None;
        for _ in 0..200 {
            match central
                .poll_notification("peer-f17", &selector, "consumer-a")
                .await
                .expect("poll")
            {
                crate::central::NotificationPoll::Value(value) => {
                    redelivered = Some(value);
                    break;
                }
                crate::central::NotificationPoll::Empty => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                other => panic!("fresh subscription must deliver values, got {other:?}"),
            }
        }
        assert_eq!(redelivered, Some(vec![0x09]));
    }

    #[tokio::test]
    async fn f17_invalidation_and_closure_distinguishable() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-f17i", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-f17i", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        assert!(matches!(
            central
                .poll_notification("peer-f17i", &selector, "consumer-a")
                .await
                .expect("poll"),
            crate::central::NotificationPoll::Empty
        ));
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-f17i".to_owned()));
        let peer_key = central.peer_key_for("peer-f17i").await.expect("peer");
        for _ in 0..200 {
            if central
                .with_core(|core| core.database_state(&peer_key))
                .await
                == Some(DatabaseState::Undiscovered)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(matches!(
            central
                .poll_notification("peer-f17i", &selector, "consumer-a")
                .await
                .expect("poll"),
            crate::central::NotificationPoll::Invalidated
        ));
        central
            .discover("peer-f17i", "lease-a")
            .await
            .expect("rediscover");
        central
            .subscribe("peer-f17i", &selector, "consumer-b", 5000)
            .await
            .expect("resubscribe after invalidation");
        let fresh_epoch = central
            .boundary()
            .enable_epochs()
            .last()
            .map(|installed| installed.1)
            .expect("forwarder installed");
        central
            .boundary()
            .push_event(notification_epoch("peer-f17i", 0, vec![0x0A], fresh_epoch));
        let mut got = None;
        for _ in 0..200 {
            match central
                .poll_notification("peer-f17i", &selector, "consumer-b")
                .await
                .expect("poll")
            {
                crate::central::NotificationPoll::Value(value) => {
                    got = Some(value);
                    break;
                }
                crate::central::NotificationPoll::Empty => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                other => panic!("fresh subscription must deliver, got {other:?}"),
            }
        }
        assert_eq!(got, Some(vec![0x0A]));
    }

    #[tokio::test]
    async fn f07_flood_bounds_ingress_and_preserves_control() {
        use ubm_core::central::{ConnectionState, ConsumerState};

        let central = open().await;
        ready_peer(&central, "peer-f07a", vec![hrm_service()]).await;
        ready_peer(&central, "peer-f07b", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-f07a", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe A");
        central
            .subscribe("peer-f07b", &selector, "consumer-b", 5000)
            .await
            .expect("subscribe B");
        // Flood A with 1000 notifications in a tight loop (no awaits, so the
        // event loop cannot drain mid-flood): the 256-item bounded ingress
        // must drop the excess explicitly instead of growing memory.
        for _ in 0..1000u32 {
            central
                .boundary()
                .push_event(notification("peer-f07a", 0, vec![0x01]));
        }
        assert!(
            central.boundary().dropped_notification_count() > 0,
            "overload drops counted, memory bounded instead of grown"
        );
        assert!(
            central.boundary().data_queued_bytes() <= 262_144,
            "queued bytes stay within the ingress bound"
        );
        // Control under flood: B's disconnect (separate control queue, push
        // order preserved) still reconciles.
        central
            .boundary()
            .push_event(RadioEvent::Disconnected("peer-f07b".to_owned()));
        let peer_b = central.peer_key_for("peer-f07b").await.expect("peer B");
        let mut lost = false;
        for _ in 0..400 {
            if central
                .with_core(|core| core.connection_state(&peer_b))
                .await
                == Some(ConnectionState::Lost)
            {
                lost = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(lost, "control delivers under data flood");
        // Other devices progress: B reconnects and reads while A's flood
        // drains.
        central
            .connect("peer-f07b", "lease-a", 5000)
            .await
            .expect("B reconnects under flood");
        central
            .discover("peer-f07b", "lease-a")
            .await
            .expect("B rediscovers");
        let value = central
            .read("peer-f07b", &hrm_selector(0), 5000)
            .await
            .expect("B reads under flood");
        assert_eq!(value, vec![0x42]);
        // A's lossless stream that cannot keep up shows a visible terminal,
        // not radio silence.
        let peer_a = central.peer_key_for("peer-f07a").await.expect("peer A");
        let path_a = central
            .with_core(|core| core.resolve_path(&peer_a, &hrm_selector(0)).expect("path"))
            .await;
        let mut failed = false;
        for _ in 0..400 {
            if central
                .with_core(|core| core.consumer_state(path_a, "consumer-a"))
                .await
                == Some(ConsumerState::Failed)
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(failed, "flooded lossless stream terminates visibly");
    }

    #[tokio::test]
    async fn f24_connect_cleanup_releases_core_lock() {
        let central = open().await;
        ready_peer(&central, "peer-f24b", vec![hrm_service()]).await;
        let selector_b = hrm_selector(0);
        // A stalls in half-open cleanup behind the disconnect gate.
        central.boundary().block_op(FaultOp::Disconnect);
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let failing = central.clone();
        let pending_a =
            tokio::spawn(async move { failing.connect("peer-f24a", "lease-a", 5000).await });
        let mut cleaning = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned())
            {
                cleaning = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(cleaning, "A reaches compensating disconnect");
        // B progresses while A's radio cleanup is still stalled: its read
        // must not wait behind the core lock.
        let reader = central.clone();
        let selector_clone = selector_b.clone();
        let pending_b =
            tokio::spawn(async move { reader.read("peer-f24b", &selector_clone, 5000).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            pending_b.is_finished(),
            "B's operation never waits on A's radio cleanup"
        );
        let value = pending_b.await.expect("B task").expect("B reads");
        assert_eq!(value, vec![0x42]);
        // B's notifications also flow while A cleans up.
        central
            .subscribe("peer-f24b", &selector_b, "consumer-b", 5000)
            .await
            .expect("B subscribes under A's cleanup");
        central
            .boundary()
            .push_event(notification("peer-f24b", 0, vec![0x0B]));
        let mut delivered = None;
        for _ in 0..200 {
            delivered = central
                .take_notification("peer-f24b", &selector_b, "consumer-b")
                .await
                .expect("take");
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(delivered, Some(vec![0x0B]));
        central.boundary().unblock_op(FaultOp::Disconnect);
        let outcome_a = pending_a.await.expect("A task");
        assert_eq!(
            outcome_a.expect_err("A still fails").code_str(),
            "connection.failed"
        );
    }

    #[tokio::test]
    async fn f03_shutdown_during_connect_cancels() {
        let central = open().await;
        central.boundary().block_op(FaultOp::Connect);
        let connector = central.clone();
        let pending =
            tokio::spawn(async move { connector.connect("peer-f03x", "lease-a", 10_000).await });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Shutdown must not hang on the gated radio: it cancels in-flight
        // work, joins the loop, and destroys promptly.
        tokio::time::timeout(Duration::from_secs(5), central.shutdown())
            .await
            .expect("shutdown completes despite gated radio");
        central.boundary().unblock_op(FaultOp::Connect);
        let outcome = pending.await.expect("connect task");
        let error = outcome.expect_err("shutdown wins over late radio success");
        assert!(
            error.code_str() == "operation.aborted"
                || error.code_str() == "adapter.unavailable"
                || error.code_str() == "lifecycle.destroyed",
            "cancelled/destroyed, got {}",
            error.code_str()
        );
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            0,
            "no live leak after shutdown race"
        );
    }

    #[tokio::test]
    async fn f03_dropped_caller_still_releases() {
        let central = open().await;
        ready_peer(&central, "peer-f03q", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        let reader = central.clone();
        let selector_clone = selector.clone();
        let pending =
            tokio::spawn(async move { reader.read("peer-f03q", &selector_clone, 5000).await });
        for _ in 0..200 {
            if !central
                .with_core(|core| core.live_operation_ids())
                .await
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Drop the caller future while the radio is still gated: the runtime
        // keeps the native work owned until cleanup finishes.
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Read);
        // Give any detached cleanup a moment, then require reclamation.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline,
            "dropped caller still reaps its op"
        );
        let value = central
            .read("peer-f03q", &selector, 5000)
            .await
            .expect("admission remains after drop");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn f03_dropped_write_still_releases() {
        let central = open().await;
        let peer = "peer-f03w".to_owned();
        ready_peer(&central, &peer, vec![battery_service()]).await;
        central.boundary().set_mtu(&peer, 23);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Write);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            caller
                .write(
                    &peer_clone,
                    &selector_clone,
                    vec![0x02],
                    "with-response",
                    5000,
                )
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_live, "write in flight before abort (non-vacuous)");
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Write);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped write still reaps its op");
        central
            .write(&peer, &selector, vec![0x02], "with-response", 5000)
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_connect_still_releases() {
        let central = open().await;
        let peer = "peer-f03c".to_owned();
        central.boundary().push_event(advertisement(&peer));
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Connect);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let pending =
            tokio::spawn(async move { caller.connect(&peer_clone, "lease-x", 10_000).await });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_live, "connect in flight before abort (non-vacuous)");
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Connect);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped connect still reaps its op");
        central
            .connect(&peer, "lease-x", 10_000)
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_subscribe_still_releases() {
        let central = open().await;
        let peer = "peer-f03s".to_owned();
        ready_peer(&central, &peer, vec![hrm_service()]).await;
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Subscribe);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let pending = tokio::spawn(async move {
            caller
                .subscribe(&peer_clone, &hrm_selector(0), "consumer-drop", 5000)
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(saw_live, "subscribe in flight before abort (non-vacuous)");
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Subscribe);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped subscribe still reaps its op");
        central
            .subscribe(&peer, &hrm_selector(0), "consumer-drop", 5000)
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_read_descriptor_still_releases() {
        let central = open().await;
        let peer = "peer-f03rd".to_owned();
        ready_peer(&central, &peer, vec![hrm_service()]).await;
        central.boundary().set_mtu(&peer, 23);
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Read);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            caller
                .read_descriptor(&peer_clone, &selector_clone, 5000)
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_live,
            "descriptor read in flight before abort (non-vacuous)"
        );
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Read);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(live, baseline, "dropped descriptor read still reaps its op");
        central
            .read_descriptor(&peer, &selector, 5000)
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f03_dropped_write_descriptor_still_releases() {
        let central = open().await;
        let peer = "peer-f03wd".to_owned();
        ready_peer(&central, &peer, vec![hrm_service()]).await;
        central.boundary().set_mtu(&peer, 23);
        let selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let baseline = central.with_core(|core| core.live_operation_count()).await;
        central.boundary().block_op(FaultOp::Write);
        let caller = central.clone();
        let peer_clone = peer.clone();
        let selector_clone = selector.clone();
        let pending = tokio::spawn(async move {
            caller
                .write_descriptor(&peer_clone, &selector_clone, vec![1], 5000)
                .await
        });
        let mut saw_live = false;
        for _ in 0..200 {
            let live = central.with_core(|core| core.live_operation_count()).await;
            if live > baseline {
                saw_live = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_live,
            "descriptor write in flight before abort (non-vacuous)"
        );
        pending.abort();
        let _ = pending.await;
        central.boundary().unblock_op(FaultOp::Write);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = central.with_core(|core| core.live_operation_count()).await;
        assert_eq!(
            live, baseline,
            "dropped descriptor write still reaps its op"
        );
        central
            .write_descriptor(&peer, &selector, vec![1], 5000)
            .await
            .expect("post-drop admission");
    }

    #[tokio::test]
    async fn f02_sequential_ops_reclaim_aggregate_and_observations() {
        let central = open().await;
        ready_peer(&central, "peer-f02", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        let baseline_live = central.with_core(|core| core.live_operation_count()).await;
        let baseline_typed = central.with_core(|core| core.typed_effects().len()).await;
        for i in 0..1000u32 {
            let value = central
                .read("peer-f02", &selector, 5000)
                .await
                .unwrap_or_else(|error| panic!("read {i} admitted: {error:?}"));
            assert_eq!(value, vec![0x42]);
            if i % 250 == 0 {
                assert_eq!(
                    central.with_core(|core| core.live_operation_count()).await,
                    baseline_live,
                    "live reclaimed at iteration {i}"
                );
                assert!(
                    central.with_core(|core| core.typed_effects().len()).await
                        <= baseline_typed + 4,
                    "typed ledger recycled at iteration {i}"
                );
            }
        }
        for _ in 0..20u32 {
            central
                .start_scan("owner-f02", &[], 5000)
                .await
                .expect("scan admitted");
            central.stop_scan().await.expect("scan stopped");
        }
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            baseline_live,
            "live returns to baseline after 1000 reads + scan cycles"
        );
        assert!(
            central.with_core(|core| core.typed_effects().len()).await <= baseline_typed + 4,
            "observations recycled"
        );
        let value = central
            .read("peer-f02", &selector, 5000)
            .await
            .expect("admission remains");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn f15_shutdown_drives_incremental_destroy_with_pending_work() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        central.boundary().block_op(FaultOp::Connect);
        // 16 dispatched connects pile up behind the blocked radio with
        // generous deadlines, so shutdown — not a timeout — must settle
        // every remainder and ack every terminal. (One gate waiter releases
        // per unblock, so the stuck callers resolve via their own deadline
        // arms after shutdown settles their ops.)
        let mut pending = Vec::new();
        for index in 0..16u32 {
            let task = central.clone();
            let peer = format!("peer-f15-{index}");
            let lease = format!("lease-{index}");
            pending.push(tokio::spawn(async move {
                task.connect(&peer, &lease, 5000).await
            }));
        }
        for _ in 0..200 {
            if central.with_core(|core| core.live_operation_count()).await == 16 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            16,
            "all 16 connects dispatched behind the blocked radio"
        );
        let report = tokio::time::timeout(Duration::from_secs(10), central.shutdown())
            .await
            .expect("shutdown completes while callers are stuck");
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(
            record.state(),
            CleanupState::Released,
            "cancelled-then-acked workload releases clean"
        );
        assert!(record.failures().is_empty(), "no failures preserved");
        assert!(
            report.destroy_steps >= 2,
            "settle+ack workload needs more than the single idle pass"
        );
        assert!(
            report.radio_close_failures.is_empty(),
            "no subscriptions were live, so no close receipts"
        );
        assert_eq!(
            central.with_core(|core| core.live_operation_count()).await,
            0,
            "no live ops remain after acknowledged destroy"
        );
        // The stuck callers resolve via their deadline arms: their ops
        // already settled under shutdown, so they observe the winning
        // terminal (cancelled family) instead of hanging or succeeding late.
        for task in pending {
            let outcome = tokio::time::timeout(Duration::from_secs(15), task)
                .await
                .expect("caller resolves via deadline")
                .expect("task joins");
            assert!(outcome.is_err(), "stuck connect fails, never succeeds late");
        }
        // Admission stays closed after the acknowledged shutdown.
        let error = central
            .connect("peer-late", "lease-late", 5000)
            .await
            .expect_err("no connects after shutdown");
        assert_eq!(error.code_str(), "adapter.unavailable");
    }

    #[tokio::test]
    async fn f15_shutdown_record_preserves_disconnect_failure() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        ready_peer(&central, "peer-f15d", vec![hrm_service()]).await;
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "os refused");
        let report = central.shutdown().await;
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "shutdown releases the owned link"
        );
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(
            record.state(),
            CleanupState::ReleaseFailed,
            "refused disconnect flips the final record"
        );
        assert_eq!(
            record.failures().len(),
            1,
            "the refused disconnect is preserved exactly once"
        );
        let peer_key = central
            .peer_key_for("peer-f15d")
            .await
            .expect("peer still known");
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&peer_key))
                .await,
            Some(ConnectionState::Disconnecting),
            "failed release leaves the link Disconnecting, never silently Connected"
        );
    }

    #[tokio::test]
    async fn f14_shutdown_releases_owned_link() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        ready_peer(&central, "peer-f14l", vec![hrm_service()]).await;
        let report = central.shutdown().await;
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "shutdown releases the owned OS link"
        );
        let peer_key = central
            .peer_key_for("peer-f14l")
            .await
            .expect("peer still known");
        assert_eq!(
            central
                .with_core(|core| core.connection_state(&peer_key))
                .await,
            Some(ConnectionState::Disconnected),
            "released link confirms Disconnected in the core"
        );
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(record.state(), CleanupState::Released);
        assert!(report.radio_close_failures.is_empty());
        // Idempotent: a second shutdown re-reports clean without new radio work.
        let disconnects = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "disconnect")
            .count();
        let again = central.shutdown().await;
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "disconnect")
                .count(),
            disconnects,
            "repeat shutdown issues no new disconnects"
        );
        assert_eq!(
            again.record.expect("second drive succeeds").state(),
            CleanupState::Released
        );
    }

    #[tokio::test]
    async fn f14_shutdown_surfaces_close_failures() {
        use ubm_core::ownership::CleanupState;

        let central = open().await;
        ready_peer(&central, "peer-f14c", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-f14c", &selector, "consumer-c", 5000)
            .await
            .expect("subscribe");
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "os stuck");
        let report = central.shutdown().await;
        assert_eq!(
            report.radio_close_failures.len(),
            1,
            "exactly one close receipt"
        );
        assert_eq!(report.radio_close_failures[0].detail, "os stuck");
        assert_eq!(report.radio_close_failures[0].scope.0, "peer-f14c");
        assert_eq!(
            central.boundary().live_subscription_count(),
            1,
            "failed scope stays live at the radio"
        );
        // The core-tracked cleanup still succeeded: the radio failure is
        // reported alongside in the same report, neither hidden inside the
        // core record nor conflated with it.
        let record = report.record.expect("destroy drive succeeds");
        assert_eq!(record.state(), CleanupState::Released);
    }
}

/// Test-only access to the core for state assertions. Real hosts observe
/// through the typed API, never through this lock.
#[cfg(test)]
pub(crate) mod test_support {
    use super::DesktopCentral;
    use crate::boundary::RadioBoundary;

    impl<B: RadioBoundary> DesktopCentral<B> {
        pub(crate) async fn with_core<T>(
            &self,
            view: impl FnOnce(&ubm_core::central::Central) -> T,
        ) -> T {
            let core = self.inner.core.lock().await;
            view(&core)
        }

        pub(crate) async fn with_core_mut<T>(
            &self,
            view: impl FnOnce(&mut ubm_core::central::Central) -> T,
        ) -> T {
            let mut core = self.inner.core.lock().await;
            view(&mut core)
        }
    }
}
