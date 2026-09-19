//! [`ForeignRadio`]: the [`RadioBoundary`] over a [`PlatformRadio`].
//!
//! Every boundary verb becomes one typed [`RadioRequest`]. The pending
//! entry is inserted before `submit` (no lock held across it), so a
//! platform that answers synchronously still finds its waiter. Dropping a
//! request future (budget expiry, cancel, task abort) removes the entry
//! and calls [`PlatformRadio::cancel`] exactly once; an answer that
//! arrives afterwards is counted as a late completion, never dropped
//! silently.
//!
//! Ingress travels three bounded queues (advertisements, notification
//! data, control) with one shared push sequence, so drain follows arrival
//! order and a data flood can neither exhaust memory nor push out a
//! disconnect. Every drop is counted per class and reported to the host.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Notify, oneshot};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use ubm_desktop::boundary::AdapterPowerState;
use ubm_desktop::{
    Budget, DeliveryMode, DesktopError, ObservedDelivery, OpControl, OpTicket, PeerSnapshot,
    RadioBoundary, RadioCloseFailure, RadioEvent, ScanFilterSpec, ServiceSnapshot,
};

use crate::radio::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, DescriptorAddress,
    IngressClass, Instance, MobilePlatform, Phy, PlatformFailure, PlatformRadio, RadioCompletion,
    RadioRequest, RequestId, RequestKind, ScanRequest,
};

// Legacy React Native queued 512 records and 1 MiB per binding across every
// record class (`kMaximumQueuedRecords`); each class here holds at least
// that on its own (finding 108, docs/MOBILE_RUST_WIRE.md "Memory bounds").

/// Queued advertisements before a new one is refused (counted).
pub const ADVERTISEMENT_INGRESS_CAP: usize = 512;
/// Queued notification values before a new one is refused (counted).
pub const NOTIFICATION_INGRESS_CAP: usize = 1024;
/// Queued notification bytes before a new value is refused (counted).
pub const NOTIFICATION_INGRESS_BYTES: usize = 1 << 20;
/// Queued control facts (link, service change, adapter, security,
/// restoration) before a new one is refused (counted).
pub const CONTROL_INGRESS_CAP: usize = 512;

tokio::task_local! {
    /// Set when a write request was actually submitted to the platform in
    /// the current op task: the difference between `not-dispatched` and
    /// `uncertain` for a failed write.
    pub static DISPATCHED: Cell<bool>;
    /// The platform requests the current op task has outstanding: what
    /// makes a session op `dispatched` rather than `queued` in its
    /// session's counters.
    pub static OP_RADIO: Arc<OpRadio>;
}

/// Platform requests one op has at the radio right now.
#[derive(Debug, Default)]
pub struct OpRadio {
    in_flight: AtomicU64,
}

impl OpRadio {
    /// Requests submitted to the platform and not yet answered or dropped.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::SeqCst)
    }
}

/// Counts one submitted request against its op until answered or dropped.
struct InFlight(Option<Arc<OpRadio>>);

impl InFlight {
    fn enter() -> Self {
        let tracker = OP_RADIO.try_with(Arc::clone).ok();
        if let Some(tracker) = &tracker {
            tracker.in_flight.fetch_add(1, Ordering::SeqCst);
        }
        Self(tracker)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(tracker) = &self.0 {
            tracker.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Connect facts the boundary verb cannot carry, staged per peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectStaging {
    /// Android `autoConnect` (the `when-available` intent).
    pub auto_connect: bool,
    /// LE PHYs to establish the link on; empty = no preference.
    pub preferred_phy: Vec<Phy>,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type DropHook = Arc<dyn Fn(IngressClass) + Send + Sync>;

#[derive(Default)]
struct Ingress {
    advertisements: VecDeque<(u64, RadioEvent)>,
    data: VecDeque<(u64, RadioEvent)>,
    data_bytes: usize,
    control: VecDeque<(u64, RadioEvent)>,
    sequence: u64,
    closed: bool,
}

struct Pending {
    kind: RequestKind,
    tx: oneshot::Sender<RadioCompletion>,
}

struct Shared {
    platform: Arc<dyn PlatformRadio>,
    /// Which platform answers: selects the legacy error identity (113).
    mobile_platform: MobilePlatform,
    adapter_label: String,
    next_id: AtomicU64,
    pending: Mutex<HashMap<RequestId, Pending>>,
    late_completions: AtomicU64,
    mismatched_completions: AtomicU64,
    ingress: Mutex<Ingress>,
    notify: Notify,
    drops: [AtomicU64; 3],
    drop_hook: Mutex<Option<DropHook>>,
    staged_scan: Mutex<Option<ScanRequest>>,
    staged_connect: Mutex<HashMap<String, ConnectStaging>>,
    /// Per-peer connect sections (X-R3): at most one same-peer connect
    /// stages, dispatches and cleans up at a time, so staging stays bound
    /// to its operation without an operation identity on the boundary verb.
    /// One small entry per peer ever connected, like the host's other
    /// per-peer maps; entries are never removed because a removal racing a
    /// new waiter would split the section in two.
    connect_sections: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    staged_preference: Mutex<HashMap<Instance, DeliveryMode>>,
    close_failures: Mutex<Vec<RadioCloseFailure>>,
    close_error: Mutex<Option<DesktopError>>,
}

/// The central's radio boundary over the native adapter. Cloning shares
/// one radio: the central holds one clone, the host another for
/// completions and ingress.
#[derive(Clone)]
pub struct ForeignRadio {
    shared: Arc<Shared>,
}

/// Radio-level counters for `counters.describe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RadioCounters {
    pub pending_requests: u64,
    pub late_completions: u64,
    pub mismatched_completions: u64,
    pub advertisement_drops: u64,
    pub notification_drops: u64,
    pub control_drops: u64,
}

/// Outcome of handing one completion to the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    /// Delivered to its waiter.
    Delivered,
    /// No waiter: the request was cancelled/abandoned or never issued.
    /// Counted in `late_completions`.
    Late,
    /// Wrong answer shape for the request: delivered as a
    /// `protocol.malformed` failure and counted.
    Mismatched,
}

fn class_index(class: IngressClass) -> usize {
    match class {
        IngressClass::Advertisement => 0,
        IngressClass::Notification => 1,
        IngressClass::Control => 2,
    }
}

/// Removes the pending entry and cancels the platform request when the
/// awaiting future is dropped before its answer.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: RequestId,
    armed: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let removed = lock(&self.shared.pending).remove(&self.id).is_some();
        if removed {
            self.shared.platform.cancel(self.id);
        }
    }
}

fn malformed(kind: RequestKind, detail: &str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::ProtocolMalformed,
        BleErrorDomain::Boundary,
        kind.operation(),
    )
    .with_detail(detail.to_owned())
}

fn failed(failure: &PlatformFailure, kind: RequestKind, platform: MobilePlatform) -> DesktopError {
    failure.to_error(kind, platform)
}

fn instance_of(
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
) -> Instance {
    Instance {
        peer_id: peer_id.to_owned(),
        service_uuid: service_uuid.to_owned(),
        service_occurrence,
        characteristic_uuid: characteristic_uuid.to_owned(),
        characteristic_occurrence,
    }
}

impl ForeignRadio {
    #[must_use]
    pub fn new(
        platform: Arc<dyn PlatformRadio>,
        mobile_platform: MobilePlatform,
        adapter_label: String,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                platform,
                mobile_platform,
                adapter_label,
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                late_completions: AtomicU64::new(0),
                mismatched_completions: AtomicU64::new(0),
                ingress: Mutex::new(Ingress::default()),
                notify: Notify::new(),
                drops: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
                drop_hook: Mutex::new(None),
                staged_scan: Mutex::new(None),
                staged_connect: Mutex::new(HashMap::new()),
                connect_sections: Mutex::new(HashMap::new()),
                staged_preference: Mutex::new(HashMap::new()),
                close_failures: Mutex::new(Vec::new()),
                close_error: Mutex::new(None),
            }),
        }
    }

    /// Install the callback that learns about every ingress drop.
    pub fn set_drop_hook(&self, hook: DropHook) {
        *lock(&self.shared.drop_hook) = Some(hook);
    }

    /// Issue one request and wait for its answer. The request id is minted
    /// here; the future is cancel-safe (drop = platform cancel).
    pub async fn call(
        &self,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        let shared = &*self.shared;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let request = make(id);
        let kind = request.kind();
        let (tx, rx) = oneshot::channel();
        lock(&shared.pending).insert(id, Pending { kind, tx });
        let mut guard = PendingGuard {
            shared,
            id,
            armed: true,
        };
        if kind == RequestKind::Write || kind == RequestKind::WriteDescriptor {
            let _ = DISPATCHED.try_with(|flag| flag.set(true));
        }
        let _in_flight = InFlight::enter();
        shared.platform.submit(request);
        let answer = rx.await;
        guard.armed = false;
        match answer {
            Ok(RadioCompletion::Failed(failure)) => {
                Err(failed(&failure, kind, self.shared.mobile_platform))
            }
            Ok(completion) => Ok(completion),
            // The sender only drops without sending when the host shuts
            // the radio down under the waiter.
            Err(_) => Err(DesktopError::new(
                BleErrorCode::LifecycleDestroyed,
                BleErrorDomain::Core,
                kind.operation(),
            )
            .with_detail("radio host closed before the platform answered")),
        }
    }

    /// Deliver one platform answer.
    pub fn complete(&self, request_id: RequestId, completion: RadioCompletion) -> CompletionStatus {
        let entry = lock(&self.shared.pending).remove(&request_id);
        let Some(pending) = entry else {
            self.shared.late_completions.fetch_add(1, Ordering::Relaxed);
            return CompletionStatus::Late;
        };
        if completion.answers(pending.kind) {
            if pending.tx.send(completion).is_err() {
                self.shared.late_completions.fetch_add(1, Ordering::Relaxed);
                return CompletionStatus::Late;
            }
            return CompletionStatus::Delivered;
        }
        self.shared
            .mismatched_completions
            .fetch_add(1, Ordering::Relaxed);
        let failure = PlatformFailure::new(
            crate::radio::FailureKind::Platform,
            format!("completion shape does not answer {}", pending.kind.as_str()),
        );
        let _ = pending.tx.send(RadioCompletion::Failed(failure));
        CompletionStatus::Mismatched
    }

    /// Queue one event for the central's event loop. Returns the class
    /// that dropped it when its queue was full.
    pub fn push_event(&self, event: RadioEvent) -> Result<(), IngressClass> {
        let mut ingress = lock(&self.shared.ingress);
        if ingress.closed {
            return Ok(());
        }
        ingress.sequence += 1;
        let sequence = ingress.sequence;
        let outcome = match event {
            RadioEvent::Advertisement(_) => {
                if ingress.advertisements.len() >= ADVERTISEMENT_INGRESS_CAP {
                    Err(IngressClass::Advertisement)
                } else {
                    ingress.advertisements.push_back((sequence, event));
                    Ok(())
                }
            }
            RadioEvent::Notification { ref value, .. } => {
                let bytes = value.len();
                if ingress.data.len() >= NOTIFICATION_INGRESS_CAP
                    || ingress.data_bytes + bytes > NOTIFICATION_INGRESS_BYTES
                {
                    Err(IngressClass::Notification)
                } else {
                    ingress.data_bytes += bytes;
                    ingress.data.push_back((sequence, event));
                    Ok(())
                }
            }
            control => {
                if ingress.control.len() >= CONTROL_INGRESS_CAP {
                    Err(IngressClass::Control)
                } else {
                    ingress.control.push_back((sequence, control));
                    Ok(())
                }
            }
        };
        drop(ingress);
        match outcome {
            Ok(()) => {
                self.shared.notify.notify_one();
                Ok(())
            }
            Err(class) => {
                self.note_drop(class);
                Err(class)
            }
        }
    }

    /// Count one ingress drop and tell the host.
    pub fn note_drop(&self, class: IngressClass) {
        self.shared.drops[class_index(class)].fetch_add(1, Ordering::Relaxed);
        let hook = lock(&self.shared.drop_hook).clone();
        if let Some(hook) = hook {
            hook(class);
        }
    }

    /// End the event source: the central's loop sees `None` after the
    /// queues drain.
    pub fn close_events(&self) {
        lock(&self.shared.ingress).closed = true;
        self.shared.notify.notify_one();
    }

    /// Stage the platform facts of the next scan start (addresses and
    /// Android settings the boundary verb cannot carry).
    pub fn stage_scan(&self, scan: ScanRequest) {
        *lock(&self.shared.staged_scan) = Some(scan);
    }

    /// Enter this peer's connect section (X-R3): hold the guard across
    /// stage, dispatch and cleanup, so a concurrent same-peer connect can
    /// neither overwrite this operation's staging nor wipe it in its own
    /// cleanup. The wait is cancellation- and deadline-aware like the scan
    /// admission section: a dead op never stages. Passing the options
    /// through the boundary instead would need an operation identity on
    /// `RadioBoundary::connect`, which is ubm-desktop's contract.
    pub async fn lock_connect_section(
        &self,
        peer_id: &str,
        ctl: &OpControl,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, DesktopError> {
        fn refused(ticket: &OpTicket, budget: &Budget) -> Option<DesktopError> {
            if ticket.is_cancel_requested() {
                return Some(DesktopError::new(
                    BleErrorCode::OperationAborted,
                    BleErrorDomain::Connection,
                    "connection.connect",
                ));
            }
            if budget.is_expired() {
                return Some(DesktopError::new(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    "connection.connect",
                ));
            }
            None
        }
        if let Some(error) = refused(&ctl.ticket, &ctl.budget) {
            return Err(error);
        }
        let section = lock(&self.shared.connect_sections)
            .entry(peer_id.to_owned())
            .or_default()
            .clone();
        let aborted = || {
            refused(&ctl.ticket, &ctl.budget).unwrap_or_else(|| {
                DesktopError::new(
                    BleErrorCode::OperationAborted,
                    BleErrorDomain::Connection,
                    "connection.connect",
                )
            })
        };
        match ctl.budget.remaining() {
            Some(wait) => {
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => Err(aborted()),
                    () = tokio::time::sleep(wait) => Err(DesktopError::new(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Connection,
                        "connection.connect",
                    )),
                    guard = section.lock_owned() => Ok(guard),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    () = ctl.ticket.cancelled() => Err(aborted()),
                    guard = section.lock_owned() => Ok(guard),
                }
            }
        }
    }

    /// Stage the connect intent for the next connect to `peer_id`. Only the
    /// holder of the peer's connect section may stage: with the section
    /// held, nothing foreign can be present, so dispatch consumes exactly
    /// what this operation staged and cleanup removes only its own.
    pub fn stage_connect(&self, peer_id: &str, staging: ConnectStaging) {
        lock(&self.shared.staged_connect).insert(peer_id.to_owned(), staging);
    }

    /// Drop whatever staging a verb left unconsumed (the central refused it
    /// before reaching the radio), so it can never leak into a later call.
    pub fn clear_staging(&self, peer_id: Option<&str>, instance: Option<&Instance>, scan: bool) {
        if let Some(peer_id) = peer_id {
            lock(&self.shared.staged_connect).remove(peer_id);
        }
        if let Some(instance) = instance {
            lock(&self.shared.staged_preference).remove(instance);
        }
        if scan {
            lock(&self.shared.staged_scan).take();
        }
    }

    /// Stage a soft delivery preference for the next enable of `instance`.
    pub fn stage_preference(&self, instance: Instance, preferred: DeliveryMode) {
        lock(&self.shared.staged_preference).insert(instance, preferred);
    }

    /// The failure of the last whole close request, if it failed.
    #[must_use]
    pub fn close_error(&self) -> Option<DesktopError> {
        lock(&self.shared.close_error).clone()
    }

    #[must_use]
    pub fn counters(&self) -> RadioCounters {
        let shared = &*self.shared;
        RadioCounters {
            pending_requests: lock(&shared.pending).len() as u64,
            late_completions: shared.late_completions.load(Ordering::Relaxed),
            mismatched_completions: shared.mismatched_completions.load(Ordering::Relaxed),
            advertisement_drops: shared.drops[0].load(Ordering::Relaxed),
            notification_drops: shared.drops[1].load(Ordering::Relaxed),
            control_drops: shared.drops[2].load(Ordering::Relaxed),
        }
    }

    /// Fail every waiting request (host teardown): no waiter is left
    /// hanging on a platform that will never answer.
    pub fn abandon_pending(&self) {
        let pending: Vec<Pending> = lock(&self.shared.pending)
            .drain()
            .map(|(_, pending)| pending)
            .collect();
        for entry in pending {
            let failure = PlatformFailure::new(
                crate::radio::FailureKind::Cancelled,
                "radio host shut down before the platform answered",
            );
            let _ = entry.tx.send(RadioCompletion::Failed(failure));
        }
    }

    fn unexpected(kind: RequestKind) -> DesktopError {
        malformed(kind, "unexpected completion")
    }

    async fn unit(&self, make: impl FnOnce(RequestId) -> RadioRequest) -> Result<(), DesktopError> {
        match self.call(make).await? {
            RadioCompletion::Unit => Ok(()),
            _ => Err(Self::unexpected(RequestKind::Close)),
        }
    }

    async fn bytes(
        &self,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<Vec<u8>, DesktopError> {
        match self.call(make).await? {
            RadioCompletion::Bytes(bytes) => Ok(bytes),
            _ => Err(Self::unexpected(RequestKind::ReadDescriptor)),
        }
    }

    /// Current adapter facts (richer than [`RadioBoundary::adapter_state`]).
    pub async fn adapter_snapshot(&self) -> Result<AdapterSnapshot, DesktopError> {
        match self.call(|id| RadioRequest::AdapterState { id }).await? {
            RadioCompletion::Adapter(snapshot) => Ok(snapshot),
            _ => Err(Self::unexpected(RequestKind::AdapterState)),
        }
    }
}

/// Whether the legacy React Native backends counted this snapshot as a lost
/// adapter (origin/main `corebluetooth-backend.ts` `handleAdapterState`):
/// not available, a blocking authorization (`isAuthorizationBlocking`:
/// denied, restricted, unavailable), or power other than on. Answered as the
/// central's loss state, so the core's once-per-episode reset runs on
/// exactly those snapshots; `None` is a usable adapter, which ends the
/// episode. The state is the loss's cause only: the `adapter` records carry
/// the platform's own snapshot.
#[must_use]
pub fn legacy_loss(snapshot: &AdapterSnapshot) -> Option<AdapterPowerState> {
    match snapshot.power {
        AdapterPower::Off => return Some(AdapterPowerState::PoweredOff),
        AdapterPower::Resetting | AdapterPower::Unknown => {
            return Some(AdapterPowerState::Resetting);
        }
        AdapterPower::Unsupported => return Some(AdapterPowerState::Unsupported),
        AdapterPower::On => {}
    }
    match snapshot.availability {
        AdapterAvailability::Available => {}
        AdapterAvailability::Unknown => return Some(AdapterPowerState::Resetting),
        AdapterAvailability::Unavailable | AdapterAvailability::Unsupported => {
            return Some(AdapterPowerState::Unsupported);
        }
    }
    match snapshot.authorization {
        AdapterAuthorization::Denied
        | AdapterAuthorization::Restricted
        | AdapterAuthorization::Unavailable => Some(AdapterPowerState::Unauthorized),
        AdapterAuthorization::Granted
        | AdapterAuthorization::NotDetermined
        | AdapterAuthorization::Unknown => None,
    }
}

/// The adapter state the central is told for one platform snapshot: powered
/// on for a usable adapter, the legacy loss otherwise ([`legacy_loss`]).
#[must_use]
pub fn central_adapter_state(snapshot: &AdapterSnapshot) -> AdapterPowerState {
    legacy_loss(snapshot).unwrap_or(AdapterPowerState::PoweredOn)
}

/// Desktop power projection of the platform adapter facts.
#[must_use]
pub fn power_state(snapshot: &AdapterSnapshot) -> AdapterPowerState {
    match snapshot.power {
        AdapterPower::On => AdapterPowerState::PoweredOn,
        AdapterPower::Off => AdapterPowerState::PoweredOff,
        AdapterPower::Resetting | AdapterPower::Unsupported | AdapterPower::Unknown => {
            AdapterPowerState::Unknown
        }
    }
}

impl RadioBoundary for ForeignRadio {
    /// The adapter identity the platform supplied at host open (Android
    /// adapter address, `"corebluetooth"` on Apple).
    async fn adapter_name(&self) -> Result<String, DesktopError> {
        if self.shared.adapter_label.is_empty() {
            return Err(DesktopError::adapter_unavailable("adapter.name")
                .with_detail("the platform supplied no adapter identity"));
        }
        Ok(self.shared.adapter_label.clone())
    }

    async fn start_scan(&self, filter: ScanFilterSpec) -> Result<(), DesktopError> {
        let staged = lock(&self.shared.staged_scan).take().unwrap_or_default();
        let scan = ScanRequest {
            service_uuids: filter.service_uuids,
            device_addresses: staged.device_addresses,
            android: staged.android,
        };
        self.unit(|id| RadioRequest::StartScan { id, scan }).await
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        self.unit(|id| RadioRequest::StopScan { id }).await
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        Err(DesktopError::new(
            BleErrorCode::CapabilityUnsupported,
            BleErrorDomain::Capability,
            "radio.peers",
        )
        .with_detail("the mobile host keeps its own peer directory"))
    }

    async fn connect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let ConnectStaging {
            auto_connect,
            preferred_phy,
        } = lock(&self.shared.staged_connect)
            .remove(peer_id)
            .unwrap_or_default();
        let peer_id = peer_id.to_owned();
        self.unit(|id| RadioRequest::Connect {
            id,
            peer_id,
            auto_connect,
            preferred_phy,
        })
        .await
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peer_id = peer_id.to_owned();
        self.unit(|id| RadioRequest::Disconnect { id, peer_id })
            .await
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        let peer_id = peer_id.to_owned();
        match self
            .call(|id| RadioRequest::Discover { id, peer_id })
            .await?
        {
            RadioCompletion::Discovered(services) => Ok(services),
            _ => Err(Self::unexpected(RequestKind::Discover)),
        }
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Result<ubm_desktop::CharacteristicRead, DesktopError> {
        let instance = instance_of(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        );
        match self.call(|id| RadioRequest::Read { id, instance }).await? {
            RadioCompletion::Read { value, provenance } => {
                Ok(ubm_desktop::CharacteristicRead { value, provenance })
            }
            _ => Err(Self::unexpected(RequestKind::Read)),
        }
    }

    async fn write_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DesktopError> {
        let instance = instance_of(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        );
        self.unit(|id| RadioRequest::Write {
            id,
            instance,
            value,
            with_response,
        })
        .await
    }

    async fn read_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        let descriptor = DescriptorAddress {
            instance: instance_of(
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
            ),
            descriptor_uuid: descriptor_uuid.to_owned(),
            descriptor_occurrence,
        };
        self.bytes(|id| RadioRequest::ReadDescriptor { id, descriptor })
            .await
    }

    async fn write_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
        value: Vec<u8>,
    ) -> Result<(), DesktopError> {
        let descriptor = DescriptorAddress {
            instance: instance_of(
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
            ),
            descriptor_uuid: descriptor_uuid.to_owned(),
            descriptor_occurrence,
        };
        self.unit(|id| RadioRequest::WriteDescriptor {
            id,
            descriptor,
            value,
        })
        .await
    }

    async fn set_notifications(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
        requested: Option<DeliveryMode>,
    ) -> Result<ObservedDelivery, DesktopError> {
        let instance = instance_of(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        );
        if !enable {
            return self
                .unit(|id| RadioRequest::DisableNotifications { id, instance })
                .await
                .map(|()| ObservedDelivery::Unknown);
        }
        let preferred = lock(&self.shared.staged_preference).remove(&instance);
        match self
            .call(|id| RadioRequest::EnableNotifications {
                id,
                instance,
                epoch,
                requested,
                preferred,
            })
            .await?
        {
            RadioCompletion::NotifyEnabled(observed) => Ok(observed),
            _ => Err(Self::unexpected(RequestKind::EnableNotifications)),
        }
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        let peer_id = peer_id.to_owned();
        match self.call(|id| RadioRequest::ReadMtu { id, peer_id }).await {
            Ok(RadioCompletion::Mtu(mtu)) => mtu,
            // An unmeasured MTU is reported unmeasured; a failed read is
            // withheld the same way, never guessed.
            _ => None,
        }
    }

    /// The platform's own per-mode answer (N1): Android's stack performs
    /// long writes with response and has a default ATT MTU before any
    /// exchange, CoreBluetooth answers `maximumWriteValueLength(for:)`, so
    /// neither is derived from [`Self::mtu`]. A failed read withholds the
    /// limit and the central fails the write closed before dispatch.
    async fn write_limits(&self, peer_id: &str) -> Option<ubm_desktop::WriteLimits> {
        let peer_id = peer_id.to_owned();
        match self
            .call(|id| RadioRequest::ReadWriteLimits { id, peer_id })
            .await
        {
            Ok(RadioCompletion::WriteLimits(limits)) => Some(limits),
            _ => None,
        }
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        loop {
            let notified = self.shared.notify.notified();
            {
                let mut ingress = lock(&self.shared.ingress);
                let heads = [
                    ingress.advertisements.front().map(|(seq, _)| *seq),
                    ingress.data.front().map(|(seq, _)| *seq),
                    ingress.control.front().map(|(seq, _)| *seq),
                ];
                let next = heads
                    .iter()
                    .enumerate()
                    .filter_map(|(index, seq)| seq.map(|seq| (seq, index)))
                    .min();
                match next {
                    Some((_, 0)) => return ingress.advertisements.pop_front().map(|(_, e)| e),
                    Some((_, 1)) => {
                        let event = ingress.data.pop_front().map(|(_, e)| e);
                        if let Some(RadioEvent::Notification { ref value, .. }) = event {
                            ingress.data_bytes = ingress.data_bytes.saturating_sub(value.len());
                        }
                        return event;
                    }
                    Some(_) => return ingress.control.pop_front().map(|(_, e)| e),
                    None if ingress.closed => return None,
                    None => {}
                }
            }
            notified.await;
        }
    }

    async fn close(&self) {
        lock(&self.shared.close_failures).clear();
        *lock(&self.shared.close_error) = None;
        // Bounded: a platform that never answers teardown must not wedge
        // the owner's shutdown; the expiry is reported, never assumed.
        let answer = tokio::time::timeout(
            ubm_desktop::LIVENESS_CLEANUP,
            self.call(|id| RadioRequest::Close { id }),
        )
        .await
        .unwrap_or_else(|_| {
            Err(DesktopError::new(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Cleanup,
                RequestKind::Close.operation(),
            )
            .with_detail(ubm_desktop::LIVENESS_BACKSTOP_DETAIL))
        });
        match answer {
            Ok(RadioCompletion::Closed(failures)) => {
                let receipts = failures
                    .into_iter()
                    .map(|failure| {
                        let instance = failure.instance;
                        RadioCloseFailure::new(
                            (
                                instance.peer_id,
                                instance.service_uuid,
                                instance.service_occurrence,
                                instance.characteristic_uuid,
                                instance.characteristic_occurrence,
                            ),
                            failure.detail,
                        )
                    })
                    .collect();
                *lock(&self.shared.close_failures) = receipts;
            }
            Ok(_) => {
                *lock(&self.shared.close_error) = Some(Self::unexpected(RequestKind::Close));
            }
            Err(error) => {
                *lock(&self.shared.close_error) = Some(error);
            }
        }
    }

    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        std::mem::take(&mut *lock(&self.shared.close_failures))
    }

    async fn read_rssi(&self, peer_id: &str) -> Result<i16, DesktopError> {
        let peer_id = peer_id.to_owned();
        match self
            .call(|id| RadioRequest::ReadRssi { id, peer_id })
            .await?
        {
            RadioCompletion::Rssi(rssi) => Ok(rssi),
            _ => Err(Self::unexpected(RequestKind::ReadRssi)),
        }
    }

    async fn adapter_state(&self) -> Result<AdapterPowerState, DesktopError> {
        self.adapter_snapshot()
            .await
            .map(|snapshot| power_state(&snapshot))
    }

    /// An adapter loss ends live work and advances the generations, as the
    /// legacy React Native backends did (`startAdapterLossCleanup`,
    /// `advanceGeneration`).
    fn tears_down_on_adapter_loss(&self) -> bool {
        true
    }
}
