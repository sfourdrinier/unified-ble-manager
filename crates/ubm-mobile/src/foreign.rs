//! [`ForeignRadio`]: the [`RadioBoundary`] over a [`PlatformRadio`].
//!
//! Every boundary verb becomes one typed [`RadioRequest`]. The pending
//! entry is inserted before `submit` (no lock held across it), so a
//! platform that answers synchronously still finds its waiter. Dropping a
//! request future (budget expiry, cancel, task abort) removes the entry
//! and calls [`PlatformRadio::cancel`] exactly once. Ordinary answers that
//! arrive afterwards are counted as late completions. Native owner-retirement
//! attempts retain their exact identity until an authoritative receipt or a
//! confirmed no-dispatch result, so a later receipt can still settle them.
//!
//! Ingress travels three bounded queues (advertisements, notification
//! data, control) with one shared push sequence, so drain follows arrival
//! order and a data flood can neither exhaust memory nor push out a
//! disconnect. Every drop is counted per class and reported to the host.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use tokio::sync::{Notify, oneshot};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use ubm_desktop::boundary::AdapterPowerState;
use ubm_desktop::{
    Budget, DeliveryMode, DesktopError, ObservedDelivery, OpControl, OpTicket, PeerSnapshot,
    RadioBoundary, RadioCloseFailure, RadioEvent, ScanFilterSpec, ServiceSnapshot,
};

use crate::host::HostInner;
use crate::radio::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, DescriptorAddress,
    IngressClass, Instance, MobilePlatform, Phy, PlatformFailure, PlatformRadio, RadioCompletion,
    RadioRequest, RequestId, RequestKind, ScanRequest,
};

#[path = "ordered_source.rs"]
mod ordered_source;

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
    /// Set when an effectful request was actually submitted to the platform in
    /// the current op task: the difference between `not-dispatched` and
    /// `uncertain` for a failed write or link-control request.
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

type DropHook = Arc<dyn Fn(IngressClass, u64) + Send + Sync>;

#[derive(Default)]
struct Ingress {
    scan_ingress_epoch: Option<u64>,
    advertisements: VecDeque<(u64, RadioEvent)>,
    data: VecDeque<(u64, RadioEvent)>,
    data_bytes: usize,
    control: VecDeque<(u64, RadioEvent)>,
    barriers: VecDeque<(u64, IngressBarrier)>,
    sequence: u64,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceFailureQueueStatus {
    Accepted,
    Full,
    Closed,
}

struct IngressBarrier {
    arrived: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

pub(crate) struct IngressFence(Option<oneshot::Sender<()>>);
impl Drop for IngressFence {
    fn drop(&mut self) {
        if let Some(resume) = self.0.take() {
            let _ = resume.send(());
        }
    }
}

struct Pending {
    kind: RequestKind,
    exact_request: Option<RadioRequest>,
    tx: oneshot::Sender<RadioCompletion>,
}

#[derive(Clone)]
struct RetainedV2Attempt {
    request: RadioRequest,
    completions: Vec<RadioCompletion>,
}

/// Opening groups retain their native result even when their original waiter is cancelled.
/// The host releases this slot only after adopting the group or completing its cleanup.
#[derive(Debug, Clone)]
pub struct RetainedOrderedOpen {
    pub request_id: RequestId,
    pub peer_id: String,
    pub connection_generation: String,
    pub database_generation: String,
    pub members: Vec<crate::OrderedMember>,
    pub completion: Option<RadioCompletion>,
    pub acknowledged: bool,
}

const RETAINED_ORDERED_OPEN_CAP: usize = 256;
const ORDERED_OPEN_ADMISSION_SLOTS: usize = 64;
pub(crate) type MobileOpenCoordinator =
    crate::ordered_open_slots::Coordinator<crate::OrderedOpenSpec, RadioCompletion>;
type MobileOpenRegistry =
    crate::ordered_open_slots::CoordinatorRegistry<crate::OrderedOpenSpec, RadioCompletion>;
static MOBILE_OPEN_REGISTRY: OnceLock<MobileOpenRegistry> = OnceLock::new();

fn mobile_open_registry() -> &'static MobileOpenRegistry {
    MOBILE_OPEN_REGISTRY.get_or_init(MobileOpenRegistry::default)
}
const ORDERED_OPEN_RECEIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
// Request identities must not restart when a ForeignRadio is replaced in this process.
static NEXT_RADIO_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
struct RetainedOrderedEnable {
    completion: Option<RadioCompletion>,
}

#[derive(Default)]
struct RetainedOrderedEnables {
    by_request: HashMap<RequestId, RetainedOrderedEnable>,
    by_activation: HashMap<crate::OrderedActivation, RequestId>,
    reserved: HashSet<crate::OrderedActivation>,
    abandoned: HashSet<crate::OrderedActivation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OrderedRegisterKey {
    generation: crate::OrderedSourceGeneration,
    group_id: u64,
    old: crate::OrderedRegistration,
}
#[derive(Debug, Clone)]
struct RetainedOrderedRegister {
    key: OrderedRegisterKey,
    completion: Option<RadioCompletion>,
}
#[derive(Default)]
struct RetainedOrderedRegisters {
    by_request: HashMap<RequestId, RetainedOrderedRegister>,
    by_key: HashMap<OrderedRegisterKey, RequestId>,
    resumed: HashMap<
        (crate::OrderedSourceGeneration, u64, u64),
        ubm_core::ordered_notifications::Membership,
    >,
}

#[derive(Clone)]
struct AuthorityOpenReceipt {
    result: tokio::sync::watch::Receiver<Option<Result<u64, DesktopError>>>,
    receipt_id: Arc<Mutex<Option<u64>>>,
}

#[derive(Clone)]
struct PermitOpenReceipt {
    key: ubm_desktop::boundary::OrderedReclaimPermitRequest,
    result: tokio::sync::watch::Receiver<Option<Result<u64, DesktopError>>>,
}

type RecordedDisables = HashMap<(Instance, u64), Arc<Mutex<Option<IngressFence>>>>;

struct Shared {
    platform: Arc<dyn PlatformRadio>,
    /// Which platform answers: selects the legacy error identity (113).
    mobile_platform: MobilePlatform,
    #[cfg(any(test, all(feature = "synthetic-marker-harness", debug_assertions)))]
    marker_supported_for_test: std::sync::atomic::AtomicBool,
    adapter_label: String,
    gatt_identities: Mutex<HashMap<String, ubm_desktop::boundary::GattSnapshotIdentity>>,
    reclaim_authority_opens: Mutex<HashMap<u64, AuthorityOpenReceipt>>,
    reclaim_permit_opens: Mutex<HashMap<(u64, u64), PermitOpenReceipt>>,
    pending: Mutex<HashMap<RequestId, Pending>>,
    retained_v2_attempts: Mutex<HashMap<RequestId, RetainedV2Attempt>>,
    // A dispatched retirement can outlive its waiter or report an ambiguous
    // failure before its authoritative native result arrives.
    retained_owner_attempts:
        Mutex<HashMap<RequestId, (RadioRequest, Option<crate::OwnerRetirementResult>)>>,
    owner_settlement_changed: Notify,
    ordered_recovery: Mutex<HashMap<RequestId, Vec<DesktopError>>>,
    // Central cleanup is independent of the native open receipt's lifetime.
    ordered_cleanup_due: Mutex<HashMap<u64, (String, RequestId)>>,
    retained_ordered_opens: Mutex<HashMap<u64, RetainedOrderedOpen>>,
    /// Native open admission must complete before recovery can submit a fence.
    ordered_open_submission: Mutex<HashMap<u64, Weak<tokio::sync::Mutex<()>>>>,
    ordered_open_lifecycle: Mutex<HashMap<u64, Weak<tokio::sync::Mutex<()>>>>,
    ordered_open_changed: Notify,
    retained_ordered_enables: Mutex<RetainedOrderedEnables>,
    ordered_enable_changed: Notify,
    ordered_enable_forget_pending: Mutex<HashSet<crate::OrderedActivation>>,
    retained_ordered_registers: Mutex<RetainedOrderedRegisters>,
    ordered_removal_pending: Mutex<HashMap<u64, crate::OrderedSourceGeneration>>,
    ordered_barriers: Mutex<HashMap<u64, Weak<tokio::sync::Mutex<()>>>>,
    ordered_pending_barriers:
        Mutex<HashMap<ubm_desktop::ordered_notification_groups::SourceKey, u64>>,
    next_ordered_barrier: AtomicU64,
    /// CDM UI survives a cancelled waiter until its own native terminal.
    /// Session ownership is independent of the operation's cancellation.
    companion_choices: Mutex<HashMap<RequestId, u64>>,
    companion_settled: Notify,
    late_completions: AtomicU64,
    mismatched_completions: AtomicU64,
    ingress: Mutex<Ingress>,
    notify: Notify,
    drops: [AtomicU64; 3],
    drop_hook: Mutex<Option<DropHook>>,
    connect_generation_hook: Mutex<Option<Weak<HostInner>>>,
    staged_scan: Mutex<Option<(RequestId, ScanRequest)>>,
    staged_connect: Mutex<HashMap<String, ConnectStaging>>,
    /// Per-peer connect sections (X-R3): at most one same-peer connect
    /// stages, dispatches and cleans up at a time, so staging stays bound
    /// to its operation without an operation identity on the boundary verb.
    /// One small entry per peer ever connected, like the host's other
    /// per-peer maps; entries are never removed because a removal racing a
    /// new waiter would split the section in two.
    connect_sections: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    staged_preference: Mutex<HashMap<Instance, DeliveryMode>>,
    recorded_disables: Mutex<RecordedDisables>,
    #[cfg(debug_assertions)]
    ingress_paused: std::sync::atomic::AtomicBool,
    close_failures: Mutex<Vec<RadioCloseFailure>>,
    close_error: Mutex<Option<DesktopError>>,
}

struct ConnectSecurityAdmission {
    host: Weak<HostInner>,
    peer_id: String,
    generation: String,
    owned: bool,
}

impl ConnectSecurityAdmission {
    fn disarm(&mut self) {
        self.owned = false;
    }
}

impl Drop for ConnectSecurityAdmission {
    fn drop(&mut self) {
        if self.owned
            && let Some(host) = self.host.upgrade()
        {
            host.clear_security_generation(&self.peer_id, &self.generation);
        }
    }
}

pub(crate) struct RecordedDisableGuard {
    shared: Arc<Shared>,
    instance: Instance,
    epoch: u64,
}
impl Drop for RecordedDisableGuard {
    fn drop(&mut self) {
        lock(&self.shared.recorded_disables).remove(&(self.instance.clone(), self.epoch));
    }
}

/// The central's radio boundary over the native adapter. Cloning shares
/// one radio: the central holds one clone, the host another for
/// completions and ingress.
#[derive(Clone)]
pub struct ForeignRadio {
    shared: Arc<Shared>,
    ordered_admission_owner: Option<crate::radio::OrderedAdmissionOwner>,
    ordered_admission_negotiation: Option<crate::radio::OrderedAdmissionNegotiation>,
    ordered_open_coordinator:
        Result<Option<Arc<MobileOpenCoordinator>>, crate::ordered_open_slots::Error>,
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
    /// Live entries of the per-peer connect-section table: one per peer
    /// ever connected, never removed, so a soak test can prove it is not
    /// growing from garbage peer ids.
    pub connect_sections: u64,
}

/// Outcome of handing one completion to the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    /// Delivered to its waiter.
    Delivered,
    /// No waiter: the request was cancelled/abandoned or never issued.
    /// Counted in `late_completions`.
    Late,
    /// Wrong answer shape for a known request: counted without settling its
    /// waiter or retained ownership; the native effect remains uncertain.
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
    pub(crate) fn invalidate_gatt_snapshot(&self, peer_id: &str) {
        lock(&self.shared.gatt_identities).remove(peer_id);
    }
    pub(crate) fn invalidate_all_gatt_snapshots(&self) {
        lock(&self.shared.gatt_identities).clear();
    }
    #[must_use]
    pub fn connection_subrate_available(&self) -> bool {
        self.shared.mobile_platform == MobilePlatform::Android
            && self.shared.platform.connection_subrate_available()
    }
    #[must_use]
    pub fn new(
        platform: Arc<dyn PlatformRadio>,
        mobile_platform: MobilePlatform,
        adapter_label: String,
    ) -> Self {
        let negotiation = platform.ordered_admission_negotiation();
        let incarnation = platform.ordered_admission_owner();
        let shared = Arc::new(Shared {
            platform,
            mobile_platform,
            #[cfg(any(test, all(feature = "synthetic-marker-harness", debug_assertions)))]
            marker_supported_for_test: std::sync::atomic::AtomicBool::new(false),
            adapter_label,
            gatt_identities: Mutex::new(HashMap::new()),
            reclaim_authority_opens: Mutex::new(HashMap::new()),
            reclaim_permit_opens: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            retained_v2_attempts: Mutex::new(HashMap::new()),
            retained_owner_attempts: Mutex::new(HashMap::new()),
            owner_settlement_changed: Notify::new(),
            ordered_recovery: Mutex::new(HashMap::new()),
            ordered_cleanup_due: Mutex::new(HashMap::new()),
            retained_ordered_opens: Mutex::new(HashMap::new()),
            ordered_open_submission: Mutex::new(HashMap::new()),
            ordered_open_lifecycle: Mutex::new(HashMap::new()),
            ordered_open_changed: Notify::new(),
            retained_ordered_enables: Mutex::new(RetainedOrderedEnables::default()),
            ordered_enable_changed: Notify::new(),
            ordered_enable_forget_pending: Mutex::new(HashSet::new()),
            retained_ordered_registers: Mutex::new(RetainedOrderedRegisters::default()),
            ordered_removal_pending: Mutex::new(HashMap::new()),
            ordered_barriers: Mutex::new(HashMap::new()),
            ordered_pending_barriers: Mutex::new(HashMap::new()),
            next_ordered_barrier: AtomicU64::new(1),
            companion_choices: Mutex::new(HashMap::new()),
            companion_settled: Notify::new(),
            late_completions: AtomicU64::new(0),
            mismatched_completions: AtomicU64::new(0),
            ingress: Mutex::new(Ingress::default()),
            notify: Notify::new(),
            drops: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            drop_hook: Mutex::new(None),
            connect_generation_hook: Mutex::new(None),
            staged_scan: Mutex::new(None),
            staged_connect: Mutex::new(HashMap::new()),
            connect_sections: Mutex::new(HashMap::new()),
            staged_preference: Mutex::new(HashMap::new()),
            recorded_disables: Mutex::new(HashMap::new()),
            #[cfg(debug_assertions)]
            ingress_paused: std::sync::atomic::AtomicBool::new(false),
            close_failures: Mutex::new(Vec::new()),
            close_error: Mutex::new(None),
        });
        let ordered_open_coordinator = incarnation
            .map(|owner| {
                mobile_open_registry().attach_or_register(
                    owner.bytes(),
                    ORDERED_OPEN_ADMISSION_SLOTS,
                    shared.clone(),
                )
            })
            .transpose();
        Self {
            shared,
            ordered_admission_owner: incarnation,
            ordered_admission_negotiation: negotiation,
            ordered_open_coordinator,
        }
    }

    pub(crate) fn ordered_admission_owner(&self) -> Option<crate::radio::OrderedAdmissionOwner> {
        self.ordered_admission_owner
    }

    pub(crate) fn uses_v2_admission(&self) -> bool {
        self.ordered_admission_negotiation
            .is_some_and(|negotiation| negotiation.version == crate::ORDERED_ADMISSION_SLOT_VERSION)
    }

    pub(crate) fn owner_retirement_supported(&self) -> bool {
        self.shared.platform.ordered_owner_retirement_version()
            == crate::ORDERED_OWNER_RETIREMENT_VERSION
            && self
                .shared
                .platform
                .ordered_owner_retirement_owner()
                .is_some()
    }

    pub(crate) fn ordered_owner_retirement_owner(
        &self,
    ) -> Option<crate::radio::OrderedAdmissionOwner> {
        self.shared.platform.ordered_owner_retirement_owner()
    }

    pub(crate) fn begin_exclusive_owner_retirement(
        &self,
        host: Arc<HostInner>,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            let radio: Arc<dyn std::any::Any + Send + Sync> = self.shared.clone();
            let host: Arc<dyn std::any::Any + Send + Sync> = host;
            mobile_open_registry().begin_exclusive_retirement(&owner, &radio, &host)?;
        }
        Ok(())
    }

    /// Host-owned slot API. Legacy wire operations cannot use these credits.
    pub(crate) fn ordered_open_coordinator(
        &self,
    ) -> Result<Option<Arc<MobileOpenCoordinator>>, crate::ordered_open_slots::Error> {
        self.ordered_open_coordinator.clone()
    }

    pub(crate) fn retain_ordered_host(
        &self,
        host: Arc<HostInner>,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            let radio: Arc<dyn std::any::Any + Send + Sync> = self.shared.clone();
            mobile_open_registry().add_host_anchor(&owner, &radio, host)?;
        }
        Ok(())
    }

    /// Called only after this exact host's cleanup is positively settled and
    /// no callback or worker can create further obligations. A shutdown
    /// request or an empty instantaneous snapshot is not that proof.
    pub(crate) fn confirm_ordered_host_cleanup(
        &self,
        host: Arc<HostInner>,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            self.confirm_ordered_radio_cleanup()?;
            let anchor: Arc<dyn std::any::Any + Send + Sync> = host;
            mobile_open_registry().confirm_cleanup(&owner, &anchor)?;
        }
        Ok(())
    }

    /// Requires positive settlement for this exact radio and its workers.
    pub(crate) fn confirm_ordered_radio_cleanup(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            let anchor: Arc<dyn std::any::Any + Send + Sync> = self.shared.clone();
            mobile_open_registry().confirm_cleanup(&owner, &anchor)?;
        }
        Ok(())
    }

    /// Only the host constructor may call this, before publication and while
    /// the constructor guarantees it has issued no radio requests.
    pub(crate) fn confirm_unpublished_ordered_radio(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            let anchor: Arc<dyn std::any::Any + Send + Sync> = self.shared.clone();
            mobile_open_registry().confirm_unpublished(&owner, &anchor)?;
        }
        Ok(())
    }

    #[allow(
        dead_code,
        reason = "the versioned native ordered-owner bridge calls this on supported mobile hosts"
    )]
    pub(crate) fn begin_ordered_owner_retirement(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            mobile_open_registry().begin_retirement(&owner)?;
        }
        Ok(())
    }

    /// The caller must have received positive native quiescence for this exact
    /// incarnation. Host shutdown, cancellation and elapsed time do not qualify.
    pub(crate) fn retire_ordered_owner_after_native_quiescence(
        &self,
    ) -> Result<(), crate::ordered_open_slots::Error> {
        if let Some(owner) = self.ordered_open_coordinator()? {
            mobile_open_registry().retire_after_native_quiescence(&owner)?;
        }
        Ok(())
    }

    /// Install the callback that learns about every ingress drop.
    pub fn set_drop_hook(&self, hook: DropHook) {
        *lock(&self.shared.drop_hook) = Some(hook);
    }

    #[cfg(any(test, all(feature = "synthetic-marker-harness", debug_assertions)))]
    pub(crate) fn enable_marker_for_test(&self) {
        self.shared
            .marker_supported_for_test
            .store(true, Ordering::SeqCst);
    }

    #[cfg(all(feature = "synthetic-marker-harness", debug_assertions))]
    pub(crate) fn synthetic_marker_enabled(&self) -> bool {
        self.shared.marker_supported_for_test.load(Ordering::SeqCst)
    }

    pub(crate) fn ordered_reclaim_graph_supported(&self) -> bool {
        #[cfg(any(test, all(feature = "synthetic-marker-harness", debug_assertions)))]
        {
            self.shared.marker_supported_for_test.load(Ordering::SeqCst)
        }
        #[cfg(not(any(test, all(feature = "synthetic-marker-harness", debug_assertions))))]
        {
            false
        }
    }

    pub(crate) fn set_connect_generation_hook(&self, host: Weak<HostInner>) {
        *lock(&self.shared.connect_generation_hook) = Some(host);
    }

    /// Issue one request and wait for its answer. The request id is minted
    /// here; the future is cancel-safe (drop = platform cancel).
    pub async fn call(
        &self,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        self.call_with_owner(None, false, false, make).await
    }

    async fn call_ordered_enable(
        &self,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        self.call_with_owner(None, true, false, make).await
    }

    async fn call_ordered_register(
        &self,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        self.call_with_owner(None, false, true, make).await
    }

    /// A host can recover an open even if its waiter was cancelled after dispatch.
    #[must_use]
    pub fn retained_ordered_open(&self, group_id: u64) -> Option<RetainedOrderedOpen> {
        lock(&self.shared.retained_ordered_opens)
            .get(&group_id)
            .cloned()
    }

    /// Register a notification before inspecting the retained result to avoid lost wakes.
    #[must_use]
    pub fn ordered_open_changed(&self) -> &Notify {
        &self.shared.ordered_open_changed
    }

    /// Release only once the host has adopted the result or completed its cleanup.
    pub fn release_retained_ordered_open(&self, group_id: u64) -> bool {
        let mut retained = lock(&self.shared.retained_ordered_opens);
        if retained
            .get(&group_id)
            .is_some_and(|entry| entry.acknowledged)
        {
            if let Some(entry) = retained.remove(&group_id)
                && !lock(&self.shared.ordered_cleanup_due).contains_key(&group_id)
            {
                lock(&self.shared.ordered_recovery).remove(&entry.request_id);
            }
            true
        } else {
            false
        }
    }

    pub(crate) fn abandoned_v2_opens(&self) -> Vec<(u64, String)> {
        lock(&self.shared.ordered_cleanup_due)
            .iter()
            .map(|(group, (peer, _))| (*group, peer.clone()))
            .collect()
    }

    pub(crate) fn complete_v2_central_cleanup(&self, group_id: u64) {
        if let Some((_, request_id)) = lock(&self.shared.ordered_cleanup_due).remove(&group_id) {
            lock(&self.shared.ordered_recovery).remove(&request_id);
        }
    }

    pub(crate) fn record_v2_recovery_error(&self, group_id: u64, error: DesktopError) {
        let request = self
            .retained_ordered_open(group_id)
            .map(|entry| entry.request_id)
            .or_else(|| {
                lock(&self.shared.ordered_cleanup_due)
                    .get(&group_id)
                    .map(|(_, request)| *request)
            });
        if let Some(request) = request
            && let Some(current) = lock(&self.shared.ordered_recovery).get_mut(&request)
            && !current.contains(&error)
        {
            current.push(error);
        }
    }

    pub(crate) fn v2_recovery_errors(&self) -> Vec<DesktopError> {
        lock(&self.shared.ordered_recovery)
            .values()
            .flat_map(|errors| errors.iter().cloned())
            .collect()
    }

    fn forget_v2_attempts(&self, original_id: RequestId) {
        lock(&self.shared.retained_v2_attempts).retain(|_, attempt| match &attempt.request {
            RadioRequest::OrderedOpenV2(
                crate::OrderedOpenRequestV2::Open { identity, .. }
                | crate::OrderedOpenRequestV2::Recover { identity, .. }
                | crate::OrderedOpenRequestV2::Acknowledge { identity, .. },
            ) => identity.original_request_id != original_id,
            _ => true,
        });
    }

    #[cfg(test)]
    pub(crate) fn retained_v2_attempt_count(&self) -> usize {
        lock(&self.shared.retained_v2_attempts).len()
    }

    // Barrier, close, seal, and release serialize per native group. A hung
    // platform reply for one group cannot retain the lock for another group.
    pub(super) async fn lock_ordered_barrier(
        &self,
        group_id: u64,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = {
            let mut gates = lock(&self.shared.ordered_barriers);
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&group_id).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(tokio::sync::Mutex::new(()));
                gates.insert(group_id, Arc::downgrade(&gate));
                gate
            }
        };
        gate.lock_owned().await
    }

    pub(super) async fn lock_ordered_open_lifecycle(
        &self,
        group_id: u64,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = {
            let mut gates = lock(&self.shared.ordered_open_lifecycle);
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&group_id).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(tokio::sync::Mutex::new(()));
                gates.insert(group_id, Arc::downgrade(&gate));
                gate
            }
        };
        gate.lock_owned().await
    }

    /// Retire only a terminal admission receipt after its disposition is retained.
    pub(super) async fn acknowledge_ordered_open_locked(
        &self,
        peer: &str,
        group_id: u64,
    ) -> Result<(), DesktopError> {
        let entry = self.retained_ordered_open(group_id).ok_or_else(|| {
            malformed(
                RequestKind::OrderedAcknowledgeOpen,
                "retained open is unavailable",
            )
        })?;
        if entry.peer_id != peer {
            return Err(malformed(
                RequestKind::OrderedAcknowledgeOpen,
                "retained open peer mismatch",
            ));
        }
        if entry.acknowledged {
            return Ok(());
        }
        if !matches!(
            entry.completion,
            Some(
                RadioCompletion::OrderedOpened { .. }
                    | RadioCompletion::OrderedOpenFailed {
                        ownership: crate::OrderedOpenOwnership::NotCreated
                            | crate::OrderedOpenOwnership::Created { .. },
                        ..
                    }
            )
        ) {
            return Err(malformed(
                RequestKind::OrderedAcknowledgeOpen,
                "open disposition is not terminal",
            ));
        }
        Self::retained_entry_lease(entry.clone(), peer, group_id)?;
        if self.uses_v2_admission() {
            let mapping = self
                .ordered_host()?
                .ordered_group(group_id)
                .ok_or_else(|| {
                    malformed(
                        RequestKind::OrderedAcknowledgeOpenV2,
                        "v2 group mapping unavailable",
                    )
                })?;
            let coordinator = self
                .ordered_open_coordinator()
                .ok()
                .flatten()
                .ok_or_else(|| {
                    malformed(
                        RequestKind::OrderedAcknowledgeOpenV2,
                        "v2 coordinator unavailable",
                    )
                })?;
            let active = coordinator
                .outstanding()
                .into_iter()
                .find(|active| {
                    active.lease.request.local_group == Some(mapping)
                        && active.lease.request.request_id == entry.request_id
                })
                .ok_or_else(|| {
                    malformed(RequestKind::OrderedAcknowledgeOpenV2, "v2 slot unavailable")
                })?;
            if active.disposition.is_none() {
                return Err(malformed(
                    RequestKind::OrderedAcknowledgeOpenV2,
                    "v2 disposition unavailable",
                ));
            }
            let owner = self.ordered_admission_owner.ok_or_else(|| {
                malformed(
                    RequestKind::OrderedAcknowledgeOpenV2,
                    "v2 owner unavailable",
                )
            })?;
            let identity = active.lease.wire_identity(owner).map_err(|_| {
                malformed(
                    RequestKind::OrderedAcknowledgeOpenV2,
                    "v2 identity mismatch",
                )
            })?;
            let late = lock(&self.shared.retained_v2_attempts)
                .values()
                .find_map(|attempt| match &attempt.request {
                    RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge {
                        identity: previous,
                        ..
                    }) if previous == &identity => attempt
                        .completions
                        .iter()
                        .find(|answer| {
                            matches!(
                                answer,
                                RadioCompletion::OrderedOpenV2(
                                    crate::OrderedOpenCompletionV2::Retired(_)
                                )
                            )
                        })
                        .cloned(),
                    _ => None,
                });
            let answer = if let Some(answer) = late {
                answer
            } else {
                tokio::time::timeout(
                    ORDERED_OPEN_RECEIPT_TIMEOUT,
                    self.call(|id| {
                        RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge {
                            id,
                            identity,
                        })
                    }),
                )
                .await
                .map_err(|_| {
                    DesktopError::new(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Gatt,
                        "gatt.ordered.acknowledge-open",
                    )
                    .with_detail("v2 retirement reply timed out; slot remains retained")
                })??
            };
            let RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Retired(receipt)) =
                answer
            else {
                return Err(malformed(
                    RequestKind::OrderedAcknowledgeOpenV2,
                    "v2 retirement was not confirmed",
                ));
            };
            coordinator
                .confirm_retired(&active.lease, receipt)
                .map_err(|_| {
                    malformed(
                        RequestKind::OrderedAcknowledgeOpenV2,
                        "v2 retirement identity mismatch",
                    )
                })?;
            let mut retained = lock(&self.shared.retained_ordered_opens);
            let current = retained.get_mut(&group_id).ok_or_else(|| {
                malformed(
                    RequestKind::OrderedAcknowledgeOpenV2,
                    "retained open disappeared",
                )
            })?;
            if current.request_id != entry.request_id || current.completion != entry.completion {
                return Err(malformed(
                    RequestKind::OrderedAcknowledgeOpenV2,
                    "retained open changed",
                ));
            }
            current.acknowledged = true;
            return Ok(());
        }
        tokio::time::timeout(
            ORDERED_OPEN_RECEIPT_TIMEOUT,
            self.unit(|id| RadioRequest::OrderedAcknowledgeOpen {
                id,
                version: crate::ORDERED_ACKNOWLEDGE_OPEN_VERSION,
                group_id,
                open_request_id: entry.request_id,
                peer_id: entry.peer_id.clone(),
            }),
        )
        .await
        .map_err(|_| {
            DesktopError::new(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Gatt,
                "gatt.ordered.acknowledge-open",
            )
            .with_detail("open acknowledgement reply timed out; receipt remains retained")
        })??;
        let mut retained = lock(&self.shared.retained_ordered_opens);
        let current = retained.get_mut(&group_id).ok_or_else(|| {
            malformed(
                RequestKind::OrderedAcknowledgeOpen,
                "retained open disappeared",
            )
        })?;
        if current.request_id != entry.request_id
            || current.peer_id != entry.peer_id
            || current.completion != entry.completion
        {
            return Err(malformed(
                RequestKind::OrderedAcknowledgeOpen,
                "retained open changed",
            ));
        }
        current.acknowledged = true;
        Ok(())
    }

    pub(crate) async fn call_owned_companion(
        &self,
        session_id: u64,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        self.call_with_owner(Some(session_id), false, false, make)
            .await
    }

    async fn call_with_owner(
        &self,
        owner: Option<u64>,
        retain_ordered_enable: bool,
        retain_ordered_register: bool,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        let id = Self::next_request_id()?;
        self.call_with_id(
            id,
            owner,
            retain_ordered_enable,
            retain_ordered_register,
            make,
        )
        .await
    }

    pub(crate) fn next_request_id() -> Result<RequestId, DesktopError> {
        NEXT_RADIO_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "radio.request",
                )
                .with_detail("radio request identifiers exhausted")
            })
    }

    async fn call_with_id(
        &self,
        id: RequestId,
        owner: Option<u64>,
        retain_ordered_enable: bool,
        retain_ordered_register: bool,
        make: impl FnOnce(RequestId) -> RadioRequest,
    ) -> Result<RadioCompletion, DesktopError> {
        let shared = &*self.shared;
        let request = make(id);
        let kind = request.kind();
        // No v1 admission operation carries the slot identity needed by a v2
        // owner, including recovery and retirement of a prior open.
        if matches!(
            &request,
            RadioRequest::OrderedOpen { .. }
                | RadioRequest::OrderedRecoverOpen { .. }
                | RadioRequest::OrderedAcknowledgeOpen { .. }
        ) && !self
            .ordered_admission_negotiation
            .is_some_and(|negotiated| {
                negotiated.version == crate::radio::ORDERED_ADMISSION_OWNER_VERSION
                    && Some(negotiated.owner) == self.ordered_admission_owner
            })
        {
            return Err(DesktopError::new(
                BleErrorCode::ProtocolIncompatible,
                BleErrorDomain::Boundary,
                "gatt.ordered-source.open",
            )
            .with_detail("legacy ordered admission requires a negotiated v1 owner"));
        }
        if let RadioRequest::OrderedOpenV2(operation) = &request {
            let (identity, attempt_matches) = match operation {
                crate::OrderedOpenRequestV2::Open { id, identity } => {
                    (identity, *id == identity.original_request_id)
                }
                crate::OrderedOpenRequestV2::Recover { id, identity }
                | crate::OrderedOpenRequestV2::Acknowledge { id, identity } => {
                    (identity, *id != identity.original_request_id)
                }
            };
            if !self
                .ordered_admission_negotiation
                .is_some_and(|negotiated| {
                    negotiated.version == crate::radio::ORDERED_ADMISSION_SLOT_VERSION
                        && negotiated.owner == identity.owner_incarnation
                        && Some(negotiated.owner) == self.ordered_admission_owner
                })
            {
                return Err(DesktopError::new(
                    BleErrorCode::ProtocolIncompatible,
                    BleErrorDomain::Boundary,
                    "gatt.ordered-source.open",
                )
                .with_detail("v2 ordered admission owner does not match negotiation"));
            }
            if request.id() != id || !attempt_matches || identity.validate().is_err() {
                return Err(DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Boundary,
                    "gatt.ordered-source.open",
                )
                .with_detail("invalid v2 ordered admission identity or attempt"));
            }
            let admitted =
                self.ordered_open_coordinator()
                    .ok()
                    .flatten()
                    .is_some_and(|coordinator| {
                        coordinator.outstanding().iter().any(|active| {
                            active.dispatched
                                && active
                                    .lease
                                    .wire_identity(identity.owner_incarnation)
                                    .ok()
                                    .as_ref()
                                    == Some(identity)
                                && (!matches!(
                                    operation,
                                    crate::OrderedOpenRequestV2::Acknowledge { .. }
                                ) || active.disposition.is_some())
                        })
                    });
            if !admitted {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered-source.open",
                )
                .with_detail("v2 operation has no exact dispatched coordinator lease"));
            }
        }
        let open_group = match &request {
            RadioRequest::OrderedOpen { group_id, .. } => Some(*group_id),
            RadioRequest::OrderedRecoverOpen { group_id, .. }
            | RadioRequest::OrderedAcknowledgeOpen { group_id, .. } => Some(*group_id),
            _ => None,
        };
        let submission_gate = open_group.map(|group| {
            let mut gates = lock(&shared.ordered_open_submission);
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&group).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(tokio::sync::Mutex::new(()));
                gates.insert(group, Arc::downgrade(&gate));
                gate
            }
        });
        let open_submission = match submission_gate {
            Some(gate) => Some(gate.lock_owned().await),
            None => None,
        };
        if retain_ordered_register {
            let RadioRequest::OrderedRegister {
                generation,
                group_id,
                member,
                ..
            } = &request
            else {
                return Err(malformed(
                    kind,
                    "retained ordered register requires a register request",
                ));
            };
            let mut retained = lock(&shared.retained_ordered_registers);
            let key = OrderedRegisterKey {
                generation: generation.clone(),
                group_id: *group_id,
                old: *member,
            };
            if retained.by_key.keys().any(|existing| {
                existing.generation == key.generation
                    && existing.group_id == key.group_id
                    && existing != &key
            }) {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.register",
                )
                .with_detail("another member registration remains unresolved"));
            }
            if retained.by_key.contains_key(&key) {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.register",
                )
                .with_detail("member registration is already retained"));
            }
            if u64::try_from(retained.by_request.len()).map_or(true, |count| {
                count >= ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
            }) {
                return Err(DesktopError::new(
                    BleErrorCode::StreamQuota,
                    BleErrorDomain::Stream,
                    "gatt.ordered.register",
                )
                .with_detail("member registration retention capacity exhausted"));
            }
            retained.by_key.insert(key.clone(), id);
            retained.by_request.insert(
                id,
                RetainedOrderedRegister {
                    key,
                    completion: None,
                },
            );
        }
        if retain_ordered_enable {
            let RadioRequest::OrderedEnableNotifications { activation, .. } = &request else {
                return Err(malformed(
                    kind,
                    "retained ordered enable requires an enable request",
                ));
            };
            let mut retained = lock(&shared.retained_ordered_enables);
            let key = activation.clone();
            if !retained.reserved.contains(&key) || retained.by_activation.contains_key(&key) {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.enable",
                )
                .with_detail("physical activation is already retained"));
            }
            if u64::try_from(retained.by_request.len()).map_or(true, |count| {
                count >= ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
            }) {
                return Err(DesktopError::new(
                    BleErrorCode::StreamQuota,
                    BleErrorDomain::Stream,
                    "gatt.ordered.enable",
                )
                .with_detail("physical activation retention capacity exhausted"));
            }
            retained.reserved.remove(&key);
            retained.by_activation.insert(key, id);
            retained
                .by_request
                .insert(id, RetainedOrderedEnable { completion: None });
        }
        if let RadioRequest::OrderedOpen {
            group_id,
            connection_generation,
            database_generation,
            snapshot_identity,
            members,
            item_capacity,
            byte_capacity,
            ..
        } = &request
        {
            let mut member_ids = HashSet::new();
            let mut instances = HashSet::new();
            let valid = !connection_generation.is_empty()
                && !database_generation.is_empty()
                && !snapshot_identity.owner.is_empty()
                && !members.is_empty()
                && u64::try_from(members.len())
                    .is_ok_and(|count| count <= ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY)
                && *item_capacity > 0
                && u64::from(*item_capacity) <= ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY
                && *byte_capacity > 0
                && u64::from(*byte_capacity) <= ubm_core::contracts::MAX_STREAM_BYTE_CAPACITY
                && members.iter().all(|member| {
                    member_ids.insert(member.member_id)
                        && instances.insert(&member.instance)
                        && member.instance.peer_id == members[0].instance.peer_id
                });
            if !valid {
                return Err(DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.ordered-source.open",
                )
                .with_detail("invalid ordered members or capacity"));
            }
            let mut retained = lock(&shared.retained_ordered_opens);
            if retained.contains_key(group_id) {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Gatt,
                    "gatt.ordered-source.open",
                )
                .with_detail("group open is already owned"));
            }
            if retained.len() >= RETAINED_ORDERED_OPEN_CAP {
                return Err(DesktopError::new(
                    BleErrorCode::StreamQuota,
                    BleErrorDomain::Stream,
                    "gatt.ordered-source.open",
                )
                .with_detail("retained open capacity is exhausted"));
            }
            retained.insert(
                *group_id,
                RetainedOrderedOpen {
                    acknowledged: false,
                    request_id: id,
                    peer_id: members[0].instance.peer_id.clone(),
                    connection_generation: connection_generation.clone(),
                    database_generation: database_generation.clone(),
                    members: members.clone(),
                    completion: None,
                },
            );
        }
        if let Some(session_id) = owner {
            debug_assert_eq!(kind, RequestKind::AssociateCompanion);
            lock(&shared.companion_choices).insert(id, session_id);
        }
        let (tx, rx) = oneshot::channel();
        let exact_request = matches!(
            &request,
            RadioRequest::OrderedOpenV2(_)
                | RadioRequest::OwnerFence { .. }
                | RadioRequest::OwnerStatus { .. }
        )
        .then(|| request.clone());
        {
            let mut pending = lock(&shared.pending);
            if matches!(&request, RadioRequest::OrderedOpenV2(_)) {
                let mut attempts = lock(&shared.retained_v2_attempts);
                let original = match &request {
                    RadioRequest::OrderedOpenV2(
                        crate::OrderedOpenRequestV2::Open { identity, .. }
                        | crate::OrderedOpenRequestV2::Recover { identity, .. }
                        | crate::OrderedOpenRequestV2::Acknowledge { identity, .. },
                    ) => identity.original_request_id,
                    _ => unreachable!(),
                };
                let previous = attempts.iter().find_map(|(previous_id, attempt)| {
                    (attempt.request.kind() == kind
                        && matches!(&attempt.request,
                            RadioRequest::OrderedOpenV2(
                                crate::OrderedOpenRequestV2::Open { identity, .. }
                                | crate::OrderedOpenRequestV2::Recover { identity, .. }
                                | crate::OrderedOpenRequestV2::Acknowledge { identity, .. }
                            ) if identity.original_request_id == original))
                    .then_some(*previous_id)
                });
                if let Some(previous_id) = previous {
                    if pending.contains_key(&previous_id) || kind == RequestKind::OrderedOpenV2 {
                        return Err(DesktopError::new(
                            BleErrorCode::LifecycleInvalidState,
                            BleErrorDomain::Cleanup,
                            kind.operation(),
                        )
                        .with_detail("v2 operation already has a live attempt"));
                    }
                    // Native retains the exact terminal disposition until retirement,
                    // and retirement is repeatable. Superseded callbacks are late;
                    // a fresh, globally unique ID avoids colliding with native work.
                    attempts.remove(&previous_id);
                }
                attempts.insert(
                    id,
                    RetainedV2Attempt {
                        request: request.clone(),
                        completions: Vec::new(),
                    },
                );
            }
            pending.insert(
                id,
                Pending {
                    kind,
                    exact_request,
                    tx,
                },
            );
            if matches!(kind, RequestKind::OwnerFence | RequestKind::OwnerStatus) {
                lock(&shared.retained_owner_attempts).insert(id, (request.clone(), None));
            }
        }
        let mut guard = PendingGuard {
            shared,
            id,
            armed: true,
        };
        if matches!(
            kind,
            RequestKind::Write
                | RequestKind::WriteDescriptor
                | RequestKind::RequestMtu
                | RequestKind::RequestConnectionPriority
                | RequestKind::RequestSubrate
                | RequestKind::RequestPhy
        ) {
            let _ = DISPATCHED.try_with(|flag| flag.set(true));
        }
        let _in_flight = InFlight::enter();
        shared.platform.submit(request);
        drop(open_submission);
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
        // A request id, not the callback variant, identifies the native owner.
        // Validate against either its live waiter or the retained owner of an
        // abandoned ordered operation before committing any answer. A wrong
        // callback cannot prove that a potentially accepted effect was refused.
        // Serialize competing completions for a live request so the waiter
        // and retained receipt cannot observe different winning answers.
        let mut pending_entries = lock(&self.shared.pending);
        let pending_kind = pending_entries.get(&request_id).map(|entry| entry.kind);
        let retained_owner = {
            let mut attempts = lock(&self.shared.retained_owner_attempts);
            if let Some((request, result)) = attempts.get_mut(&request_id) {
                if !completion.answers_request(request) {
                    self.shared
                        .mismatched_completions
                        .fetch_add(1, Ordering::Relaxed);
                    return CompletionStatus::Mismatched;
                }
                if let RadioCompletion::OwnerRetirement(receipt) = &completion {
                    if result.as_ref().is_some_and(|old| old != receipt) {
                        self.shared
                            .mismatched_completions
                            .fetch_add(1, Ordering::Relaxed);
                        return CompletionStatus::Mismatched;
                    }
                    *result = Some(receipt.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        let v2_kind = {
            let mut attempts = lock(&self.shared.retained_v2_attempts);
            if let Some(attempt) = attempts.get_mut(&request_id) {
                let terminal = |answer: &RadioCompletion| match (&attempt.request, answer) {
                    (
                        RadioRequest::OrderedOpenV2(
                            crate::OrderedOpenRequestV2::Open { identity, .. }
                            | crate::OrderedOpenRequestV2::Recover { identity, .. },
                        ),
                        RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(
                            result,
                        )),
                    ) => result.terminal_for(identity).is_some(),
                    (
                        _,
                        RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Retired(_)),
                    ) => true,
                    _ => false,
                };
                if !completion.answers_request(&attempt.request)
                    || !Self::valid_v2_open_payload(&attempt.request, &completion)
                    || (!attempt.completions.contains(&completion)
                        && ((!terminal(&completion) && !attempt.completions.is_empty())
                            || attempt.completions.iter().any(terminal)))
                {
                    self.shared
                        .mismatched_completions
                        .fetch_add(1, Ordering::Relaxed);
                    return CompletionStatus::Mismatched;
                }
                if let RadioRequest::OrderedOpenV2(
                    crate::OrderedOpenRequestV2::Open { identity, .. }
                    | crate::OrderedOpenRequestV2::Recover { identity, .. },
                ) = &attempt.request
                    && !self.retain_v2_open_completion(identity, &completion)
                {
                    self.shared
                        .mismatched_completions
                        .fetch_add(1, Ordering::Relaxed);
                    return CompletionStatus::Mismatched;
                }
                if !attempt.completions.contains(&completion) {
                    attempt.completions.push(completion.clone());
                }
                Some(attempt.request.kind())
            } else {
                None
            }
        };
        let retained_kind = if lock(&self.shared.retained_ordered_opens)
            .values()
            .any(|entry| entry.request_id == request_id)
        {
            Some(RequestKind::OrderedOpen)
        } else if lock(&self.shared.retained_ordered_enables)
            .by_request
            .contains_key(&request_id)
        {
            Some(RequestKind::OrderedEnableNotifications)
        } else if lock(&self.shared.retained_ordered_registers)
            .by_request
            .contains_key(&request_id)
        {
            Some(RequestKind::OrderedRegister)
        } else {
            None
        };
        if let Some(pending) = pending_entries.get(&request_id)
            && let Some(request) = &pending.exact_request
            && (!completion.answers_request(request)
                || !Self::valid_v2_open_payload(request, &completion))
        {
            self.shared
                .mismatched_completions
                .fetch_add(1, Ordering::Relaxed);
            return CompletionStatus::Mismatched;
        }
        if let Some(kind) = pending_kind.or(v2_kind).or(retained_kind)
            && !matches!(
                (kind, &completion),
                (
                    RequestKind::OwnerFence | RequestKind::OwnerStatus,
                    RadioCompletion::OwnerRetirement(_)
                )
            )
            && !completion.answers(kind)
        {
            self.shared
                .mismatched_completions
                .fetch_add(1, Ordering::Relaxed);
            return CompletionStatus::Mismatched;
        }
        {
            let mut retained = lock(&self.shared.retained_ordered_registers);
            if let Some(entry) = retained.by_request.get_mut(&request_id)
                && entry.completion.is_none()
            {
                entry.completion = Some(completion.clone());
            }
        }
        let retained_enable = {
            let mut retained = lock(&self.shared.retained_ordered_enables);
            if let Some(entry) = retained.by_request.get_mut(&request_id) {
                if entry.completion.is_none() {
                    entry.completion = Some(completion.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if retained_enable {
            self.shared.ordered_enable_changed.notify_waiters();
        }
        let retained_open = {
            let mut retained = lock(&self.shared.retained_ordered_opens);
            if let Some((group_id, entry)) = retained
                .iter_mut()
                .find(|(_, entry)| v2_kind.is_none() && entry.request_id == request_id)
            {
                if matches!(
                    completion,
                    RadioCompletion::OrderedOpened { .. }
                        | RadioCompletion::OrderedOpenFailed {
                            ownership: crate::OrderedOpenOwnership::Created { .. },
                            ..
                        }
                ) {
                    let mut candidate = entry.clone();
                    candidate.completion = Some(completion.clone());
                    if Self::retained_entry_lease(candidate, &entry.peer_id, *group_id).is_err() {
                        self.shared
                            .mismatched_completions
                            .fetch_add(1, Ordering::Relaxed);
                        return CompletionStatus::Mismatched;
                    }
                }
                // An untyped failure only answered the original waiter; it
                // did not settle native ownership. Preserve a later exact
                // original answer, without replacing an already terminal
                // ownership decision made by recovery.
                let unresolved = matches!(
                    entry.completion,
                    None | Some(RadioCompletion::Failed(_))
                        | Some(RadioCompletion::OrderedOpenFailed {
                            ownership: crate::OrderedOpenOwnership::Indeterminate { .. },
                            ..
                        })
                );
                let exact = matches!(
                    completion,
                    RadioCompletion::OrderedOpened { .. }
                        | RadioCompletion::OrderedOpenFailed {
                            ownership: crate::OrderedOpenOwnership::Created { .. }
                                | crate::OrderedOpenOwnership::NotCreated,
                            ..
                        }
                );
                if !unresolved && exact && entry.completion.as_ref() != Some(&completion) {
                    self.shared
                        .mismatched_completions
                        .fetch_add(1, Ordering::Relaxed);
                    return CompletionStatus::Mismatched;
                }
                if entry.completion.is_none() || (unresolved && exact) {
                    entry.completion = Some(completion.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if retained_open {
            self.shared.ordered_open_changed.notify_waiters();
        }
        // A malformed completion cannot prove that the native picker closed.
        if completion.answers(RequestKind::AssociateCompanion)
            && lock(&self.shared.companion_choices)
                .remove(&request_id)
                .is_some()
        {
            self.shared.companion_settled.notify_waiters();
        }
        let entry = pending_entries.remove(&request_id);
        drop(pending_entries);
        let Some(pending) = entry else {
            if retained_owner {
                self.shared.owner_settlement_changed.notify_one();
                return CompletionStatus::Delivered;
            }
            self.shared.late_completions.fetch_add(1, Ordering::Relaxed);
            return CompletionStatus::Late;
        };
        if pending.tx.send(completion).is_err() {
            if retained_owner {
                self.shared.owner_settlement_changed.notify_one();
                return CompletionStatus::Delivered;
            }
            self.shared.late_completions.fetch_add(1, Ordering::Relaxed);
            return CompletionStatus::Late;
        }
        CompletionStatus::Delivered
    }

    pub(crate) fn take_owner_settlement(
        &self,
        owner: crate::OrderedAdmissionOwner,
        retirement: crate::OrderedAdmissionOwner,
    ) -> Option<crate::OwnerRetirementResult> {
        let mut attempts = lock(&self.shared.retained_owner_attempts);
        let id = attempts.iter().find_map(|(id, (_, result))| {
            result
                .as_ref()
                .filter(|receipt| {
                    receipt.owner_incarnation == owner && receipt.retirement_id == retirement
                })
                .map(|_| *id)
        })?;
        attempts.remove(&id).and_then(|(_, result)| result)
    }

    pub(crate) fn forget_owner_attempt(&self, id: RequestId) {
        lock(&self.shared.retained_owner_attempts).remove(&id);
    }

    pub(crate) async fn owner_settlement_changed(&self) {
        self.shared.owner_settlement_changed.notified().await;
    }

    /// Retry exact CDM cancellations, bounded by one drain interval. Native
    /// refusal or a held UI queue leaves the obligation owned for retry.
    pub(crate) async fn release_companion_choices(
        &self,
        session_id: u64,
    ) -> Result<(), DesktopError> {
        let ids: Vec<_> = lock(&self.shared.companion_choices)
            .iter()
            .filter_map(|(id, owner)| (*owner == session_id).then_some(*id))
            .collect();
        for id in ids {
            self.shared.platform.cancel(id);
        }
        let settled = async {
            loop {
                let notified = self.shared.companion_settled.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !lock(&self.shared.companion_choices)
                    .values()
                    .any(|owner| *owner == session_id)
                {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), settled)
            .await
            .map_err(|_| {
                DesktopError::new(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Cleanup,
                    "companion.associate.release",
                )
                .with_detail(
                    "native companion picker release remains unconfirmed; owner retained for retry",
                )
            })
    }

    /// Admit a retryable source failure without recording a drop. The queue
    /// lock linearizes admission with closure and with all other events.
    pub(crate) fn try_push_source_failure(&self, event: RadioEvent) -> SourceFailureQueueStatus {
        let mut ingress = lock(&self.shared.ingress);
        if ingress.closed {
            return SourceFailureQueueStatus::Closed;
        }
        if ingress.control.len() >= CONTROL_INGRESS_CAP {
            return SourceFailureQueueStatus::Full;
        }
        ingress.sequence += 1;
        let sequence = ingress.sequence;
        ingress.control.push_back((sequence, event));
        drop(ingress);
        self.shared.notify.notify_one();
        SourceFailureQueueStatus::Accepted
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
            RadioEvent::Advertisement(snapshot) => {
                let event = match ingress.scan_ingress_epoch {
                    Some(ingress_epoch) => RadioEvent::AdvertisementScoped {
                        snapshot,
                        ingress_epoch,
                    },
                    None => RadioEvent::Advertisement(snapshot),
                };
                if ingress.advertisements.len() >= ADVERTISEMENT_INGRESS_CAP {
                    Err(IngressClass::Advertisement)
                } else {
                    ingress.advertisements.push_back((sequence, event));
                    Ok(())
                }
            }
            RadioEvent::Notification { ref value, .. }
            | RadioEvent::NotificationWithReceipt { ref value, .. } => {
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
        self.note_drops(class, 1);
    }

    /// Account an exact batch without iterating over its members.
    pub fn note_drops(&self, class: IngressClass, count: u64) {
        if count == 0 {
            return;
        }
        self.shared.drops[class_index(class)].fetch_add(count, Ordering::Relaxed);
        let hook = lock(&self.shared.drop_hook).clone();
        if let Some(hook) = hook {
            hook(class, count);
        }
    }

    pub(crate) fn stage_recorded_disable(
        &self,
        instance: Instance,
        epoch: u64,
        fence: IngressFence,
    ) -> Result<RecordedDisableGuard, DesktopError> {
        let mut active = lock(&self.shared.recorded_disables);
        if active.contains_key(&(instance.clone(), epoch)) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "notification.disable-fence",
            ));
        }
        active.insert((instance.clone(), epoch), Arc::new(Mutex::new(Some(fence))));
        Ok(RecordedDisableGuard {
            shared: Arc::clone(&self.shared),
            instance,
            epoch,
        })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn ingress_paused(&self) -> bool {
        self.shared.ingress_paused.load(Ordering::SeqCst)
    }

    /// Pause the event lane after every earlier admitted event is processed.
    /// Dropping the returned fence resumes it, including cancellation paths.
    pub(crate) async fn fence_ingress(&self) -> Result<IngressFence, DesktopError> {
        let (arrived, receiver) = oneshot::channel();
        let (resume, released) = oneshot::channel();
        {
            let mut ingress = lock(&self.shared.ingress);
            if ingress.closed {
                return Err(DesktopError::new(
                    BleErrorCode::StreamClosed,
                    BleErrorDomain::Gatt,
                    "notification.ingress-fence",
                ));
            }
            if ingress.barriers.len() >= CONTROL_INGRESS_CAP {
                return Err(DesktopError::new(
                    BleErrorCode::StreamQuota,
                    BleErrorDomain::Gatt,
                    "notification.ingress-fence",
                ));
            }
            ingress.sequence += 1;
            let sequence = ingress.sequence;
            ingress.barriers.push_back((
                sequence,
                IngressBarrier {
                    arrived,
                    resume: released,
                },
            ));
        }
        self.shared.notify.notify_one();
        tokio::time::timeout(ubm_desktop::LIVENESS_OP, receiver)
            .await
            .map_err(|_| {
                DesktopError::new(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Gatt,
                    "notification.ingress-fence",
                )
            })?
            .map_err(|_| {
                DesktopError::new(
                    BleErrorCode::StreamClosed,
                    BleErrorDomain::Gatt,
                    "notification.ingress-fence",
                )
            })?;
        Ok(IngressFence(Some(resume)))
    }

    /// End the event source: the central's loop sees `None` after the
    /// queues drain.
    pub fn close_events(&self) {
        lock(&self.shared.ingress).closed = true;
        self.shared.notify.notify_one();
    }

    /// Stage the platform facts of the next scan start (addresses and
    /// Android settings the boundary verb cannot carry).
    pub(crate) fn stage_scan(&self, id: RequestId, scan: ScanRequest) {
        *lock(&self.shared.staged_scan) = Some((id, scan));
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
            connect_sections: lock(&shared.connect_sections).len() as u64,
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
/// denied, restricted, unavailable), or a concrete power-loss state. Unknown
/// power is not evidence of resetting, including initial central creation.
/// Answered as the
/// central's loss state, so the core's once-per-episode reset runs on
/// exactly those snapshots; `None` means no measured loss, not necessarily
/// readiness. Only measured powered-on ends the loss episode; unknown power
/// preserves it. The state is the loss's cause only: the `adapter` records carry
/// the platform's own snapshot.
#[must_use]
pub fn legacy_loss(snapshot: &AdapterSnapshot) -> Option<AdapterPowerState> {
    match snapshot.power {
        AdapterPower::Off => return Some(AdapterPowerState::PoweredOff),
        AdapterPower::Resetting => {
            return Some(AdapterPowerState::Resetting);
        }
        AdapterPower::Unsupported => return Some(AdapterPowerState::Unsupported),
        AdapterPower::On | AdapterPower::Unknown => {}
    }
    match snapshot.availability {
        AdapterAvailability::Available | AdapterAvailability::Unknown => {}
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
/// on for a usable adapter, unknown for unmeasured power, or the concrete
/// loss otherwise ([`legacy_loss`]).
#[must_use]
pub fn central_adapter_state(snapshot: &AdapterSnapshot) -> AdapterPowerState {
    legacy_loss(snapshot).unwrap_or_else(|| power_state(snapshot))
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

fn marker_status(
    status: crate::OrderedMarkerWriteStatus,
    platform: MobilePlatform,
) -> ubm_desktop::boundary::OrderedMarkerWriteStatus {
    use crate::{OrderedMarkerAdmission as A, OrderedMarkerWriteState as W};
    use ubm_desktop::boundary::{OrderedMarkerWritePhase as P, OrderedMarkerWriteStatus as S};
    let convert =
        |failure: PlatformFailure| failed(&failure, RequestKind::OrderedMarkerWrite, platform);
    match (status.admission, status.write) {
        (A::Pending, W::NotStarted { .. }) => S::Pending,
        (A::NotAdmitted, W::NotStarted { failure }) => S::NotAdmitted(failure.map(convert)),
        (A::Rejected { failure }, W::NotStarted { .. }) => S::Rejected(convert(failure)),
        (A::Admitted { ordinal }, write) => S::Admitted {
            ordinal,
            write: match write {
                W::Initiating => P::Initiating,
                W::NotStarted { failure } => P::NotStarted(failure.map(convert)),
                W::Started { failure } => P::Started(failure.map(convert)),
                W::Settled { failure } => {
                    P::Settled(failure.map_or(Ok(()), |failure| Err(convert(failure))))
                }
            },
        },
        _ => unreachable!("completion validation rejects impossible marker states"),
    }
}

impl RadioBoundary for ForeignRadio {
    async fn arm_scan_ingress(&self, ingress_epoch: u64) -> Result<(), DesktopError> {
        let mut ingress = lock(&self.shared.ingress);
        if ingress.closed || ingress.scan_ingress_epoch.is_some() {
            return Err(DesktopError::scan_start_failed(
                "mobile scan ingress is unavailable",
            ));
        }
        ingress.scan_ingress_epoch = Some(ingress_epoch);
        Ok(())
    }

    async fn retire_scan_ingress(&self, ingress_epoch: u64) -> Result<(), DesktopError> {
        let mut ingress = lock(&self.shared.ingress);
        if ingress.scan_ingress_epoch != Some(ingress_epoch) {
            return Err(DesktopError::scan_stop_failed(
                "mobile scan ingress epoch is unavailable",
            ));
        }
        ingress.scan_ingress_epoch = None;
        Ok(())
    }

    async fn fence_scan_ingress(&self, _ingress_epoch: u64) -> Result<(), DesktopError> {
        {
            let ingress = lock(&self.shared.ingress);
            if ingress.closed {
                if ingress.advertisements.is_empty()
                    && ingress.data.is_empty()
                    && ingress.control.is_empty()
                    && ingress.barriers.is_empty()
                {
                    return Ok(());
                }
                return Err(DesktopError::scan_stop_failed(
                    "mobile scan ingress closed before its accepted prefix drained",
                ));
            }
        }
        // `retire_scan_ingress` closed callback admission under this same
        // mutex first. This marker therefore follows every accepted old-epoch
        // advertisement; dropping it resumes the consumer after observation.
        drop(self.fence_ingress().await?);
        Ok(())
    }

    async fn open_ordered_reclaim_authority(&self, request: u64) -> Result<u64, DesktopError> {
        let mut receiver = {
            let mut entries = lock(&self.shared.reclaim_authority_opens);
            if let Some(existing) = entries.get(&request) {
                existing.result.clone()
            } else {
                if entries.len() >= 64 {
                    return Err(DesktopError::new(
                        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                        ubm_core::contracts::BleErrorDomain::Cleanup,
                        "gatt.ordered.authority-open",
                    )
                    .with_detail("authority open capacity exhausted")
                    .with_outcome(
                        Some(ubm_core::contracts::CommitState::NotDispatched),
                        ubm_desktop::Retryability::CallerDecides,
                    ));
                }
                let (sender, receiver) = tokio::sync::watch::channel(None);
                let receipt_id = Arc::new(Mutex::new(None));
                entries.insert(
                    request,
                    AuthorityOpenReceipt {
                        result: receiver.clone(),
                        receipt_id: Arc::clone(&receipt_id),
                    },
                );
                let radio = self.clone();
                tokio::spawn(async move {
                    let initial = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        radio.call(|id| RadioRequest::OrderedGraphAuthorityReserve { id, request }),
                    )
                    .await;
                    let receipt = match initial {
                        Ok(Ok(RadioCompletion::OrderedGraphAuthorityReserved(id))) => id,
                        Ok(Ok(RadioCompletion::OrderedGraphReserveStatus(
                            crate::OrderedGraphReserveStatus::NotCreated,
                        ))) => {
                            let _ = sender.send(Some(Err(DesktopError::new(
                                ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                                ubm_core::contracts::BleErrorDomain::Cleanup,
                                "gatt.ordered.authority-reserve",
                            )
                            .with_outcome(
                                Some(ubm_core::contracts::CommitState::NotDispatched),
                                ubm_desktop::Retryability::CallerDecides,
                            ))));
                            return;
                        }
                        Ok(Err(error))
                            if error.commit()
                                == Some(ubm_core::contracts::CommitState::NotDispatched) =>
                        {
                            let _ = sender.send(Some(Err(error)));
                            return;
                        }
                        _ => {
                            let mut delay = std::time::Duration::from_millis(250);
                            loop {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    radio.call(|id| {
                                        RadioRequest::OrderedGraphAuthorityReserveFence {
                                            id,
                                            request,
                                        }
                                    }),
                                )
                                .await
                                {
                                    Ok(Ok(RadioCompletion::OrderedGraphReserveStatus(
                                        crate::OrderedGraphReserveStatus::Issued(id),
                                    ))) => break id,
                                    Ok(Ok(RadioCompletion::OrderedGraphReserveStatus(
                                        crate::OrderedGraphReserveStatus::NotCreated,
                                    ))) => {
                                        let error = DesktopError::new(ubm_core::contracts::BleErrorCode::LifecycleInvalidState, ubm_core::contracts::BleErrorDomain::Cleanup, "gatt.ordered.authority-reserve").with_outcome(Some(ubm_core::contracts::CommitState::NotDispatched), ubm_desktop::Retryability::CallerDecides);
                                        let _ = sender.send(Some(Err(error)));
                                        return;
                                    }
                                    _ => {
                                        tokio::time::sleep(delay).await;
                                        delay = (delay * 2).min(std::time::Duration::from_secs(30));
                                    }
                                }
                            }
                        }
                    };
                    *lock(&receipt_id) = Some(receipt);
                    let first = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        radio.call(|id| RadioRequest::OrderedGraphAuthorityOpen {
                            id,
                            request: receipt,
                        }),
                    )
                    .await;
                    let result = match first {
                        Ok(Ok(RadioCompletion::OrderedGraphAuthorityOpened(authority))) => {
                            Ok(authority)
                        }
                        Ok(Ok(RadioCompletion::OrderedGraphAuthorityStatus(
                            crate::OrderedGraphAuthorityStatus::NotCreated,
                        ))) => Err(DesktopError::new(
                            ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                            ubm_core::contracts::BleErrorDomain::Cleanup,
                            "gatt.ordered.authority-open",
                        )
                        .with_outcome(
                            Some(ubm_core::contracts::CommitState::NotDispatched),
                            ubm_desktop::Retryability::CallerDecides,
                        )),

                        Ok(Err(error))
                            if error.commit()
                                == Some(ubm_core::contracts::CommitState::NotDispatched) =>
                        {
                            Err(error)
                        }
                        _ => {
                            let mut delay = std::time::Duration::from_millis(250);
                            loop {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    radio.call(|id| RadioRequest::OrderedGraphAuthorityFenceOpen {
                                        id,
                                        request: receipt,
                                    }),
                                )
                                .await
                                {
                                    Ok(Ok(RadioCompletion::OrderedGraphAuthorityStatus(
                                        crate::OrderedGraphAuthorityStatus::Opened(authority),
                                    ))) => break Ok(authority),
                                    Ok(Ok(RadioCompletion::OrderedGraphAuthorityStatus(
                                        crate::OrderedGraphAuthorityStatus::Closed(authority),
                                    ))) => {
                                        // The native authority may still own exact permit receipts.
                                        // Retain this worker until the source confirms retirement.
                                        loop {
                                            let retired = tokio::time::timeout(
                                                std::time::Duration::from_secs(5),
                                                radio.unit(|id| {
                                                    RadioRequest::OrderedGraphAuthorityForget {
                                                        id,
                                                        authority_id: authority,
                                                    }
                                                }),
                                            )
                                            .await;
                                            if matches!(retired, Ok(Ok(()))) {
                                                break;
                                            }
                                            tokio::time::sleep(delay).await;
                                            delay =
                                                (delay * 2).min(std::time::Duration::from_secs(30));
                                        }
                                        break Err(DesktopError::new(ubm_core::contracts::BleErrorCode::LifecycleInvalidState, ubm_core::contracts::BleErrorDomain::Cleanup, "gatt.ordered.authority-open").with_detail("native authority already closed").with_outcome(Some(ubm_core::contracts::CommitState::NotDispatched), ubm_desktop::Retryability::CallerDecides));
                                    }
                                    Ok(Ok(RadioCompletion::OrderedGraphAuthorityStatus(
                                        crate::OrderedGraphAuthorityStatus::NotCreated,
                                    ))) => break Err(DesktopError::new(
                                        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                                        ubm_core::contracts::BleErrorDomain::Cleanup,
                                        "gatt.ordered.authority-open",
                                    )
                                    .with_detail("native authority open was not created")
                                    .with_outcome(
                                        Some(ubm_core::contracts::CommitState::NotDispatched),
                                        ubm_desktop::Retryability::CallerDecides,
                                    )),
                                    _ => {
                                        tokio::time::sleep(delay).await;
                                        delay = (delay * 2).min(std::time::Duration::from_secs(30));
                                    }
                                }
                            }
                        }
                    };
                    let _ = sender.send(Some(result));
                });
                receiver
            }
        };
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            receiver.changed().await.map_err(|_| {
                malformed(
                    RequestKind::OrderedGraphAuthorityOpen,
                    "authority worker terminated",
                )
            })?;
        }
    }
    async fn forget_ordered_reclaim_open(&self, request: u64) -> Result<(), DesktopError> {
        let entry = {
            lock(&self.shared.reclaim_authority_opens)
                .get(&request)
                .cloned()
        };
        let mut receiver = entry.as_ref().map(|entry| entry.result.clone());
        if let Some(ref mut receiver) = receiver {
            loop {
                if receiver.borrow().is_some() {
                    break;
                }
                receiver.changed().await.map_err(|_| {
                    malformed(
                        RequestKind::OrderedGraphAuthorityForgetOpen,
                        "authority worker terminated",
                    )
                })?;
            }
        }
        let receipt = entry.as_ref().and_then(|entry| *lock(&entry.receipt_id));
        match self
            .call(|id| RadioRequest::OrderedGraphAuthorityReserveFence { id, request })
            .await?
        {
            RadioCompletion::OrderedGraphReserveStatus(
                crate::OrderedGraphReserveStatus::NotCreated,
            ) if receipt.is_none() => {}
            RadioCompletion::OrderedGraphReserveStatus(
                crate::OrderedGraphReserveStatus::Issued(id),
            ) if receipt == Some(id) => {}
            _ => {
                return Err(malformed(
                    RequestKind::OrderedGraphAuthorityReserveFence,
                    "authority reservation fence unresolved or mismatched",
                ));
            }
        }
        if let Some(receipt) = receipt {
            // A reserved receipt may still own a queued open even when the
            // adapter refused dispatch. Fence that exact receipt before forget.
            match self
                .call(|id| RadioRequest::OrderedGraphAuthorityFenceOpen {
                    id,
                    request: receipt,
                })
                .await?
            {
                RadioCompletion::OrderedGraphAuthorityStatus(
                    crate::OrderedGraphAuthorityStatus::NotCreated
                    | crate::OrderedGraphAuthorityStatus::Closed(_),
                ) => {}
                _ => {
                    return Err(malformed(
                        RequestKind::OrderedGraphAuthorityFenceOpen,
                        "authority open receipt remains pending or open",
                    ));
                }
            }
            self.unit(|id| RadioRequest::OrderedGraphAuthorityForgetOpen {
                id,
                request: receipt,
            })
            .await?;
        }
        self.unit(|id| RadioRequest::OrderedGraphAuthorityReserveForget { id, request })
            .await?;
        lock(&self.shared.reclaim_authority_opens).remove(&request);
        Ok(())
    }
    async fn close_ordered_reclaim_authority(&self, authority_id: u64) -> Result<(), DesktopError> {
        self.unit(|id| RadioRequest::OrderedGraphAuthorityClose { id, authority_id })
            .await
    }
    async fn forget_ordered_reclaim_authority(
        &self,
        authority_id: u64,
    ) -> Result<(), DesktopError> {
        self.unit(|id| RadioRequest::OrderedGraphAuthorityForget { id, authority_id })
            .await?;
        // The central seals admission and joins its workers before this call.
        let request = {
            lock(&self.shared.reclaim_authority_opens)
                .iter()
                .find_map(|(request, entry)| {
                    matches!(&*entry.result.borrow(), Some(Ok(id)) if *id == authority_id)
                        .then_some(*request)
                })
        };
        if let Some(request) = request {
            self.forget_ordered_reclaim_open(request).await?;
        }
        Ok(())
    }

    async fn reserve_ordered_reclaim_permit(
        &self,
        key: &ubm_desktop::boundary::OrderedReclaimPermitRequest,
    ) -> Result<u64, DesktopError> {
        let mut receiver = {
            let mut entries = lock(&self.shared.reclaim_permit_opens);
            let identity = (key.authority_id, key.request_id);
            if let Some(existing) = entries.get(&identity) {
                if existing.key != *key {
                    return Err(malformed(
                        RequestKind::OrderedGraphPermitReserve,
                        "conflicting permit reservation identity",
                    ));
                }
                existing.result.clone()
            } else {
                if entries.len() >= 256 {
                    return Err(DesktopError::new(
                        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                        ubm_core::contracts::BleErrorDomain::Cleanup,
                        "gatt.ordered.permit-reserve",
                    )
                    .with_outcome(
                        Some(ubm_core::contracts::CommitState::NotDispatched),
                        ubm_desktop::Retryability::CallerDecides,
                    ));
                }
                let (sender, receiver) = tokio::sync::watch::channel(None);
                entries.insert(
                    identity,
                    PermitOpenReceipt {
                        key: key.clone(),
                        result: receiver.clone(),
                    },
                );
                let radio = self.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    let initial = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        radio.call(|id| RadioRequest::OrderedGraphPermitReserve {
                            id,
                            key: key.clone(),
                        }),
                    )
                    .await;
                    let result = match initial {
                        Ok(Ok(RadioCompletion::OrderedGraphPermitReserved(id))) => Ok(id),
                        Ok(Ok(RadioCompletion::OrderedGraphReserveStatus(
                            crate::OrderedGraphReserveStatus::NotCreated,
                        ))) => Err(DesktopError::new(
                            ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                            ubm_core::contracts::BleErrorDomain::Cleanup,
                            "gatt.ordered.permit-reserve",
                        )
                        .with_outcome(
                            Some(ubm_core::contracts::CommitState::NotDispatched),
                            ubm_desktop::Retryability::CallerDecides,
                        )),
                        Ok(Err(error))
                            if error.commit()
                                == Some(ubm_core::contracts::CommitState::NotDispatched) =>
                        {
                            Err(error)
                        }
                        _ => {
                            let mut delay = std::time::Duration::from_millis(250);
                            loop {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    radio.call(|id| RadioRequest::OrderedGraphPermitReserveFence {
                                        id,
                                        key: key.clone(),
                                    }),
                                )
                                .await
                                {
                                    Ok(Ok(RadioCompletion::OrderedGraphReserveStatus(
                                        crate::OrderedGraphReserveStatus::Issued(id),
                                    ))) => break Ok(id),
                                    Ok(Ok(RadioCompletion::OrderedGraphReserveStatus(
                                        crate::OrderedGraphReserveStatus::NotCreated,
                                    ))) => break Err(DesktopError::new(
                                        ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                                        ubm_core::contracts::BleErrorDomain::Cleanup,
                                        "gatt.ordered.permit-reserve",
                                    )
                                    .with_outcome(
                                        Some(ubm_core::contracts::CommitState::NotDispatched),
                                        ubm_desktop::Retryability::CallerDecides,
                                    )),
                                    _ => {
                                        tokio::time::sleep(delay).await;
                                        delay = (delay * 2).min(std::time::Duration::from_secs(30));
                                    }
                                }
                            }
                        }
                    };
                    let _ = sender.send(Some(result));
                });
                receiver
            }
        };
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            receiver.changed().await.map_err(|_| {
                malformed(
                    RequestKind::OrderedGraphPermitReserve,
                    "permit reservation worker terminated",
                )
            })?;
        }
    }
    async fn forget_ordered_reclaim_permit_request(
        &self,
        key: &ubm_desktop::boundary::OrderedReclaimPermitRequest,
    ) -> Result<(), DesktopError> {
        let entry = {
            lock(&self.shared.reclaim_permit_opens)
                .get(&(key.authority_id, key.request_id))
                .cloned()
        };
        if let Some(ref entry) = entry {
            if entry.key != *key {
                return Err(malformed(
                    RequestKind::OrderedGraphPermitForgetRequest,
                    "conflicting permit reservation identity",
                ));
            }
            let mut receiver = entry.result.clone();
            loop {
                if receiver.borrow().is_some() {
                    break;
                }
                receiver.changed().await.map_err(|_| {
                    malformed(
                        RequestKind::OrderedGraphPermitForgetRequest,
                        "permit reservation worker terminated",
                    )
                })?;
            }
        }
        let expected = entry
            .as_ref()
            .and_then(|entry| match &*entry.result.borrow() {
                Some(Ok(id)) => Some(*id),
                _ => None,
            });
        match self
            .call(|id| RadioRequest::OrderedGraphPermitReserveFence {
                id,
                key: key.clone(),
            })
            .await?
        {
            RadioCompletion::OrderedGraphReserveStatus(
                crate::OrderedGraphReserveStatus::NotCreated,
            ) if expected.is_none() => {}
            RadioCompletion::OrderedGraphReserveStatus(
                crate::OrderedGraphReserveStatus::Issued(id),
            ) if expected == Some(id) => {}
            _ => {
                return Err(malformed(
                    RequestKind::OrderedGraphPermitReserveFence,
                    "permit reservation fence unresolved or mismatched",
                ));
            }
        }
        self.unit(|id| RadioRequest::OrderedGraphPermitForgetRequest {
            id,
            key: key.clone(),
        })
        .await?;
        lock(&self.shared.reclaim_permit_opens).remove(&(key.authority_id, key.request_id));
        Ok(())
    }

    async fn acquire_ordered_reclaim_graph(
        &self,
        key: &ubm_desktop::boundary::OrderedReclaimGraphKey,
    ) -> Result<ubm_desktop::boundary::OrderedReclaimGraphAcquire, DesktopError> {
        use ubm_desktop::boundary::OrderedReclaimGraphAcquire as A;
        match self
            .call(|id| RadioRequest::OrderedGraphPermitAcquire {
                id,
                key: key.clone(),
            })
            .await?
        {
            RadioCompletion::OrderedGraphPermit(crate::OrderedGraphPermitState::Acquired) => {
                Ok(A::Acquired)
            }
            RadioCompletion::OrderedGraphPermit(crate::OrderedGraphPermitState::Stale) => {
                Ok(A::Stale)
            }
            RadioCompletion::OrderedGraphPermit(crate::OrderedGraphPermitState::Busy) => {
                Ok(A::Busy)
            }
            RadioCompletion::OrderedGraphPermitNotAcquired(failure) => Ok(A::NotAcquired(failed(
                &failure,
                RequestKind::OrderedGraphPermitAcquire,
                self.shared.mobile_platform,
            ))),
            _ => Err(malformed(
                RequestKind::OrderedGraphPermitAcquire,
                "invalid graph permit acquisition",
            )),
        }
    }
    async fn commit_ordered_reclaim_graph(
        &self,
        key: &ubm_desktop::boundary::OrderedReclaimGraphKey,
    ) -> Result<ubm_desktop::boundary::OrderedReclaimGraphCommit, DesktopError> {
        use ubm_desktop::boundary::OrderedReclaimGraphCommit as C;
        match self
            .call(|id| RadioRequest::OrderedGraphPermitCommit {
                id,
                key: key.clone(),
            })
            .await?
        {
            RadioCompletion::OrderedGraphPermit(crate::OrderedGraphPermitState::Committed) => {
                Ok(C::Committed)
            }
            RadioCompletion::OrderedGraphPermitInvalidated(failure) => Ok(C::Invalidated(failed(
                &failure,
                RequestKind::OrderedGraphPermitCommit,
                self.shared.mobile_platform,
            ))),
            _ => Err(malformed(
                RequestKind::OrderedGraphPermitCommit,
                "invalid graph permit commit",
            )),
        }
    }
    async fn release_ordered_reclaim_graph(
        &self,
        key: &ubm_desktop::boundary::OrderedReclaimGraphKey,
    ) -> Result<(), DesktopError> {
        match self
            .call(|id| RadioRequest::OrderedGraphPermitRelease {
                id,
                key: key.clone(),
            })
            .await?
        {
            RadioCompletion::OrderedGraphPermit(crate::OrderedGraphPermitState::Released) => Ok(()),
            _ => Err(malformed(
                RequestKind::OrderedGraphPermitRelease,
                "invalid graph permit release",
            )),
        }
    }

    async fn forget_ordered_reclaim_graph(
        &self,
        key: &ubm_desktop::boundary::OrderedReclaimGraphKey,
    ) -> Result<(), DesktopError> {
        match self
            .call(|id| RadioRequest::OrderedGraphPermitForget {
                id,
                key: key.clone(),
            })
            .await?
        {
            RadioCompletion::OrderedGraphPermit(crate::OrderedGraphPermitState::Forgotten) => {
                let request = {
                    lock(&self.shared.reclaim_permit_opens).values().find(|entry| entry.key.authority_id == key.authority_id && matches!(&*entry.result.borrow(), Some(Ok(id)) if *id == key.permit_id)).map(|entry| entry.key.clone())
                };
                if let Some(request) = request {
                    self.forget_ordered_reclaim_permit_request(&request).await?;
                }
                Ok(())
            }
            _ => Err(malformed(
                RequestKind::OrderedGraphPermitForget,
                "invalid graph permit forget",
            )),
        }
    }

    fn ordered_marker_write_supported(&self) -> bool {
        #[cfg(any(test, all(feature = "synthetic-marker-harness", debug_assertions)))]
        {
            self.shared.marker_supported_for_test.load(Ordering::SeqCst)
        }
        #[cfg(not(any(test, all(feature = "synthetic-marker-harness", debug_assertions))))]
        {
            false
        }
    }

    async fn start_ordered_marker_write(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        request: &ubm_desktop::boundary::OrderedMarkerWriteRequest,
    ) -> Result<ubm_desktop::boundary::OrderedMarkerWriteStatus, DesktopError> {
        if request.target.0 != lease.peer_id
            || request.connection_generation != lease.connection_generation
        {
            return Err(malformed(
                RequestKind::OrderedMarkerWrite,
                "marker source and target identity differ",
            ));
        }
        let target = &request.target;
        let result = self
            .call(|id| RadioRequest::OrderedMarkerWrite {
                id,
                generation: lease.source_generation.clone(),
                group_id: lease.native_group,
                marker_id: request.marker_id,
                connection_generation: request.connection_generation.clone(),
                snapshot_identity: request.snapshot_identity.clone(),
                instance: instance_of(&target.0, &target.1, target.2, &target.3, target.4),
                value: request.value.clone(),
                with_response: request.with_response,
            })
            .await;
        match result {
            Ok(RadioCompletion::OrderedMarkerWriteStatus(status)) => {
                Ok(marker_status(status, self.shared.mobile_platform))
            }
            Ok(_) => Err(malformed(
                RequestKind::OrderedMarkerWrite,
                "invalid marker status",
            )),
            Err(error) => Err(error),
        }
    }

    async fn cancel_ordered_marker_write(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        request: &ubm_desktop::boundary::OrderedMarkerWriteRequest,
    ) -> Result<ubm_desktop::boundary::OrderedMarkerWriteStatus, DesktopError> {
        match self
            .call(|id| RadioRequest::OrderedCancelMarkerWrite {
                id,
                generation: lease.source_generation.clone(),
                group_id: lease.native_group,
                marker_id: request.marker_id,
            })
            .await?
        {
            RadioCompletion::OrderedMarkerWriteStatus(status) => {
                Ok(marker_status(status, self.shared.mobile_platform))
            }
            _ => Err(malformed(
                RequestKind::OrderedCancelMarkerWrite,
                "invalid marker cancellation status",
            )),
        }
    }

    async fn ordered_marker_write_status(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        request: &ubm_desktop::boundary::OrderedMarkerWriteRequest,
    ) -> Result<ubm_desktop::boundary::OrderedMarkerWriteStatus, DesktopError> {
        match self
            .call(|id| RadioRequest::OrderedMarkerWriteSettlement {
                id,
                generation: lease.source_generation.clone(),
                group_id: lease.native_group,
                marker_id: request.marker_id,
            })
            .await?
        {
            RadioCompletion::OrderedMarkerWriteStatus(status) => {
                Ok(marker_status(status, self.shared.mobile_platform))
            }
            _ => Err(malformed(
                RequestKind::OrderedMarkerWriteSettlement,
                "invalid marker status",
            )),
        }
    }

    async fn forget_ordered_marker_write(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        request: &ubm_desktop::boundary::OrderedMarkerWriteRequest,
    ) -> Result<(), DesktopError> {
        self.unit(|id| RadioRequest::OrderedForgetMarkerWriteReceipt {
            id,
            generation: lease.source_generation.clone(),
            group_id: lease.native_group,
            marker_id: request.marker_id,
        })
        .await
    }

    fn ordered_source_requires_snapshot_identity(&self) -> bool {
        true
    }

    fn gatt_snapshot_identity(
        &self,
        peer_id: &str,
    ) -> Result<Option<ubm_desktop::boundary::GattSnapshotIdentity>, DesktopError> {
        Ok(lock(&self.shared.gatt_identities).get(peer_id).cloned())
    }

    async fn discover_scoped(
        &self,
        peer_id: &str,
    ) -> Result<
        (
            Vec<ServiceSnapshot>,
            Option<ubm_desktop::boundary::GattSnapshotIdentity>,
        ),
        DesktopError,
    > {
        let peer = peer_id.to_owned();
        match self
            .call(|id| RadioRequest::Discover {
                id,
                peer_id: peer.clone(),
            })
            .await?
        {
            RadioCompletion::DiscoveredScoped { services, identity }
                if !identity.owner.is_empty() =>
            {
                lock(&self.shared.gatt_identities).insert(peer, identity.clone());
                Ok((services, Some(identity)))
            }
            RadioCompletion::Discovered(services) => {
                self.invalidate_gatt_snapshot(&peer);
                Ok((services, None))
            }
            _ => Err(Self::unexpected(RequestKind::Discover)),
        }
    }
    async fn prepare_ordered_source(
        &self,
        request: ubm_desktop::boundary::OrderedSourceRequest,
    ) -> Result<
        ubm_desktop::boundary::OrderedSourceLease,
        ubm_desktop::boundary::OrderedSourceFailure,
    > {
        self.ordered_prepare_source(request).await
    }

    async fn recover_ordered_source(
        &self,
        peer_id: &str,
        collection_id: u64,
    ) -> Result<ubm_desktop::boundary::OrderedSourceRecovery, DesktopError> {
        use ubm_desktop::boundary::OrderedSourceRecovery;
        let _lifecycle = self.lock_ordered_open_lifecycle(collection_id).await;
        let Some(entry) = self.retained_ordered_open(collection_id) else {
            return Ok(OrderedSourceRecovery::NotStarted);
        };
        if entry.peer_id != peer_id {
            return Err(DesktopError::new(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Gatt,
                "gatt.ordered.recover",
            ));
        }
        if self.uses_v2_admission() {
            let mapping = self
                .ordered_host()?
                .ordered_group(collection_id)
                .ok_or_else(|| {
                    malformed(
                        RequestKind::OrderedRecoverOpenV2,
                        "v2 group mapping unavailable",
                    )
                })?;
            return self
                .recover_v2_ordered_source_locked(peer_id, collection_id, mapping, entry)
                .await;
        }
        let unresolved = match &entry.completion {
            None | Some(RadioCompletion::Failed(_)) => true,
            Some(RadioCompletion::OrderedOpenFailed {
                ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key },
                ..
            }) => {
                if *recovery_key != collection_id {
                    return Err(malformed(
                        RequestKind::OrderedRecoverOpen,
                        "native recovery key does not match group",
                    ));
                }
                true
            }
            _ => false,
        };
        if unresolved {
            let recovery = tokio::time::timeout(
                ORDERED_OPEN_RECEIPT_TIMEOUT,
                self.call(|id| RadioRequest::OrderedRecoverOpen {
                    id,
                    group_id: collection_id,
                    open_request_id: entry.request_id,
                    peer_id: entry.peer_id.clone(),
                }),
            );
            tokio::pin!(recovery);
            let resolved = loop {
                let changed = self.ordered_open_changed().notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let terminal = self
                    .retained_ordered_open(collection_id)
                    .is_some_and(|current| {
                        current.request_id == entry.request_id
                            && matches!(
                                current.completion,
                                Some(
                                    RadioCompletion::OrderedOpened { .. }
                                        | RadioCompletion::OrderedOpenFailed {
                                            ownership: crate::OrderedOpenOwnership::NotCreated
                                                | crate::OrderedOpenOwnership::Created { .. },
                                            ..
                                        }
                                )
                            )
                    });
                if terminal {
                    break None;
                }
                tokio::select! {
                    answer = &mut recovery => break Some(answer.map_err(|_| {
                        DesktopError::new(BleErrorCode::OperationTimedOut, BleErrorDomain::Gatt, "gatt.ordered.recover")
                            .with_detail("open recovery reply timed out; ownership remains retained")
                    })??),
                    () = &mut changed => {}
                }
            };
            if let Some(resolved) = resolved {
                if matches!(&resolved, RadioCompletion::OrderedOpenFailed {
                    ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key }, ..
                } if *recovery_key != collection_id)
                {
                    return Err(malformed(
                        RequestKind::OrderedRecoverOpen,
                        "native recovery key does not match group",
                    ));
                }
                let mut retained = lock(&self.shared.retained_ordered_opens);
                let current = retained.get_mut(&collection_id).ok_or_else(|| {
                    malformed(
                        RequestKind::OrderedRecoverOpen,
                        "retained source disappeared",
                    )
                })?;
                if current.request_id != entry.request_id {
                    return Err(malformed(
                        RequestKind::OrderedRecoverOpen,
                        "retained source changed",
                    ));
                }
                // A late original answer or another recovery may have settled
                // ownership while this request was in flight. Never overwrite
                // that newer decision with the stale response.
                if current.completion == entry.completion {
                    let mut candidate = current.clone();
                    candidate.completion = Some(resolved.clone());
                    // Validate the generation and memberships before replacing
                    // the unresolved receipt, so malformed replies can be retried.
                    Self::retained_entry_lease(candidate, peer_id, collection_id)?;
                    current.completion = Some(resolved);
                }
            }
        }
        if self
            .retained_ordered_open(collection_id)
            .is_some_and(|entry| {
                matches!(
                    entry.completion,
                    Some(
                        RadioCompletion::OrderedOpened { .. }
                            | RadioCompletion::OrderedOpenFailed {
                                ownership: crate::OrderedOpenOwnership::NotCreated
                                    | crate::OrderedOpenOwnership::Created { .. },
                                ..
                            }
                    )
                )
            })
        {
            self.acknowledge_ordered_open_locked(peer_id, collection_id)
                .await?;
        }
        if matches!(
            self.retained_ordered_open(collection_id)
                .and_then(|entry| entry.completion),
            Some(RadioCompletion::OrderedOpenFailed {
                ownership: crate::OrderedOpenOwnership::NotCreated,
                ..
            })
        ) {
            if !self.release_retained_ordered_open(collection_id) {
                return Err(malformed(
                    RequestKind::OrderedAcknowledgeOpen,
                    "acknowledged open ownership is unavailable",
                ));
            }
            if let Ok(host) = self.ordered_host() {
                host.ordered_wakes.remove(collection_id);
            }
            return Ok(OrderedSourceRecovery::NotStarted);
        }
        Ok(match self.retained_lease(peer_id, collection_id)? {
            Some(lease) => OrderedSourceRecovery::Ready(lease),
            None => OrderedSourceRecovery::Pending,
        })
    }

    async fn ordered_source_installed(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
    ) -> Result<(), DesktopError> {
        self.ordered_host()?.attach_ordered_source(lease)
    }

    async fn retain_ordered_source_claim(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
        activation: &ubm_desktop::boundary::PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        let member = self.validate_ordered_claim(lease, member_id, membership, activation)?;
        let generation = lease.source_generation.clone();
        match self
            .call(|id| RadioRequest::OrderedRetainClaim {
                id,
                generation: generation.clone(),
                group_id: lease.native_group,
                member,
            })
            .await?
        {
            RadioCompletion::OrderedClaim {
                generation: returned,
                member: claimed,
            } if returned == generation && claimed == member => Ok(()),
            _ => Err(Self::unexpected(RequestKind::OrderedRetainClaim)),
        }
    }

    async fn confirm_ordered_source_cleanup(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
        activation: &ubm_desktop::boundary::PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        let member = self.validate_ordered_claim(lease, member_id, membership, activation)?;
        self.unit(|id| RadioRequest::OrderedConfirmCleanup {
            id,
            generation: lease.source_generation.clone(),
            group_id: lease.native_group,
            member,
        })
        .await
    }

    async fn retire_ordered_member(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
    ) -> Result<(), DesktopError> {
        self.ordered_retire_member(lease, member_id, membership)
            .await
    }

    async fn reregister_ordered_member(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        old: ubm_core::ordered_notifications::Membership,
    ) -> Result<ubm_core::ordered_notifications::Membership, DesktopError> {
        self.ordered_reregister_member(lease, member_id, old).await
    }

    async fn resume_ordered_member(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
    ) -> Result<(), DesktopError> {
        self.ordered_resume_member(lease, member_id, membership)
            .await
    }

    async fn fence_ordered_source(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
    ) -> Result<(), DesktopError> {
        self.ordered_fence(lease).await
    }

    async fn retire_ordered_source(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        reason: ubm_core::ordered_notifications::EndReason,
    ) -> Result<(), DesktopError> {
        self.ordered_retire(lease, reason).await
    }

    async fn abandon_ordered_renewal(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        _old: ubm_core::ordered_notifications::Membership,
        new: Option<ubm_core::ordered_notifications::Membership>,
    ) -> Result<ubm_desktop::boundary::OrderedRenewalAbandon, DesktopError> {
        let Some(new) = new else {
            return Ok(ubm_desktop::boundary::OrderedRenewalAbandon::Pending);
        };
        // Central has rebound the exact new membership before invoking this hook.
        // Keep the retained pause until that rebind, then admit and acknowledge
        // everything ahead of a native barrier before lifecycle close proceeds.
        self.ordered_resume_member(lease, member_id, new).await?;
        let host = self.ordered_host()?;
        let consumer = host.ordered_consumer_for_lease(lease).ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.ordered.renewal",
            )
            .with_detail("ordered consumer is not attached")
        })?;
        match consumer.drain_to_empty().await {
            Ok(true) => {}
            Ok(false) => return Ok(ubm_desktop::boundary::OrderedRenewalAbandon::Pending),
            Err(error) => {
                consumer.mark_failed_on_drain_error(&error);
                return Err(error);
            }
        }
        if consumer.terminal_acked() {
            if self.seal_ordered_source(lease).await?
                == ubm_desktop::boundary::OrderedSourceSeal::Pending
            {
                return Ok(ubm_desktop::boundary::OrderedRenewalAbandon::Pending);
            }
        } else {
            self.ordered_fence(lease).await?;
        }
        Ok(ubm_desktop::boundary::OrderedRenewalAbandon::Retired {
            tail_uncertain: true,
        })
    }

    async fn release_ordered_source(
        &self,
        cleanup: &ubm_desktop::boundary::OrderedSourceCleanup,
    ) -> Result<ubm_desktop::boundary::OrderedSourceRelease, DesktopError> {
        self.ordered_release(cleanup).await
    }

    async fn seal_ordered_source(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
    ) -> Result<ubm_desktop::boundary::OrderedSourceSeal, DesktopError> {
        let _guard = self.lock_ordered_barrier(lease.native_group).await;
        let host = self.ordered_host()?;
        let consumer = host.ordered_consumer_for_lease(lease).ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.ordered.seal",
            )
            .with_detail("ordered consumer is not attached")
        })?;
        if !consumer.seal_confirmed() {
            let answer = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                self.unit(|id| RadioRequest::OrderedSeal {
                    id,
                    generation: lease.source_generation.clone(),
                    group_id: lease.native_group,
                }),
            )
            .await;
            match answer {
                Ok(result) => result?,
                Err(_) => return Ok(ubm_desktop::boundary::OrderedSourceSeal::Pending),
            }
            consumer.confirm_seal();
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            consumer.finish_sealed_drain(),
        )
        .await
        {
            Ok(Ok(true)) => Ok(ubm_desktop::boundary::OrderedSourceSeal::Sealed),
            Ok(Ok(false)) | Err(_) => Ok(ubm_desktop::boundary::OrderedSourceSeal::Pending),
            Ok(Err(error)) => Err(error),
        }
    }

    async fn prepare_notification_activation(
        &self,
        scope: &ubm_desktop::boundary::InstanceKey,
        epoch: u64,
        requested: Option<DeliveryMode>,
        preferred: Option<DeliveryMode>,
    ) -> Result<ubm_desktop::boundary::PreparedNotificationActivation, DesktopError> {
        Ok(ubm_desktop::boundary::PreparedNotificationActivation {
            scope: scope.clone(),
            activation_epoch: epoch,
            requested,
            preferred,
        })
    }

    fn reserve_ordered_notification(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
        prepared: &ubm_desktop::boundary::PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        let activation = self.ordered_activation(lease, member_id, membership, prepared)?;
        let mut retained = lock(&self.shared.retained_ordered_enables);
        if retained.abandoned.contains(&activation) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Gatt,
                "gatt.ordered.enable",
            )
            .with_detail("physical activation was abandoned"));
        }
        if retained.by_activation.contains_key(&activation)
            || retained.reserved.contains(&activation)
        {
            return Ok(());
        }
        if retained.by_activation.len() + retained.reserved.len() + retained.abandoned.len()
            >= ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY as usize
        {
            return Err(DesktopError::new(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "gatt.ordered.enable",
            )
            .with_detail("physical activation retention capacity exhausted"));
        }
        retained.reserved.insert(activation);
        Ok(())
    }

    async fn dispatch_ordered_notification(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
        prepared: &ubm_desktop::boundary::PreparedNotificationActivation,
    ) -> Result<ObservedDelivery, DesktopError> {
        let activation = self.ordered_activation(lease, member_id, membership, prepared)?;
        match self
            .call_ordered_enable(|id| RadioRequest::OrderedEnableNotifications {
                id,
                activation,
                requested: prepared.requested,
                preferred: prepared.preferred,
            })
            .await?
        {
            RadioCompletion::NotifyEnabled(delivery) => Ok(delivery),
            RadioCompletion::OrderedEnableReleased(failure) => Err(failed(
                &failure,
                RequestKind::OrderedEnableNotifications,
                self.shared.mobile_platform,
            )),
            _ => Err(Self::unexpected(RequestKind::OrderedEnableNotifications)),
        }
    }

    async fn ordered_notification_settlement(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
        prepared: &ubm_desktop::boundary::PreparedNotificationActivation,
    ) -> Result<ubm_desktop::boundary::OrderedNotificationSettlement, DesktopError> {
        use ubm_desktop::boundary::OrderedNotificationSettlement as Settlement;
        let activation = self.ordered_activation(lease, member_id, membership, prepared)?;
        {
            let mut retained = lock(&self.shared.retained_ordered_enables);
            if retained.reserved.remove(&activation) {
                retained.abandoned.insert(activation.clone());
            }
            if retained.abandoned.contains(&activation) {
                return Ok(Settlement::Released);
            }
            if !retained.by_activation.contains_key(&activation) {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.enable-settlement",
                )
                .with_detail("exact activation receipt is unavailable"));
            }
        }
        match self
            .call(|id| RadioRequest::OrderedEnableSettlement { id, activation })
            .await?
        {
            RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Pending) => {
                Ok(Settlement::Pending)
            }
            RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Enabled(_)) => {
                Ok(Settlement::Enabled)
            }
            RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Released(_)) => {
                Ok(Settlement::Released)
            }
            RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Uncertain(error)) => {
                Err(failed(
                    &error,
                    RequestKind::OrderedEnableSettlement,
                    self.shared.mobile_platform,
                ))
            }
            _ => Err(Self::unexpected(RequestKind::OrderedEnableSettlement)),
        }
    }

    async fn release_ordered_notification(
        &self,
        lease: &ubm_desktop::boundary::OrderedSourceLease,
        member_id: u64,
        membership: ubm_core::ordered_notifications::Membership,
        prepared: &ubm_desktop::boundary::PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        let activation = self.ordered_activation(lease, member_id, membership, prepared)?;
        {
            let mut retained = lock(&self.shared.retained_ordered_enables);
            if retained.reserved.remove(&activation) {
                retained.abandoned.insert(activation.clone());
            }
            if retained.abandoned.contains(&activation) {
                return Ok(());
            }
            if !retained.by_activation.contains_key(&activation) {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Cleanup,
                    "gatt.ordered.enable-release",
                )
                .with_detail("exact activation receipt is unavailable"));
            }
        }
        if !lock(&self.shared.ordered_enable_forget_pending).contains(&activation) {
            match self
                .call(|id| RadioRequest::OrderedEnableSettlement {
                    id,
                    activation: activation.clone(),
                })
                .await?
            {
                RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Pending) => {
                    return Err(DesktopError::new(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Cleanup,
                        "gatt.ordered.enable-settlement",
                    )
                    .with_detail("native enable remains pending"));
                }
                RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Uncertain(
                    error,
                )) => {
                    return Err(failed(
                        &error,
                        RequestKind::OrderedEnableSettlement,
                        self.shared.mobile_platform,
                    ));
                }
                RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Enabled(_)) => {
                    self.unit(|id| RadioRequest::OrderedDisableNotifications {
                        id,
                        activation: activation.clone(),
                    })
                    .await?;
                }
                RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Released(_)) => {}
                _ => return Err(Self::unexpected(RequestKind::OrderedEnableSettlement)),
            }
            lock(&self.shared.ordered_enable_forget_pending).insert(activation.clone());
        }
        self.unit(|id| RadioRequest::OrderedForgetEnableReceipt {
            id,
            activation: activation.clone(),
        })
        .await?;
        lock(&self.shared.ordered_enable_forget_pending).remove(&activation);
        let mut retained = lock(&self.shared.retained_ordered_enables);
        if let Some(request_id) = retained.by_activation.remove(&activation) {
            retained.by_request.remove(&request_id);
        }
        retained.abandoned.insert(activation);
        Ok(())
    }

    async fn flush_peer_notification_source(&self, peer_id: &str) -> Result<(), DesktopError> {
        self.ordered_flush_peer(peer_id).await
    }

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
        let (id, staged) = match lock(&self.shared.staged_scan).take() {
            Some(staged) => staged,
            None => (Self::next_request_id()?, ScanRequest::default()),
        };
        let scan = ScanRequest {
            service_uuids: filter.service_uuids,
            device_addresses: staged.device_addresses,
            android: staged.android,
        };
        match self
            .call_with_id(id, None, false, false, |id| RadioRequest::StartScan {
                id,
                scan,
            })
            .await?
        {
            RadioCompletion::Unit => Ok(()),
            _ => Err(Self::unexpected(RequestKind::StartScan)),
        }
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

    async fn connect(&self, peer_id: &str, expected_generation: &str) -> Result<(), DesktopError> {
        if expected_generation.is_empty() {
            return Err(DesktopError::new(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                "connection.connect",
            ));
        }
        let ConnectStaging {
            auto_connect,
            preferred_phy,
        } = lock(&self.shared.staged_connect)
            .remove(peer_id)
            .unwrap_or_default();
        let peer_id = peer_id.to_owned();
        let host = lock(&self.shared.connect_generation_hook).clone();
        let mut admission = if let Some(host) = host.as_ref() {
            let Some(host) = host.upgrade() else {
                return Err(DesktopError::new(
                    BleErrorCode::LifecycleDestroyed,
                    BleErrorDomain::Core,
                    "connection.connect",
                )
                .with_detail("mobile host closed before connect ownership admission"));
            };
            Some(ConnectSecurityAdmission {
                owned: host.stamp_security_generation(&peer_id, expected_generation),
                host: Arc::downgrade(&host),
                peer_id: peer_id.clone(),
                generation: expected_generation.to_owned(),
            })
        } else {
            None
        };
        let result = self
            .unit(|id| RadioRequest::Connect {
                id,
                peer_id: peer_id.clone(),
                auto_connect,
                preferred_phy,
                expected_generation: expected_generation.to_owned(),
            })
            .await;
        if result.is_ok()
            && let Some(admission) = admission.as_mut()
        {
            admission.disarm();
        }
        result
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peer_id = peer_id.to_owned();
        self.unit(|id| RadioRequest::Disconnect { id, peer_id })
            .await
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        let (services, _) = self.discover_scoped(peer_id).await?;
        Ok(services)
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
            let fence = lock(&self.shared.recorded_disables)
                .get(&(instance.clone(), epoch))
                .cloned();
            // A last consumer stays in the core while the physical disable is
            // in flight, so admit its final callbacks until disable settles.
            if let Some(slot) = &fence {
                lock(slot).take();
            }
            self.unit(|id| RadioRequest::DisableNotifications {
                id,
                instance,
                epoch,
            })
            .await?;
            if let Some(slot) = fence {
                *lock(&slot) = Some(self.fence_ingress().await?);
            }
            return Ok(ObservedDelivery::Unknown);
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

    /// Apple probes `canSendWriteWithoutResponse`. Android has no queue
    /// readiness signal, so the core refuses before a write waits on it.
    async fn write_without_response_ready(&self, peer_id: &str) -> Result<bool, DesktopError> {
        if self.shared.mobile_platform != MobilePlatform::Apple {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "gatt.write-readiness",
            )
            .with_detail("Android reports no write-without-response readiness"));
        }
        let peer_id = peer_id.to_owned();
        match self
            .call(|id| RadioRequest::ReadWriteReadiness { id, peer_id })
            .await?
        {
            RadioCompletion::Ready(ready) => Ok(ready),
            _ => Err(Self::unexpected(RequestKind::ReadWriteReadiness)),
        }
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        loop {
            let notified = self.shared.notify.notified();
            let barrier = {
                let mut ingress = lock(&self.shared.ingress);
                let heads = [
                    ingress.advertisements.front().map(|(seq, _)| *seq),
                    ingress.data.front().map(|(seq, _)| *seq),
                    ingress.control.front().map(|(seq, _)| *seq),
                    ingress.barriers.front().map(|(seq, _)| *seq),
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
                        if let Some(
                            RadioEvent::Notification { ref value, .. }
                            | RadioEvent::NotificationWithReceipt { ref value, .. },
                        ) = event
                        {
                            ingress.data_bytes = ingress.data_bytes.saturating_sub(value.len());
                        }
                        return event;
                    }
                    Some((_, 2)) => return ingress.control.pop_front().map(|(_, e)| e),
                    Some(_) => ingress.barriers.pop_front().map(|(_, barrier)| barrier),
                    None if ingress.closed => return None,
                    None => None,
                }
            };
            if let Some(barrier) = barrier {
                #[cfg(debug_assertions)]
                self.shared.ingress_paused.store(true, Ordering::SeqCst);
                let _ = barrier.arrived.send(());
                let _ = barrier.resume.await;
                #[cfg(debug_assertions)]
                self.shared.ingress_paused.store(false, Ordering::SeqCst);
                continue;
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

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_power_is_an_observation_not_a_reset() {
        let mut snapshot = super::AdapterSnapshot {
            availability: super::AdapterAvailability::Available,
            authorization: super::AdapterAuthorization::NotDetermined,
            power: super::AdapterPower::Unknown,
            safe_reason: None,
        };
        assert_eq!(super::legacy_loss(&snapshot), None);
        assert_eq!(
            super::central_adapter_state(&snapshot),
            super::AdapterPowerState::Unknown
        );
        snapshot.power = super::AdapterPower::Resetting;
        assert_eq!(
            super::legacy_loss(&snapshot),
            Some(super::AdapterPowerState::Resetting)
        );
        snapshot.power = super::AdapterPower::Off;
        assert_eq!(
            super::legacy_loss(&snapshot),
            Some(super::AdapterPowerState::PoweredOff)
        );
        snapshot.power = super::AdapterPower::On;
        assert_eq!(
            super::central_adapter_state(&snapshot),
            super::AdapterPowerState::PoweredOn
        );
        snapshot.power = super::AdapterPower::Unknown;
        snapshot.authorization = super::AdapterAuthorization::Denied;
        assert_eq!(
            super::central_adapter_state(&snapshot),
            super::AdapterPowerState::Unauthorized
        );
        snapshot.authorization = super::AdapterAuthorization::NotDetermined;
        snapshot.availability = super::AdapterAvailability::Unknown;
        assert_eq!(super::legacy_loss(&snapshot), None);
        assert_eq!(
            super::central_adapter_state(&snapshot),
            super::AdapterPowerState::Unknown
        );
        snapshot.power = super::AdapterPower::On;
        assert_eq!(
            super::central_adapter_state(&snapshot),
            super::AdapterPowerState::PoweredOn
        );
        snapshot.availability = super::AdapterAvailability::Unavailable;
        assert_eq!(
            super::legacy_loss(&snapshot),
            Some(super::AdapterPowerState::Unsupported)
        );
    }
    use super::*;

    macro_rules! negotiated_v1 {
        () => {
            fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
                Some(
                    crate::OrderedAdmissionOwner::negotiate(
                        1,
                        "12345678-1234-4abc-8def-123456789af0",
                    )
                    .unwrap(),
                )
            }
        };
    }
    macro_rules! negotiated_v2 {
        () => {
            fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
                Some(
                    crate::OrderedAdmissionOwner::negotiate_slot(
                        2,
                        "12345678-1234-4abc-8def-123456789abc",
                    )
                    .unwrap(),
                )
            }
            fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
                self.ordered_admission_owner()
                    .map(|owner| crate::OrderedAdmissionNegotiation { version: 2, owner })
            }
        };
    }

    struct Stub;

    impl PlatformRadio for Stub {
        negotiated_v1!();
        fn submit(&self, _request: RadioRequest) {}
        fn cancel(&self, _request_id: RequestId) {}
    }

    fn radio() -> ForeignRadio {
        ForeignRadio::new(
            Arc::new(Stub),
            MobilePlatform::Android,
            "stub-adapter".to_owned(),
        )
    }

    fn advertisement(peer_id: &str) -> RadioEvent {
        RadioEvent::Advertisement(PeerSnapshot {
            id: peer_id.to_owned(),
            address: None,
            service_uuids: Vec::new(),
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: ubm_desktop::boundary::AdvertisementExtras::default(),
        })
    }

    #[tokio::test]
    async fn retired_scan_ingress_fence_accepts_closed_drained_source() {
        let radio = radio();

        radio.arm_scan_ingress(41).await.unwrap();
        radio.retire_scan_ingress(41).await.unwrap();
        radio.close_events();

        radio.fence_scan_ingress(41).await.unwrap();
    }

    #[tokio::test]
    async fn retired_scan_ingress_fence_refuses_closed_undrained_prefix_then_succeeds() {
        let radio = radio();

        radio.arm_scan_ingress(42).await.unwrap();
        radio
            .push_event(advertisement("accepted-before-retirement"))
            .unwrap();
        radio.retire_scan_ingress(42).await.unwrap();
        radio.close_events();

        let error = radio.fence_scan_ingress(42).await.unwrap_err();
        assert_eq!(error.code(), BleErrorCode::ScanStopFailed);
        assert_eq!(
            radio.next_event().await,
            Some(RadioEvent::AdvertisementScoped {
                snapshot: PeerSnapshot {
                    id: "accepted-before-retirement".to_owned(),
                    address: None,
                    service_uuids: Vec::new(),
                    rssi: Some(-60),
                    local_name: None,
                    manufacturer_data: Vec::new(),
                    service_data: Vec::new(),
                    tx_power_level: None,
                    extras: ubm_desktop::boundary::AdvertisementExtras::default(),
                },
                ingress_epoch: 42,
            })
        );

        radio.fence_scan_ingress(42).await.unwrap();
    }

    #[test]
    fn central_cleanup_error_survives_native_no_effect_retirement() {
        let radio = radio();
        lock(&radio.shared.ordered_cleanup_due).insert(17, ("peer".into(), 42));
        lock(&radio.shared.ordered_recovery).insert(42, Vec::new());
        let error = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Cleanup,
            "original-cleanup",
        );
        radio.record_v2_recovery_error(17, error.clone());
        assert_eq!(radio.v2_recovery_errors().len(), 1);
        assert_eq!(radio.v2_recovery_errors()[0].operation(), error.operation());
        radio.complete_v2_central_cleanup(17);
        assert!(radio.v2_recovery_errors().is_empty());
    }

    #[test]
    fn central_cleanup_retains_distinct_failures_for_one_obligation() {
        let radio = radio();
        lock(&radio.shared.ordered_cleanup_due).insert(17, ("peer".into(), 42));
        lock(&radio.shared.ordered_recovery).insert(42, Vec::new());
        for operation in ["first-cleanup", "second-cleanup"] {
            radio.record_v2_recovery_error(
                17,
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Cleanup,
                    operation,
                ),
            );
        }
        let failures = radio.v2_recovery_errors();
        assert!(
            failures
                .iter()
                .any(|error| error.operation() == "first-cleanup")
        );
        assert!(
            failures
                .iter()
                .any(|error| error.operation() == "second-cleanup")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_retains_owner_for_central_debt_after_slot_removal() {
        use crate::test_common::*;
        let platform = Scripted::polar();
        let (host, _) = open(&platform, crate::MobilePlatform::Android).await;
        lock(&host.inner.radio.shared.ordered_cleanup_due).insert(17, ("peer".into(), 42));
        lock(&host.inner.radio.shared.ordered_recovery).insert(42, Vec::new());
        host.inner.radio.record_v2_recovery_error(
            17,
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Cleanup,
                "central-cleanup-evidence",
            )
            .with_detail("retained central cleanup failure"),
        );
        let receipt: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
        assert_eq!(receipt["state"], "release-failed");
        assert!(
            receipt.to_string().contains("central-cleanup-evidence"),
            "{receipt}"
        );
        assert!(
            receipt
                .to_string()
                .contains("retained central cleanup failure"),
            "{receipt}"
        );
        assert_eq!(platform.count(RequestKind::Close), 0);
        host.inner.radio.complete_v2_central_cleanup(17);
        let receipt: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
        assert_eq!(receipt["state"], "released");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_reports_central_debt_with_pending_scan_transfer() {
        use crate::test_common::*;
        let platform = Scripted::polar();
        let (host, _) = open(&platform, crate::MobilePlatform::Android).await;
        lock(&host.inner.radio.shared.ordered_cleanup_due).insert(17, ("peer".into(), 42));
        lock(&host.inner.radio.shared.ordered_recovery).insert(42, Vec::new());
        host.inner.radio.record_v2_recovery_error(
            17,
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Cleanup,
                "central-cleanup-evidence",
            ),
        );
        host.test_hold_scan_failure_transfer(true);
        let receipt: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
        assert!(receipt.to_string().contains("scan-failure"), "{receipt}");
        assert!(
            receipt.to_string().contains("central-cleanup-evidence"),
            "{receipt}"
        );
        host.test_hold_scan_failure_transfer(false);
        host.inner.radio.complete_v2_central_cleanup(17);
        let _ = host.shutdown().await;
    }

    #[tokio::test]
    async fn graph_authority_open_coalesces_and_retries_only_no_effect() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let first = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.open_ordered_reclaim_authority(91).await })
        };
        let reserve_id = rx.recv().await.unwrap().id();
        first.abort();
        let retry = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.open_ordered_reclaim_authority(91).await })
        };
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "lost waiter must not duplicate native reservation"
        );
        radio.complete(
            reserve_id,
            RadioCompletion::OrderedGraphAuthorityReserved(3),
        );
        let open_id = rx.recv().await.unwrap().id();
        let mut failure = PlatformFailure::new(crate::FailureKind::Busy, "capacity");
        failure.dispatched = false;
        radio.complete(open_id, RadioCompletion::Failed(failure));
        assert!(retry.await.unwrap().is_err());
        let retire = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.forget_ordered_reclaim_open(91).await })
        };
        let retire_fence = rx.recv().await.unwrap().id();
        radio.complete(
            retire_fence,
            RadioCompletion::OrderedGraphReserveStatus(crate::OrderedGraphReserveStatus::Issued(3)),
        );
        let open_fence = rx.recv().await.unwrap();
        assert!(matches!(
            open_fence,
            RadioRequest::OrderedGraphAuthorityFenceOpen { request: 3, .. }
        ));
        radio.complete(
            open_fence.id(),
            RadioCompletion::OrderedGraphAuthorityStatus(
                crate::OrderedGraphAuthorityStatus::NotCreated,
            ),
        );
        let retire_id = rx.recv().await.unwrap().id();
        radio.complete(retire_id, RadioCompletion::Unit);
        let retire_token = rx.recv().await.unwrap().id();
        radio.complete(retire_token, RadioCompletion::Unit);
        retire.await.unwrap().unwrap();
        let next = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.open_ordered_reclaim_authority(92).await })
        };
        let reserve_id = rx.recv().await.unwrap().id();
        radio.complete(
            reserve_id,
            RadioCompletion::OrderedGraphAuthorityReserved(4),
        );
        let next_id = rx.recv().await.unwrap().id();
        radio.complete(next_id, RadioCompletion::OrderedGraphAuthorityOpened(7));
        assert_eq!(next.await.unwrap().unwrap(), 7);
        assert_eq!(radio.open_ordered_reclaim_authority(92).await.unwrap(), 7);
        assert!(rx.try_recv().is_err());
        let close = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.close_ordered_reclaim_authority(7).await })
        };
        let close_id = rx.recv().await.unwrap().id();
        radio.complete(close_id, RadioCompletion::Unit);
        close.await.unwrap().unwrap();
        let forget = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.forget_ordered_reclaim_authority(7).await })
        };
        let forget_id = rx.recv().await.unwrap().id();
        radio.complete(forget_id, RadioCompletion::Unit);
        let forget_fence = rx.recv().await.unwrap().id();
        radio.complete(
            forget_fence,
            RadioCompletion::OrderedGraphReserveStatus(crate::OrderedGraphReserveStatus::Issued(4)),
        );
        let open_fence = rx.recv().await.unwrap();
        assert!(matches!(
            open_fence,
            RadioRequest::OrderedGraphAuthorityFenceOpen { request: 4, .. }
        ));
        radio.complete(
            open_fence.id(),
            RadioCompletion::OrderedGraphAuthorityStatus(
                crate::OrderedGraphAuthorityStatus::Closed(7),
            ),
        );
        let forget_open_id = rx.recv().await.unwrap().id();
        radio.complete(forget_open_id, RadioCompletion::Unit);
        let forget_token_id = rx.recv().await.unwrap().id();
        radio.complete(forget_token_id, RadioCompletion::Unit);
        forget.await.unwrap().unwrap();
        assert!(lock(&radio.shared.reclaim_authority_opens).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn graph_authority_lost_reservation_reply_queries_fence_without_redispatch() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let waiter = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.open_ordered_reclaim_authority(44).await })
        };
        assert!(matches!(
            rx.recv().await.unwrap(),
            RadioRequest::OrderedGraphAuthorityReserve { request: 44, .. }
        ));
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let fence = rx.recv().await.unwrap();
        assert!(matches!(
            fence,
            RadioRequest::OrderedGraphAuthorityReserveFence { request: 44, .. }
        ));
        radio.complete(
            fence.id(),
            RadioCompletion::OrderedGraphReserveStatus(crate::OrderedGraphReserveStatus::Issued(9)),
        );
        let open = rx.recv().await.unwrap();
        assert!(matches!(
            open,
            RadioRequest::OrderedGraphAuthorityOpen { request: 9, .. }
        ));
        radio.complete(open.id(), RadioCompletion::OrderedGraphAuthorityOpened(11));
        assert_eq!(waiter.await.unwrap().unwrap(), 11);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn graph_authority_lost_open_closed_is_retired_before_no_effect() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let waiter = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.open_ordered_reclaim_authority(45).await })
        };
        let reserve = rx.recv().await.unwrap();
        radio.complete(
            reserve.id(),
            RadioCompletion::OrderedGraphAuthorityReserved(9),
        );
        assert!(matches!(
            rx.recv().await.unwrap(),
            RadioRequest::OrderedGraphAuthorityOpen { request: 9, .. }
        ));
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let fence = rx.recv().await.unwrap();
        assert!(matches!(
            fence,
            RadioRequest::OrderedGraphAuthorityFenceOpen { request: 9, .. }
        ));
        radio.complete(
            fence.id(),
            RadioCompletion::OrderedGraphAuthorityStatus(
                crate::OrderedGraphAuthorityStatus::Closed(11),
            ),
        );
        let forget = rx.recv().await.unwrap();
        assert!(matches!(
            forget,
            RadioRequest::OrderedGraphAuthorityForget {
                authority_id: 11,
                ..
            }
        ));
        radio.complete(forget.id(), RadioCompletion::Unit);
        let error = waiter.await.unwrap().unwrap_err();
        assert_eq!(
            error.commit(),
            Some(ubm_core::contracts::CommitState::NotDispatched)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn graph_permit_reservation_lost_reply_is_fenced_and_retired() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let key = ubm_desktop::boundary::OrderedReclaimPermitRequest {
            authority_id: 5,
            request_id: 7,
            peer_id: "peer".into(),
            connection_generation: "generation".into(),
            identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "graph".into(),
                attachment: u64::MAX,
                revision: u64::MAX,
            },
        };
        let waiter = {
            let radio = radio.clone();
            let key = key.clone();
            tokio::spawn(async move { radio.reserve_ordered_reclaim_permit(&key).await })
        };
        assert!(
            matches!(rx.recv().await.unwrap(), RadioRequest::OrderedGraphPermitReserve { key: ref actual, .. } if *actual == key)
        );
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let fence = rx.recv().await.unwrap();
        assert!(
            matches!(fence, RadioRequest::OrderedGraphPermitReserveFence { key: ref actual, .. } if *actual == key)
        );
        radio.complete(
            fence.id(),
            RadioCompletion::OrderedGraphReserveStatus(crate::OrderedGraphReserveStatus::Issued(9)),
        );
        assert_eq!(waiter.await.unwrap().unwrap(), 9);
        let retire = {
            let radio = radio.clone();
            let key = key.clone();
            tokio::spawn(async move { radio.forget_ordered_reclaim_permit_request(&key).await })
        };
        let fence = rx.recv().await.unwrap();
        radio.complete(
            fence.id(),
            RadioCompletion::OrderedGraphReserveStatus(crate::OrderedGraphReserveStatus::Issued(9)),
        );
        let forget = rx.recv().await.unwrap();
        assert!(
            matches!(forget, RadioRequest::OrderedGraphPermitForgetRequest { key: ref actual, .. } if *actual == key)
        );
        radio.complete(forget.id(), RadioCompletion::Unit);
        retire.await.unwrap().unwrap();
        assert!(lock(&radio.shared.reclaim_permit_opens).is_empty());
    }

    #[tokio::test]
    async fn ordered_late_drain_can_be_retried_then_acknowledged() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (requests, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(requests)),
            MobilePlatform::Android,
            "test".into(),
        );
        let generation = crate::OrderedSourceGeneration::Counter(u64::MAX);
        let request_generation = generation.clone();
        let first_radio = radio.clone();
        let first = tokio::spawn(async move {
            first_radio
                .call(|id| RadioRequest::OrderedDrain {
                    id,
                    generation: request_generation,
                    group_id: 4,
                })
                .await
        });
        let old_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedDrain { id, .. } => id,
            other => panic!("{other:?}"),
        };
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let batch = RadioCompletion::OrderedRecords {
            batch_id: Some(u64::MAX),
            records: vec![crate::OrderedRecord::Fence {
                member: crate::OrderedRegistration {
                    member_id: 1,
                    source_member: 2,
                    epoch: 3,
                },
                uncertain_tail: true,
            }],
        };
        assert_eq!(
            radio.complete(old_id, batch.clone()),
            CompletionStatus::Late
        );
        let next_radio = radio.clone();
        let next_generation = generation.clone();
        let next = tokio::spawn(async move {
            next_radio
                .call(|id| RadioRequest::OrderedDrain {
                    id,
                    generation: next_generation,
                    group_id: 4,
                })
                .await
        });
        let next_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedDrain { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(next_id, batch.clone()),
            CompletionStatus::Delivered
        );
        assert_eq!(next.await.unwrap().unwrap(), batch);
        let ack_radio = radio.clone();
        let ack = tokio::spawn(async move {
            ack_radio
                .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                    id,
                    generation,
                    group_id: 4,
                    batch_id: u64::MAX,
                })
                .await
        });
        let ack_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedAcknowledgeBatch {
                id,
                batch_id: u64::MAX,
                ..
            } => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(ack_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        assert_eq!(ack.await.unwrap().unwrap(), RadioCompletion::Unit);
    }

    #[tokio::test]
    async fn marker_boundary_routes_pending_settlement_and_exact_forget() {
        use ubm_desktop::boundary::{
            GattSnapshotIdentity, OrderedMarkerWriteRequest, OrderedMarkerWriteStatus,
            OrderedSourceIngress, OrderedSourceLease,
        };
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(tx)),
            MobilePlatform::Android,
            "test".into(),
        );
        let lease = OrderedSourceLease {
            peer_id: "p".into(),
            connection_generation: "connection".into(),
            collection_id: 4,
            source_generation: crate::OrderedSourceGeneration::Counter(u64::MAX),
            native_group: u64::MAX,
            members: vec![],
            ingress: OrderedSourceIngress::CentralRetainedBatch,
        };
        let request = OrderedMarkerWriteRequest {
            marker_id: u64::MAX,
            connection_generation: "connection".into(),
            snapshot_identity: GattSnapshotIdentity {
                owner: "graph".into(),
                attachment: u64::MAX,
                revision: 1,
            },
            target: ("p".into(), "s".into(), 2, "c".into(), 3),
            value: vec![1, 255],
            with_response: true,
        };
        let task = {
            let radio = radio.clone();
            let lease = lease.clone();
            let request = request.clone();
            tokio::spawn(async move { radio.start_ordered_marker_write(&lease, &request).await })
        };
        let id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedMarkerWrite {
                id,
                group_id: u64::MAX,
                marker_id: u64::MAX,
                instance,
                value,
                ..
            } if instance.service_occurrence == 2 && value == vec![1, 255] => id,
            other => panic!("unexpected {other:?}"),
        };
        radio.complete(
            id,
            RadioCompletion::OrderedMarkerWriteStatus(crate::OrderedMarkerWriteStatus {
                admission: crate::OrderedMarkerAdmission::Pending,
                write: crate::OrderedMarkerWriteState::NotStarted { failure: None },
            }),
        );
        assert!(matches!(
            task.await.unwrap().unwrap(),
            OrderedMarkerWriteStatus::Pending
        ));
        let cancel = {
            let radio = radio.clone();
            let lease = lease.clone();
            let request = request.clone();
            tokio::spawn(async move { radio.cancel_ordered_marker_write(&lease, &request).await })
        };
        let cancel_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedCancelMarkerWrite {
                id,
                group_id: u64::MAX,
                marker_id: u64::MAX,
                ..
            } => id,
            other => panic!("unexpected {other:?}"),
        };
        radio.complete(
            cancel_id,
            RadioCompletion::OrderedMarkerWriteStatus(crate::OrderedMarkerWriteStatus {
                admission: crate::OrderedMarkerAdmission::Admitted { ordinal: u64::MAX },
                write: crate::OrderedMarkerWriteState::Started { failure: None },
            }),
        );
        assert!(matches!(
            cancel.await.unwrap().unwrap(),
            OrderedMarkerWriteStatus::Admitted {
                ordinal: u64::MAX,
                write: ubm_desktop::boundary::OrderedMarkerWritePhase::Started(None)
            }
        ));
        let task = {
            let radio = radio.clone();
            let lease = lease.clone();
            let request = request.clone();
            tokio::spawn(async move { radio.ordered_marker_write_status(&lease, &request).await })
        };
        let id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedMarkerWriteSettlement {
                id,
                group_id: u64::MAX,
                marker_id: u64::MAX,
                ..
            } => id,
            other => panic!("unexpected {other:?}"),
        };
        radio.complete(
            id,
            RadioCompletion::OrderedMarkerWriteStatus(crate::OrderedMarkerWriteStatus {
                admission: crate::OrderedMarkerAdmission::Admitted { ordinal: u64::MAX },
                write: crate::OrderedMarkerWriteState::Settled { failure: None },
            }),
        );
        assert!(matches!(
            task.await.unwrap().unwrap(),
            OrderedMarkerWriteStatus::Admitted {
                ordinal: u64::MAX,
                ..
            }
        ));
        let task = {
            let radio = radio.clone();
            let lease = lease.clone();
            let request = request.clone();
            tokio::spawn(async move { radio.ordered_marker_write_status(&lease, &request).await })
        };
        let id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedMarkerWriteSettlement { id, .. } => id,
            other => panic!("unexpected {other:?}"),
        };
        radio.complete(
            id,
            RadioCompletion::OrderedMarkerWriteStatus(crate::OrderedMarkerWriteStatus {
                admission: crate::OrderedMarkerAdmission::NotAdmitted,
                write: crate::OrderedMarkerWriteState::NotStarted { failure: None },
            }),
        );
        assert!(matches!(
            task.await.unwrap().unwrap(),
            OrderedMarkerWriteStatus::NotAdmitted(None)
        ));
        let task = {
            let radio = radio.clone();
            tokio::spawn(async move { radio.forget_ordered_marker_write(&lease, &request).await })
        };
        let id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedForgetMarkerWriteReceipt {
                id,
                group_id: u64::MAX,
                marker_id: u64::MAX,
                ..
            } => id,
            other => panic!("unexpected {other:?}"),
        };
        radio.complete(id, RadioCompletion::Unit);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn v2_negotiation_does_not_dispatch_legacy_open() {
        struct V2Owner(
            tokio::sync::mpsc::UnboundedSender<RadioRequest>,
            crate::OrderedAdmissionOwner,
        );
        impl PlatformRadio for V2Owner {
            fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
                Some(self.1)
            }
            fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
                Some(crate::OrderedAdmissionNegotiation {
                    version: 2,
                    owner: self.1,
                })
            }
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789acc")
                .unwrap();
        let radio = ForeignRadio::new(
            Arc::new(V2Owner(sender, owner)),
            MobilePlatform::Android,
            "test".into(),
        );
        let task = tokio::spawn(async move {
            radio
                .call(|id| RadioRequest::OrderedOpen {
                    id,
                    group_id: 0,
                    connection_generation: "c".into(),
                    database_generation: "d".into(),
                    snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                        owner: "native".into(),
                        attachment: 1,
                        revision: 1,
                    },
                    members: vec![crate::OrderedMember {
                        member_id: 1,
                        instance: instance_of("p", "s", 0, "c", 0),
                    }],
                    item_capacity: 4,
                    byte_capacity: 64,
                })
                .await
        });
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "v2 owner received a legacy open");
        let result = task.await.unwrap();
        assert_eq!(
            result.unwrap_err().code(),
            BleErrorCode::ProtocolIncompatible
        );
    }

    #[tokio::test]
    async fn v2_negotiation_rejects_legacy_recovery_and_acknowledgement() {
        struct V2Owner(
            tokio::sync::mpsc::UnboundedSender<RadioRequest>,
            crate::OrderedAdmissionOwner,
        );
        impl PlatformRadio for V2Owner {
            fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
                Some(self.1)
            }
            fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
                Some(crate::OrderedAdmissionNegotiation {
                    version: 2,
                    owner: self.1,
                })
            }
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789aca")
                .unwrap();
        let radio = ForeignRadio::new(
            Arc::new(V2Owner(sender, owner)),
            MobilePlatform::Android,
            "test".into(),
        );
        let recover = {
            let radio = radio.clone();
            tokio::spawn(async move {
                radio
                    .call(|id| RadioRequest::OrderedRecoverOpen {
                        id,
                        group_id: 7,
                        open_request_id: 9,
                        peer_id: "p".into(),
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "v2 owner received legacy recovery");
        assert_eq!(
            recover.await.unwrap().unwrap_err().code(),
            BleErrorCode::ProtocolIncompatible
        );
        let ack = {
            let radio = radio.clone();
            tokio::spawn(async move {
                radio
                    .call(|id| RadioRequest::OrderedAcknowledgeOpen {
                        id,
                        version: 1,
                        group_id: 7,
                        open_request_id: 9,
                        peer_id: "p".into(),
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "v2 owner received legacy acknowledgement"
        );
        assert_eq!(
            ack.await.unwrap().unwrap_err().code(),
            BleErrorCode::ProtocolIncompatible
        );
    }

    #[tokio::test]
    async fn v2_recovery_without_host_never_falls_back_to_legacy() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v2!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![],
                completion: None,
            },
        );
        assert!(radio.recover_ordered_source("p", 17).await.is_err());
        lock(&radio.shared.retained_ordered_opens)
            .get_mut(&17)
            .unwrap()
            .completion = Some(RadioCompletion::OrderedOpenFailed {
            failure: PlatformFailure::new(crate::FailureKind::Platform, "no effect"),
            ownership: crate::OrderedOpenOwnership::NotCreated,
        });
        assert!(
            radio
                .acknowledge_ordered_open_locked("p", 17)
                .await
                .is_err()
        );
        assert!(!radio.release_retained_ordered_open(17));
        assert!(
            requests.try_recv().is_err(),
            "v2 owner received a legacy request"
        );
    }

    #[tokio::test]
    async fn legacy_admission_requires_a_negotiated_v1_owner() {
        struct Unnegotiated(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Unnegotiated {
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Unnegotiated(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let call = tokio::spawn(async move {
            radio
                .call(|id| RadioRequest::OrderedRecoverOpen {
                    id,
                    group_id: 7,
                    open_request_id: 9,
                    peer_id: "p".into(),
                })
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "unnegotiated owner received legacy recovery"
        );
        assert_eq!(
            call.await.unwrap().unwrap_err().code(),
            BleErrorCode::ProtocolIncompatible
        );
    }

    #[tokio::test]
    async fn v2_admission_requires_the_exact_negotiated_owner() {
        struct V2Owner(
            tokio::sync::mpsc::UnboundedSender<RadioRequest>,
            crate::OrderedAdmissionOwner,
        );
        impl PlatformRadio for V2Owner {
            fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
                Some(self.1)
            }
            fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
                Some(crate::OrderedAdmissionNegotiation {
                    version: 2,
                    owner: self.1,
                })
            }
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789ac1")
                .unwrap();
        let other =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789ac2")
                .unwrap();
        let radio = ForeignRadio::new(
            Arc::new(V2Owner(sender, owner)),
            MobilePlatform::Android,
            "test".into(),
        );
        let call = tokio::spawn(async move {
            radio
                .call(|id| {
                    RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Recover {
                        id,
                        identity: crate::OrderedOpenIdentity {
                            version: 2,
                            owner_incarnation: other,
                            slot: 0,
                            slot_generation: 1,
                            original_request_id: u64::MAX,
                            native_group_id: 8,
                            peer_id: "p".into(),
                            spec: crate::OrderedOpenSpec {
                                connection_generation: "c".into(),
                                database_generation: "d".into(),
                                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                                    owner: "native".into(),
                                    attachment: 1,
                                    revision: 1,
                                },
                                members: vec![],
                                item_capacity: 4,
                                byte_capacity: 64,
                            },
                        },
                    })
                })
                .await
        });
        tokio::select! {
            request = rx.recv() => panic!("request for another v2 owner was submitted: {request:?}"),
            result = call => assert_eq!(result.unwrap().unwrap_err().code(), BleErrorCode::ProtocolIncompatible),
        }
    }

    #[tokio::test]
    async fn malformed_v2_slot_identity_is_rejected_before_submit() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v2!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let call = tokio::spawn(async move {
            radio
                .call(|id| {
                    RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open {
                        id,
                        identity: crate::OrderedOpenIdentity {
                            version: 2,
                            owner_incarnation: owner,
                            slot: 0,
                            slot_generation: 0,
                            original_request_id: id,
                            native_group_id: 8,
                            peer_id: "p".into(),
                            spec: crate::OrderedOpenSpec {
                                connection_generation: "c".into(),
                                database_generation: "d".into(),
                                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                                    owner: "native".into(),
                                    attachment: 1,
                                    revision: 1,
                                },
                                members: vec![crate::OrderedMember {
                                    member_id: 3,
                                    instance: instance_of("p", "s", 0, "c", 0),
                                }],
                                item_capacity: 4,
                                byte_capacity: 64,
                            },
                        },
                    })
                })
                .await
        });
        tokio::select! {
            request = rx.recv() => panic!("invalid slot identity was submitted: {request:?}"),
            result = call => assert_eq!(result.unwrap().unwrap_err().code(), BleErrorCode::ArgumentInvalid),
        }
    }

    #[tokio::test]
    async fn v2_wire_attempt_must_match_registered_request_id() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v2!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let call = tokio::spawn(async move {
            radio
                .call(|id| {
                    RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open {
                        id: id + 1,
                        identity: crate::OrderedOpenIdentity {
                            version: 2,
                            owner_incarnation: owner,
                            slot: 0,
                            slot_generation: 1,
                            original_request_id: id + 1,
                            native_group_id: 8,
                            peer_id: "p".into(),
                            spec: crate::OrderedOpenSpec {
                                connection_generation: "c".into(),
                                database_generation: "d".into(),
                                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                                    owner: "native".into(),
                                    attachment: 1,
                                    revision: 1,
                                },
                                members: vec![crate::OrderedMember {
                                    member_id: 3,
                                    instance: instance_of("p", "s", 0, "c", 0),
                                }],
                                item_capacity: 4,
                                byte_capacity: 64,
                            },
                        },
                    })
                })
                .await
        });
        tokio::select! {
            request = rx.recv() => panic!("unregistered attempt was submitted: {request:?}"),
            result = call => assert_eq!(result.unwrap().unwrap_err().code(), BleErrorCode::ArgumentInvalid),
        }
    }

    #[tokio::test]
    async fn v2_open_rejects_mismatched_members_before_delivery() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v2!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let coordinator = radio.ordered_open_coordinator().unwrap().unwrap();
        let host = coordinator.register_host().unwrap();
        let mapping = coordinator.allocate_group(host, 8).unwrap();
        let task = {
            let radio = radio.clone();
            tokio::spawn(async move {
                radio
                    .call(|id| {
                        let spec = crate::OrderedOpenSpec {
                            connection_generation: "c".into(),
                            database_generation: "d".into(),
                            snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                                owner: "native".into(),
                                attachment: 1,
                                revision: 1,
                            },
                            members: vec![crate::OrderedMember {
                                member_id: 3,
                                instance: instance_of("p", "s", 0, "c", 0),
                            }],
                            item_capacity: 4,
                            byte_capacity: 64,
                        };
                        let lease = coordinator
                            .acquire(crate::ordered_open_slots::OpenRequest {
                                request_id: id,
                                group_id: mapping.native_group_id,
                                peer: "p".into(),
                                fingerprint: spec,
                                local_group: Some(mapping),
                            })
                            .unwrap();
                        let identity = coordinator.begin_dispatch_v2(&lease, owner).unwrap();
                        RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open {
                            id,
                            identity,
                        })
                    })
                    .await
            })
        };
        let request = rx.recv().await.unwrap();
        let RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { id, identity }) =
            request
        else {
            panic!("expected open")
        };
        // The production prepare path retains local ownership before dispatch.
        lock(&radio.shared.retained_ordered_opens).insert(
            mapping.collection_id,
            RetainedOrderedOpen {
                request_id: id,
                peer_id: identity.peer_id.clone(),
                connection_generation: identity.spec.connection_generation.clone(),
                database_generation: identity.spec.database_generation.clone(),
                members: identity.spec.members.clone(),
                completion: None,
                acknowledged: false,
            },
        );
        let completion = |member_id| {
            RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(
                crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: crate::OrderedOpenOutcomeV2::Opened {
                        source_generation: crate::OrderedSourceGeneration::Counter(1),
                        members: vec![crate::OrderedRegistration {
                            member_id,
                            source_member: 7,
                            epoch: 1,
                        }],
                    },
                },
            ))
        };
        assert_eq!(
            radio.complete(id, completion(4)),
            CompletionStatus::Mismatched
        );
        assert!(!task.is_finished());
        assert!(
            radio
                .retained_ordered_open(mapping.collection_id)
                .unwrap()
                .completion
                .is_none()
        );
        assert_eq!(
            radio.complete(id, completion(3)),
            CompletionStatus::Delivered
        );
        task.await.unwrap().unwrap();
        assert!(
            radio
                .retained_ordered_open(mapping.collection_id)
                .unwrap()
                .completion
                .is_some()
        );
    }

    #[tokio::test]
    async fn v2_retirement_rejects_wrong_slot_without_settling_waiter() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v2!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let owner =
            crate::OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let coordinator = radio.ordered_open_coordinator().unwrap().unwrap();
        let host = coordinator.register_host().unwrap();
        let mapping = coordinator.allocate_group(host, 8).unwrap();
        let spec = crate::OrderedOpenSpec {
            connection_generation: "c".into(),
            database_generation: "d".into(),
            snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "native".into(),
                attachment: 1,
                revision: 1,
            },
            members: vec![],
            item_capacity: 4,
            byte_capacity: 64,
        };
        let lease = coordinator
            .acquire(crate::ordered_open_slots::OpenRequest {
                request_id: u64::MAX,
                group_id: mapping.native_group_id,
                peer: "p".into(),
                fingerprint: spec,
                local_group: Some(mapping),
            })
            .unwrap();
        let identity = coordinator.begin_dispatch_v2(&lease, owner).unwrap();
        coordinator
            .retain_terminal(
                &lease,
                lease.identity,
                RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(
                    crate::OrderedOpenResultV2 {
                        slot_identity: identity.slot_identity(),
                        outcome: crate::OrderedOpenOutcomeV2::Failed {
                            original_failure: PlatformFailure::new(
                                crate::FailureKind::Platform,
                                "test",
                            ),
                            ownership: crate::OrderedOpenOwnershipV2::NotCreated,
                        },
                    },
                )),
            )
            .unwrap();
        let task = {
            let radio = radio.clone();
            let identity = identity.clone();
            tokio::spawn(async move {
                radio
                    .call(|id| {
                        RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge {
                            id,
                            identity,
                        })
                    })
                    .await
            })
        };
        let request = rx.recv().await.unwrap();
        let mut wrong = identity.slot_identity();
        wrong.slot_generation += 1;
        assert_eq!(
            radio.complete(
                request.id(),
                RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Retired(
                    crate::OrderedOpenRetired {
                        slot_identity: wrong
                    }
                ))
            ),
            CompletionStatus::Mismatched
        );
        assert!(!task.is_finished());
        assert_eq!(
            radio.complete(
                request.id(),
                RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Retired(
                    crate::OrderedOpenRetired {
                        slot_identity: identity.slot_identity()
                    }
                ))
            ),
            CompletionStatus::Delivered
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_open_retains_late_identity_until_host_adopts_it() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (requests, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(requests)),
            MobilePlatform::Apple,
            "test".into(),
        );
        let opener = radio.clone();
        let task = tokio::spawn(async move {
            opener
                .call(|id| RadioRequest::OrderedOpen {
                    id,
                    group_id: 17,
                    connection_generation: "connection".into(),
                    database_generation: "database".into(),
                    snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                        owner: "native".into(),
                        attachment: 1,
                        revision: 1,
                    },
                    members: vec![crate::OrderedMember {
                        member_id: 2,
                        instance: instance_of("p", "s", 0, "c", 0),
                    }],
                    item_capacity: 4,
                    byte_capacity: 64,
                })
                .await
        });
        let id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedOpen { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert!(!radio.release_retained_ordered_open(17));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            radio.complete(
                id,
                RadioCompletion::OrderedOpened {
                    generation: crate::OrderedSourceGeneration::Opaque("source".into()),
                    members: vec![crate::OrderedRegistration {
                        member_id: 2,
                        source_member: u64::MAX,
                        epoch: 1,
                    }],
                }
            ),
            CompletionStatus::Late
        );
        let retained = radio.retained_ordered_open(17).unwrap();
        assert_eq!(retained.request_id, id);
        let Some(RadioCompletion::OrderedOpened {
            generation,
            members,
        }) = retained.completion
        else {
            panic!("missing recovered native identity")
        };
        assert_eq!(
            radio.complete(
                id,
                RadioCompletion::OrderedOpened {
                    generation: crate::OrderedSourceGeneration::Opaque("conflicting".into()),
                    members: members.clone(),
                }
            ),
            CompletionStatus::Mismatched
        );
        assert!(
            matches!(radio.retained_ordered_open(17).unwrap().completion,
            Some(RadioCompletion::OrderedOpened { generation: ref current, .. }) if *current == generation)
        );
        let lease = radio.retained_lease("p", 17).unwrap().unwrap();
        let activation = ubm_desktop::boundary::PreparedNotificationActivation {
            scope: ("p".into(), "s".into(), 0, "c".into(), 0),
            activation_epoch: 3,
            requested: Some(DeliveryMode::Notification),
            preferred: None,
        };
        let membership = ubm_core::ordered_notifications::Membership {
            member: u64::MAX,
            epoch: 1,
        };
        assert_eq!(
            radio
                .validate_ordered_claim(&lease, 2, membership, &activation)
                .unwrap(),
            members[0]
        );
        let mut stale_source = lease.clone();
        stale_source.source_generation = crate::OrderedSourceGeneration::Opaque("older".into());
        assert!(
            radio
                .validate_ordered_claim(&stale_source, 2, membership, &activation)
                .is_err()
        );
        let mut stale_connection = lease.clone();
        stale_connection.connection_generation = "older".into();
        assert!(
            radio
                .validate_ordered_claim(&stale_connection, 2, membership, &activation)
                .is_err()
        );
        let mut wrong_activation = activation.clone();
        wrong_activation.scope.4 = 1;
        assert!(
            radio
                .validate_ordered_claim(&lease, 2, membership, &wrong_activation)
                .is_err()
        );
        let drainer = radio.clone();
        let drain_generation = generation.clone();
        let drain = tokio::spawn(async move {
            drainer
                .call(|id| RadioRequest::OrderedDrain {
                    id,
                    generation: drain_generation,
                    group_id: 17,
                })
                .await
        });
        let drain_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedDrain { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(
                drain_id,
                RadioCompletion::OrderedRecords {
                    batch_id: Some(1),
                    records: vec![crate::OrderedRecord::Value {
                        member: members[0],
                        ordinal: u64::MAX,
                        value: vec![7],
                        delivery: ObservedDelivery::Unknown,
                        provenance: crate::OrderedProvenance::Notification
                    }]
                }
            ),
            CompletionStatus::Delivered
        );
        assert!(
            matches!(drain.await.unwrap().unwrap(), RadioCompletion::OrderedRecords { records, .. } if matches!(&records[0], crate::OrderedRecord::Value { value, ordinal: u64::MAX, .. } if value == &[7]))
        );
        let acknowledger = radio.clone();
        let ack_generation = generation.clone();
        let ack = tokio::spawn(async move {
            acknowledger
                .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                    id,
                    generation: ack_generation,
                    group_id: 17,
                    batch_id: 1,
                })
                .await
        });
        let ack_id = rx.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(ack_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        ack.await.unwrap().unwrap();
        let closer = radio.clone();
        let close_generation = generation.clone();
        let close = tokio::spawn(async move {
            closer
                .call(|id| RadioRequest::OrderedClose {
                    id,
                    generation: close_generation,
                    group_id: 17,
                })
                .await
        });
        let close_id = rx.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(close_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        close.await.unwrap().unwrap();
        let terminal_radio = radio.clone();
        let terminal_generation = generation.clone();
        let terminal_drain = tokio::spawn(async move {
            terminal_radio
                .call(|id| RadioRequest::OrderedDrain {
                    id,
                    generation: terminal_generation,
                    group_id: 17,
                })
                .await
        });
        let terminal_id = rx.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(
                terminal_id,
                RadioCompletion::OrderedRecords {
                    batch_id: Some(2),
                    records: vec![crate::OrderedRecord::Terminal {
                        reason: crate::OrderedEnd::Closed,
                        uncertain_tail: true,
                        native_domain: None,
                        native_code: None,
                        failure_member: None,
                        rejected: None
                    }]
                }
            ),
            CompletionStatus::Delivered
        );
        assert!(matches!(
            terminal_drain.await.unwrap().unwrap(),
            RadioCompletion::OrderedRecords {
                batch_id: Some(2),
                ..
            }
        ));
        let terminal_ack_radio = radio.clone();
        let terminal_ack_generation = generation.clone();
        let terminal_ack = tokio::spawn(async move {
            terminal_ack_radio
                .call(|id| RadioRequest::OrderedAcknowledgeBatch {
                    id,
                    generation: terminal_ack_generation,
                    group_id: 17,
                    batch_id: 2,
                })
                .await
        });
        let terminal_ack_id = rx.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(terminal_ack_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        terminal_ack.await.unwrap().unwrap();
        let remover = radio.clone();
        let remove = tokio::spawn(async move {
            remover
                .call(|id| RadioRequest::OrderedRemove {
                    id,
                    generation,
                    group_id: 17,
                })
                .await
        });
        let remove_id = rx.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(remove_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        remove.await.unwrap().unwrap();
        lock(&radio.shared.retained_ordered_opens)
            .get_mut(&17)
            .unwrap()
            .acknowledged = true;
        assert!(radio.release_retained_ordered_open(17));
        assert!(radio.retained_ordered_open(17).is_none());
    }

    #[tokio::test]
    async fn invalid_ordered_open_is_rejected_before_native_dispatch() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (requests, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(requests)),
            MobilePlatform::Android,
            "test".into(),
        );
        let instance = instance_of("p", "s", 0, "c", 0);
        let error = radio
            .call(|id| RadioRequest::OrderedOpen {
                id,
                group_id: 1,
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "native".into(),
                    attachment: 1,
                    revision: 1,
                },
                members: vec![
                    crate::OrderedMember {
                        member_id: 1,
                        instance: instance.clone(),
                    },
                    crate::OrderedMember {
                        member_id: 2,
                        instance,
                    },
                ],
                item_capacity: 4,
                byte_capacity: 32,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), BleErrorCode::ArgumentInvalid);
        assert!(rx.try_recv().is_err());
        assert!(radio.retained_ordered_open(1).is_none());
        {
            let mut retained = lock(&radio.shared.retained_ordered_opens);
            for group_id in 0..u64::try_from(RETAINED_ORDERED_OPEN_CAP).unwrap() {
                retained.insert(
                    group_id,
                    RetainedOrderedOpen {
                        acknowledged: false,
                        request_id: group_id,
                        peer_id: "p".into(),
                        connection_generation: "connection".into(),
                        database_generation: "database".into(),
                        members: vec![crate::OrderedMember {
                            member_id: 1,
                            instance: instance_of("p", "s", 0, "c", 0),
                        }],
                        completion: None,
                    },
                );
            }
        }
        let error = radio
            .call(|id| RadioRequest::OrderedOpen {
                id,
                group_id: u64::MAX,
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "native".into(),
                    attachment: 1,
                    revision: 1,
                },
                members: vec![crate::OrderedMember {
                    member_id: 1,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                item_capacity: 4,
                byte_capacity: 32,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), BleErrorCode::StreamQuota);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelled_ordered_enable_keeps_debt_until_late_success_and_disable() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (requests, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(requests)),
            MobilePlatform::Android,
            "test".into(),
        );
        let instance = instance_of("p", "s", 0, "c", 0);
        let registration = crate::OrderedRegistration {
            member_id: 2,
            source_member: 3,
            epoch: 4,
        };
        let membership = ubm_core::ordered_notifications::Membership {
            member: 3,
            epoch: 4,
        };
        let generation = crate::OrderedSourceGeneration::Counter(1);
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 1,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance,
                }],
                completion: Some(RadioCompletion::OrderedOpened {
                    generation: generation.clone(),
                    members: vec![registration],
                }),
            },
        );
        let lease = radio.retained_lease("p", 17).unwrap().unwrap();
        let prepared = ubm_desktop::boundary::PreparedNotificationActivation {
            scope: ("p".into(), "s".into(), 0, "c".into(), 0),
            activation_epoch: 42,
            requested: Some(DeliveryMode::Notification),
            preferred: None,
        };
        let mut never_started = prepared.clone();
        never_started.activation_epoch = 41;
        radio
            .reserve_ordered_notification(&lease, 2, membership, &never_started)
            .unwrap();
        let unpolled = radio.dispatch_ordered_notification(&lease, 2, membership, &never_started);
        assert!(matches!(
            radio
                .ordered_notification_settlement(&lease, 2, membership, &never_started)
                .await
                .unwrap(),
            ubm_desktop::boundary::OrderedNotificationSettlement::Released
        ));
        assert!(unpolled.await.is_err());
        radio
            .release_ordered_notification(&lease, 2, membership, &never_started)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
        radio
            .reserve_ordered_notification(&lease, 2, membership, &prepared)
            .unwrap();
        assert_eq!(
            radio
                .ordered_release(&ubm_desktop::boundary::OrderedSourceCleanup::Lease(
                    lease.clone()
                ))
                .await
                .unwrap(),
            ubm_desktop::boundary::OrderedSourceRelease::Pending
        );
        assert!(
            rx.try_recv().is_err(),
            "origin source must remain pinned while the physical receipt is owned"
        );
        let worker = radio.clone();
        let activation = prepared.clone();
        let active_lease = lease.clone();
        let enable = tokio::spawn(async move {
            worker
                .dispatch_ordered_notification(&active_lease, 2, membership, &activation)
                .await
        });
        let enable_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedEnableNotifications { id, activation, .. }
                if activation.activation_epoch == 42 =>
            {
                id
            }
            other => panic!("{other:?}"),
        };
        enable.abort();
        assert!(enable.await.unwrap_err().is_cancelled());
        let worker = radio.clone();
        let activation = prepared.clone();
        let active_lease = lease.clone();
        let pending = tokio::spawn(async move {
            worker
                .release_ordered_notification(&active_lease, 2, membership, &activation)
                .await
        });
        let query_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedEnableSettlement { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(
                query_id,
                RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Pending)
            ),
            CompletionStatus::Delivered
        );
        assert_eq!(
            pending.await.unwrap().unwrap_err().code(),
            BleErrorCode::OperationTimedOut
        );
        assert!(
            rx.try_recv().is_err(),
            "disable cannot precede unresolved enable"
        );
        assert_eq!(
            radio.complete(enable_id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(
            lock(&radio.shared.retained_ordered_enables).by_request[&enable_id]
                .completion
                .is_none()
        );
        assert_eq!(
            radio.complete(
                enable_id,
                RadioCompletion::NotifyEnabled(ObservedDelivery::Notification)
            ),
            CompletionStatus::Late
        );
        let worker = radio.clone();
        let activation = prepared.clone();
        let active_lease = lease.clone();
        let release = tokio::spawn(async move {
            worker
                .release_ordered_notification(&active_lease, 2, membership, &activation)
                .await
        });
        let query_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedEnableSettlement { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(
                query_id,
                RadioCompletion::OrderedEnableStatus(crate::OrderedEnableStatus::Enabled(
                    ObservedDelivery::Notification
                ))
            ),
            CompletionStatus::Delivered
        );
        let disable_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedDisableNotifications { id, activation }
                if activation.activation_epoch == 42 =>
            {
                id
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(disable_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        let forget_id = match rx.recv().await.unwrap() {
            RadioRequest::OrderedForgetEnableReceipt { id, .. } => id,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            radio.complete(forget_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        release.await.unwrap().unwrap();
        assert!(
            radio
                .release_ordered_notification(&lease, 2, membership, &prepared)
                .await
                .is_ok()
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn old_disable_does_not_take_new_recording_fence() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (requests, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(requests)),
            MobilePlatform::Android,
            "test".into(),
        );
        let instance = instance_of("peer", "service", 0, "characteristic", 0);
        let (resume, mut released) = oneshot::channel();
        let guard = radio
            .stage_recorded_disable(instance.clone(), 2, IngressFence(Some(resume)))
            .unwrap();
        let old_radio = radio.clone();
        let old = tokio::spawn(async move {
            old_radio
                .set_notifications("peer", "service", 0, "characteristic", 0, false, 1, None)
                .await
        });
        let old_id = match rx.recv().await.unwrap() {
            RadioRequest::DisableNotifications { id, epoch: 1, .. } => id,
            other => panic!("unexpected old request: {other:?}"),
        };
        assert!(
            matches!(
                released.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "old activation must not release the new fence"
        );
        let new_radio = radio.clone();
        let new = tokio::spawn(async move {
            new_radio
                .set_notifications("peer", "service", 0, "characteristic", 0, false, 2, None)
                .await
        });
        let new_id = match rx.recv().await.unwrap() {
            RadioRequest::DisableNotifications { id, epoch: 2, .. } => id,
            other => panic!("unexpected new request: {other:?}"),
        };
        assert!(released.await.is_ok());
        assert_eq!(
            radio.complete(old_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), old)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            radio.complete(new_id, RadioCompletion::Unit),
            CompletionStatus::Delivered
        );
        let ingress_radio = radio.clone();
        let ingress = tokio::spawn(async move { ingress_radio.next_event().await });
        tokio::time::timeout(std::time::Duration::from_secs(2), new)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(guard);
        ingress.abort();
    }

    #[tokio::test]
    async fn receipt_notification_uses_bounded_data_queue_and_releases_bytes() {
        let radio = radio();
        let event = RadioEvent::NotificationWithReceipt {
            peer_id: "peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 0,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 0,
            epoch: 1,
            value: vec![1, 2, 3],
            receipt: ubm_core::central::NotificationReceipt {
                received_at_utc_ms: 1.0,
                received_at_monotonic_ms: 2.0,
                clock_id: "clock".into(),
            },
        };
        radio.push_event(event).unwrap();
        {
            let ingress = lock(&radio.shared.ingress);
            assert_eq!(ingress.data.len(), 1);
            assert_eq!(ingress.data_bytes, 3);
            assert!(ingress.control.is_empty());
        }
        assert!(matches!(
            radio.next_event().await,
            Some(RadioEvent::NotificationWithReceipt { .. })
        ));
        assert_eq!(lock(&radio.shared.ingress).data_bytes, 0);
    }

    #[tokio::test]
    async fn acknowledged_source_failure_joins_existing_global_fifo() {
        let radio = radio();
        let before = RadioEvent::Notification {
            peer_id: "original-peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 2,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 3,
            epoch: 42,
            value: vec![1, 2],
        };
        let failure = RadioEvent::NotificationSourceFailed {
            peer_id: "original-peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 2,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 3,
            epoch: 42,
            error: DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Platform,
                "gatt.notification.source",
            )
            .with_detail("exact"),
        };
        let after = RadioEvent::Disconnected("after".into());
        radio.push_event(before.clone()).unwrap();
        assert_eq!(
            radio.try_push_source_failure(failure.clone()),
            SourceFailureQueueStatus::Accepted
        );
        radio.push_event(after.clone()).unwrap();
        assert_eq!(radio.next_event().await, Some(before));
        assert_eq!(radio.next_event().await, Some(failure));
        assert_eq!(radio.next_event().await, Some(after));
        radio.close_events();
        assert_eq!(
            radio.try_push_source_failure(RadioEvent::Connected("late".into())),
            SourceFailureQueueStatus::Closed
        );
    }

    #[tokio::test]
    async fn accepted_source_failure_waits_behind_fence_and_survives_event_close() {
        let radio = radio();
        let consumer = radio.clone();
        let fence_radio = radio.clone();
        let fence_task = tokio::spawn(async move { fence_radio.fence_ingress().await.unwrap() });
        while lock(&radio.shared.ingress).barriers.is_empty() {
            tokio::task::yield_now().await;
        }
        let failure = RadioEvent::NotificationSourceFailed {
            peer_id: "old-owner".into(),
            service_uuid: "service".into(),
            service_occurrence: 1,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 2,
            epoch: 7,
            error: DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Platform,
                "gatt.notification.source",
            )
            .with_detail("original"),
        };
        assert_eq!(
            radio.try_push_source_failure(failure.clone()),
            SourceFailureQueueStatus::Accepted
        );
        radio.close_events();
        let next = tokio::spawn(async move { consumer.next_event().await });
        let fence = fence_task.await.unwrap();
        assert!(!next.is_finished());
        drop(fence);
        assert_eq!(next.await.unwrap(), Some(failure));
        assert_eq!(radio.next_event().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_companion_remains_owned_until_native_terminal_and_is_session_scoped() {
        #[derive(Default)]
        struct HeldPicker(Mutex<Vec<RequestId>>);
        impl PlatformRadio for HeldPicker {
            fn submit(&self, _: RadioRequest) {}
            fn cancel(&self, id: RequestId) {
                lock(&self.0).push(id);
            }
        }
        let picker = Arc::new(HeldPicker::default());
        let radio = ForeignRadio::new(picker.clone(), MobilePlatform::Android, "test".into());
        let mut choice =
            Box::pin(
                radio.call_owned_companion(7, |id| RadioRequest::AssociateCompanion {
                    id,
                    name: None,
                    service_uuid: None,
                    filters_json: None,
                }),
            );
        std::future::poll_fn(|cx| {
            assert!(choice.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(choice);
        assert_eq!(*lock(&picker.0), vec![1]);
        assert!(radio.release_companion_choices(8).await.is_ok());
        assert_eq!(
            *lock(&picker.0),
            vec![1],
            "unrelated session cannot cancel this UI"
        );
        assert!(radio.release_companion_choices(7).await.is_err());
        assert_eq!(
            *lock(&picker.0),
            vec![1, 1],
            "destroy retries the exact native owner"
        );
        assert_eq!(
            radio.complete(1, RadioCompletion::Unit),
            CompletionStatus::Late
        );
        assert!(radio.release_companion_choices(7).await.is_err());
        radio.complete(
            1,
            RadioCompletion::Failed(PlatformFailure::new(
                crate::radio::FailureKind::Cancelled,
                "native picker closed",
            )),
        );
        assert!(radio.release_companion_choices(7).await.is_ok());
    }

    /// The connect-section table keeps one entry per peer ever seen: the
    /// count tracks distinct peers and stays flat when the same peers
    /// reconnect, so a soak test can tell growth from garbage peer ids.
    #[tokio::test]
    async fn connect_section_count_tracks_distinct_peers() {
        let radio = radio();
        assert_eq!(radio.counters().connect_sections, 0);
        let ctl = OpControl::unbounded();
        let _first = radio
            .lock_connect_section("peer-a", &ctl)
            .await
            .expect("section");
        let _second = radio
            .lock_connect_section("peer-b", &ctl)
            .await
            .expect("section");
        assert_eq!(radio.counters().connect_sections, 2);
        drop(_first);
        drop(_second);
        let _repeat = radio
            .lock_connect_section("peer-a", &ctl)
            .await
            .expect("section");
        assert_eq!(radio.counters().connect_sections, 2);
    }

    #[tokio::test]
    async fn mismatched_live_ordered_completions_leave_waiters_and_exact_receipts_open() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let opener = radio.clone();
        let open = tokio::spawn(async move {
            opener
                .call(|id| RadioRequest::OrderedOpen {
                    id,
                    group_id: 17,
                    connection_generation: "connection".into(),
                    database_generation: "database".into(),
                    snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                        owner: "native".into(),
                        attachment: 1,
                        revision: 1,
                    },
                    members: vec![crate::OrderedMember {
                        member_id: 2,
                        instance: instance_of("p", "s", 0, "c", 0),
                    }],
                    item_capacity: 4,
                    byte_capacity: 64,
                })
                .await
        });
        let open_id = requests.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(open_id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(!open.is_finished());
        assert!(
            radio
                .retained_ordered_open(17)
                .unwrap()
                .completion
                .is_none()
        );
        let registration = crate::OrderedRegistration {
            member_id: 2,
            source_member: 3,
            epoch: 1,
        };
        let generation = crate::OrderedSourceGeneration::Counter(9);
        let opened = RadioCompletion::OrderedOpened {
            generation: generation.clone(),
            members: vec![registration],
        };
        assert_eq!(
            radio.complete(open_id, opened.clone()),
            CompletionStatus::Delivered
        );
        assert_eq!(open.await.unwrap().unwrap(), opened);
        assert_eq!(
            radio
                .retained_lease("p", 17)
                .unwrap()
                .unwrap()
                .source_generation,
            generation
        );

        let activation = crate::OrderedActivation {
            instance: instance_of("p", "s", 0, "c", 0),
            connection_generation: "connection".into(),
            source_generation: generation.clone(),
            group_id: 17,
            member: registration,
            activation_epoch: 42,
        };
        lock(&radio.shared.retained_ordered_enables)
            .reserved
            .insert(activation.clone());
        let worker = radio.clone();
        let enable_activation = activation.clone();
        let enable = tokio::spawn(async move {
            worker
                .call_ordered_enable(|id| RadioRequest::OrderedEnableNotifications {
                    id,
                    activation: enable_activation,
                    requested: None,
                    preferred: None,
                })
                .await
        });
        let enable_id = requests.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(enable_id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(!enable.is_finished());
        assert!(
            lock(&radio.shared.retained_ordered_enables).by_request[&enable_id]
                .completion
                .is_none()
        );
        let enabled = RadioCompletion::NotifyEnabled(ObservedDelivery::Notification);
        assert_eq!(
            radio.complete(enable_id, enabled.clone()),
            CompletionStatus::Delivered
        );
        assert_eq!(enable.await.unwrap().unwrap(), enabled);
        assert_eq!(
            lock(&radio.shared.retained_ordered_enables).by_request[&enable_id].completion,
            Some(enabled)
        );

        let worker = radio.clone();
        let register_generation = generation.clone();
        let register = tokio::spawn(async move {
            worker
                .call_ordered_register(|id| RadioRequest::OrderedRegister {
                    id,
                    generation: register_generation,
                    group_id: 17,
                    member: registration,
                })
                .await
        });
        let register_id = requests.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(register_id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(!register.is_finished());
        assert!(
            lock(&radio.shared.retained_ordered_registers).by_request[&register_id]
                .completion
                .is_none()
        );
        let registered = RadioCompletion::OrderedRegistered(crate::OrderedRegistration {
            member_id: 2,
            source_member: 4,
            epoch: 2,
        });
        assert_eq!(
            radio.complete(register_id, registered.clone()),
            CompletionStatus::Delivered
        );
        assert_eq!(register.await.unwrap().unwrap(), registered);
        assert_eq!(
            lock(&radio.shared.retained_ordered_registers).by_request[&register_id].completion,
            Some(registered)
        );
        assert_eq!(radio.counters().mismatched_completions, 3);
    }

    #[tokio::test]
    async fn mismatched_late_enable_and_register_do_not_replace_native_receipts() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let generation = crate::OrderedSourceGeneration::Counter(9);
        let member = crate::OrderedRegistration {
            member_id: 2,
            source_member: 3,
            epoch: 1,
        };
        let activation = crate::OrderedActivation {
            instance: instance_of("p", "s", 0, "c", 0),
            connection_generation: "connection".into(),
            source_generation: generation.clone(),
            group_id: 17,
            member,
            activation_epoch: 42,
        };
        lock(&radio.shared.retained_ordered_enables)
            .reserved
            .insert(activation.clone());
        let worker = radio.clone();
        let enable = tokio::spawn(async move {
            worker
                .call_ordered_enable(|id| RadioRequest::OrderedEnableNotifications {
                    id,
                    activation,
                    requested: None,
                    preferred: None,
                })
                .await
        });
        let enable_id = requests.recv().await.unwrap().id();
        enable.abort();
        assert!(enable.await.unwrap_err().is_cancelled());
        assert_eq!(
            radio.complete(enable_id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(
            lock(&radio.shared.retained_ordered_enables).by_request[&enable_id]
                .completion
                .is_none()
        );
        let enabled = RadioCompletion::NotifyEnabled(ObservedDelivery::Notification);
        assert_eq!(
            radio.complete(enable_id, enabled.clone()),
            CompletionStatus::Late
        );
        assert_eq!(
            lock(&radio.shared.retained_ordered_enables).by_request[&enable_id].completion,
            Some(enabled)
        );

        let worker = radio.clone();
        let register = tokio::spawn(async move {
            worker
                .call_ordered_register(|id| RadioRequest::OrderedRegister {
                    id,
                    generation,
                    group_id: 17,
                    member,
                })
                .await
        });
        let register_id = requests.recv().await.unwrap().id();
        register.abort();
        assert!(register.await.unwrap_err().is_cancelled());
        assert_eq!(
            radio.complete(register_id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(
            lock(&radio.shared.retained_ordered_registers).by_request[&register_id]
                .completion
                .is_none()
        );
        let registered = RadioCompletion::OrderedRegistered(crate::OrderedRegistration {
            member_id: 2,
            source_member: 4,
            epoch: 2,
        });
        assert_eq!(
            radio.complete(register_id, registered.clone()),
            CompletionStatus::Late
        );
        assert_eq!(
            lock(&radio.shared.retained_ordered_registers).by_request[&register_id].completion,
            Some(registered)
        );
        assert_eq!(radio.counters().mismatched_completions, 2);
    }

    #[tokio::test]
    async fn open_acknowledgement_debt_survives_cancel_and_retries_exact_identity() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            u64::MAX,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: u64::MAX - 1,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::OrderedOpenFailed {
                    failure: PlatformFailure::new(crate::FailureKind::Platform, "not created"),
                    ownership: crate::OrderedOpenOwnership::NotCreated,
                }),
            },
        );
        assert!(!radio.release_retained_ordered_open(u64::MAX));
        let worker = radio.clone();
        let attempt =
            tokio::spawn(
                async move { worker.acknowledge_ordered_open_locked("p", u64::MAX).await },
            );
        let first = requests.recv().await.unwrap();
        assert!(matches!(first, RadioRequest::OrderedAcknowledgeOpen {
            group_id: u64::MAX, open_request_id, ref peer_id, ..
        } if open_request_id == u64::MAX - 1 && peer_id == "p"));
        attempt.abort();
        assert!(attempt.await.unwrap_err().is_cancelled());
        assert!(!radio.release_retained_ordered_open(u64::MAX));
        let worker = radio.clone();
        let retry =
            tokio::spawn(
                async move { worker.acknowledge_ordered_open_locked("p", u64::MAX).await },
            );
        let second = requests.recv().await.unwrap();
        assert_ne!(first.id(), second.id());
        assert!(matches!(second, RadioRequest::OrderedAcknowledgeOpen {
            group_id: u64::MAX, open_request_id, ref peer_id, ..
        } if open_request_id == u64::MAX - 1 && peer_id == "p"));
        radio.complete(second.id(), RadioCompletion::Unit);
        retry.await.unwrap().unwrap();
        assert!(radio.release_retained_ordered_open(u64::MAX));
    }

    #[tokio::test(start_paused = true)]
    async fn lost_open_ack_reply_times_out_without_retiring_debt() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::OrderedOpenFailed {
                    failure: PlatformFailure::new(crate::FailureKind::Platform, "no effect"),
                    ownership: crate::OrderedOpenOwnership::NotCreated,
                }),
            },
        );
        let worker = radio.clone();
        let attempt =
            tokio::spawn(async move { worker.acknowledge_ordered_open_locked("p", 17).await });
        let _lost = requests.recv().await.unwrap();
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), attempt)
                .await
                .expect("lost acknowledgement must time out")
                .unwrap()
                .is_err()
        );
        assert!(!radio.release_retained_ordered_open(17));
        let worker = radio.clone();
        let retry =
            tokio::spawn(async move { worker.acknowledge_ordered_open_locked("p", 17).await });
        let request = requests.recv().await.unwrap();
        radio.complete(request.id(), RadioCompletion::Unit);
        retry.await.unwrap().unwrap();
        assert!(radio.release_retained_ordered_open(17));
    }

    #[tokio::test(start_paused = true)]
    async fn lost_open_recovery_reply_keeps_exact_identity_retryable() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: None,
            },
        );
        let worker = radio.clone();
        let attempt = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let _lost = requests.recv().await.unwrap();
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert!(attempt.await.unwrap().is_err());
        assert!(radio.retained_ordered_open(17).is_some());
        let worker = radio.clone();
        let retry = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let request = requests.recv().await.unwrap();
        assert!(
            matches!(request, RadioRequest::OrderedRecoverOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
        );
        radio.complete(
            request.id(),
            RadioCompletion::OrderedOpenFailed {
                failure: PlatformFailure::new(crate::FailureKind::Platform, "pending"),
                ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key: 17 },
            },
        );
        assert!(matches!(
            retry.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Pending
        ));
    }

    #[tokio::test]
    async fn terminal_open_receipts_are_retained_until_ack_succeeds() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Apple,
            "test".into(),
        );
        let member = crate::OrderedRegistration {
            member_id: 2,
            source_member: u64::MAX,
            epoch: 1 << 63,
        };
        let failure = PlatformFailure::new(crate::FailureKind::Platform, "original failure");
        let terminal = [
            RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Counter(u64::MAX),
                members: vec![member],
            },
            RadioCompletion::OrderedOpenFailed {
                failure: failure.clone(),
                ownership: crate::OrderedOpenOwnership::Created {
                    generation: crate::OrderedSourceGeneration::Counter(3),
                    members: vec![],
                },
            },
            RadioCompletion::OrderedOpenFailed {
                failure: failure.clone(),
                ownership: crate::OrderedOpenOwnership::NotCreated,
            },
        ];
        for (index, completion) in terminal.into_iter().enumerate() {
            let group = 20 + index as u64;
            lock(&radio.shared.retained_ordered_opens).insert(
                group,
                RetainedOrderedOpen {
                    acknowledged: false,
                    request_id: 99 + group,
                    peer_id: "p".into(),
                    connection_generation: "c".into(),
                    database_generation: "d".into(),
                    members: vec![crate::OrderedMember {
                        member_id: 2,
                        instance: instance_of("p", "s", 0, "c", 0),
                    }],
                    completion: Some(completion.clone()),
                },
            );
            assert!(
                radio
                    .acknowledge_ordered_open_locked("other", group)
                    .await
                    .is_err()
            );
            assert!(requests.try_recv().is_err());
            let worker = radio.clone();
            let attempt =
                tokio::spawn(
                    async move { worker.acknowledge_ordered_open_locked("p", group).await },
                );
            let request = requests.recv().await.unwrap();
            radio.complete(request.id(), RadioCompletion::Failed(failure.clone()));
            assert!(attempt.await.unwrap().is_err());
            assert_eq!(
                radio.retained_ordered_open(group).unwrap().completion,
                Some(completion)
            );
            assert!(!radio.release_retained_ordered_open(group));
            let worker = radio.clone();
            let retry =
                tokio::spawn(
                    async move { worker.acknowledge_ordered_open_locked("p", group).await },
                );
            let request = requests.recv().await.unwrap();
            radio.complete(request.id(), RadioCompletion::Unit);
            retry.await.unwrap().unwrap();
            assert!(radio.release_retained_ordered_open(group));
        }
    }

    #[tokio::test]
    async fn replacing_a_radio_does_not_reuse_request_identity() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let first = ForeignRadio::new(
            Arc::new(Holding(sender.clone())),
            MobilePlatform::Android,
            "first".into(),
        );
        let second = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "second".into(),
        );
        let task1 =
            tokio::spawn(async move { first.call(|id| RadioRequest::AdapterState { id }).await });
        let id1 = requests.recv().await.unwrap().id();
        let task2 =
            tokio::spawn(async move { second.call(|id| RadioRequest::AdapterState { id }).await });
        let id2 = requests.recv().await.unwrap().id();
        assert_ne!(id1, id2);
        task1.abort();
        task2.abort();
    }

    #[tokio::test]
    async fn recovered_no_effect_remains_owned_until_acknowledged() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        struct NoWake;
        impl crate::WakeSink for NoWake {
            fn wake(&self, _: u64) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let host = crate::MobileHost::open(
            Arc::new(Holding(sender)),
            Arc::new(NoWake),
            crate::HostOptions {
                platform: MobilePlatform::Android,
                owner: "test".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap();
        let radio = host.inner.radio.clone();
        host.inner.ordered_wakes.begin(17).unwrap();
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: None,
            },
        );
        let worker = radio.clone();
        let first = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let request = requests.recv().await.unwrap();
        assert!(
            matches!(request, RadioRequest::OrderedRecoverOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
        );
        let failure = PlatformFailure::new(crate::FailureKind::Platform, "no effect");
        radio.complete(
            request.id(),
            RadioCompletion::OrderedOpenFailed {
                failure: failure.clone(),
                ownership: crate::OrderedOpenOwnership::NotCreated,
            },
        );
        let ack = requests.recv().await.unwrap();
        radio.complete(ack.id(), RadioCompletion::Failed(failure));
        assert!(first.await.unwrap().is_err());
        assert!(radio.retained_ordered_open(17).is_some());
        let worker = radio.clone();
        let retry = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let ack = requests.recv().await.unwrap();
        assert!(matches!(
            ack,
            RadioRequest::OrderedAcknowledgeOpen {
                version: crate::ORDERED_ACKNOWLEDGE_OPEN_VERSION,
                group_id: 17,
                open_request_id: 99,
                ..
            }
        ));
        radio.complete(ack.id(), RadioCompletion::Unit);
        assert!(matches!(
            retry.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::NotStarted
        ));
        assert!(radio.retained_ordered_open(17).is_none());
    }

    #[tokio::test]
    async fn stale_cleanup_cannot_acknowledge_a_new_source() {
        let radio = radio();
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::OrderedOpened {
                    generation: crate::OrderedSourceGeneration::Counter(2),
                    members: vec![crate::OrderedRegistration {
                        member_id: 2,
                        source_member: 3,
                        epoch: 4,
                    }],
                }),
            },
        );
        let mut old = radio.retained_lease("p", 17).unwrap().unwrap();
        old.source_generation = crate::OrderedSourceGeneration::Counter(1);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            radio.ordered_release(&ubm_desktop::boundary::OrderedSourceCleanup::Lease(old)),
        )
        .await;
        assert!(
            result
                .expect("stale cleanup must not submit acknowledgement")
                .is_err()
        );
        assert!(!radio.retained_ordered_open(17).unwrap().acknowledged);
    }

    #[tokio::test]
    async fn concurrent_recoveries_share_terminal_acknowledgement() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::OrderedOpened {
                    generation: crate::OrderedSourceGeneration::Counter(1),
                    members: vec![crate::OrderedRegistration {
                        member_id: 2,
                        source_member: 3,
                        epoch: 4,
                    }],
                }),
            },
        );
        let a = radio.clone();
        let first = tokio::spawn(async move { a.recover_ordered_source("p", 17).await });
        let request = requests.recv().await.unwrap();
        let b = radio.clone();
        let second = tokio::spawn(async move { b.recover_ordered_source("p", 17).await });
        tokio::task::yield_now().await;
        assert!(requests.try_recv().is_err());
        radio.complete(request.id(), RadioCompletion::Unit);
        assert!(matches!(
            first.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
        ));
        assert!(matches!(
            second.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
        ));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn unresolved_open_is_never_acknowledged_as_no_effect() {
        let radio = radio();
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "c".into(),
                database_generation: "d".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::Failed(PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "unknown",
                ))),
            },
        );
        assert!(
            radio
                .acknowledge_ordered_open_locked("p", 17)
                .await
                .is_err()
        );
        assert!(!radio.release_retained_ordered_open(17));
    }

    #[test]
    fn generic_open_failure_does_not_prove_native_source_absence() {
        let radio = radio();
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 1,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::Failed(PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "completion without an ownership result",
                ))),
            },
        );
        assert!(radio.retained_lease("p", 17).unwrap().is_none());
        assert!(radio.retained_ordered_open(17).is_some());
    }

    #[test]
    fn late_exact_open_replaces_generic_failure_without_changing_the_caller_error() {
        let radio = radio();
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 1,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::Failed(PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "unreadable completion",
                ))),
            },
        );
        let opened = RadioCompletion::OrderedOpened {
            generation: crate::OrderedSourceGeneration::Counter(9),
            members: vec![crate::OrderedRegistration {
                member_id: 2,
                source_member: 3,
                epoch: 1,
            }],
        };
        assert_eq!(radio.complete(1, opened.clone()), CompletionStatus::Late);
        assert_eq!(
            radio.retained_ordered_open(17).unwrap().completion,
            Some(opened)
        );
        assert!(radio.retained_lease("p", 17).unwrap().is_some());
    }

    #[tokio::test]
    async fn late_open_is_not_overwritten_by_an_older_pending_recovery() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let failure = PlatformFailure::new(crate::FailureKind::Platform, "open uncertain");
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::OrderedOpenFailed {
                    failure: failure.clone(),
                    ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key: 17 },
                }),
            },
        );
        let worker = radio.clone();
        let recovery = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let _id = match requests.recv().await.unwrap() {
            RadioRequest::OrderedRecoverOpen {
                id,
                group_id: 17,
                open_request_id: 99,
                ref peer_id,
            } if peer_id == "p" => id,
            other => panic!("unexpected {other:?}"),
        };
        let opened = RadioCompletion::OrderedOpened {
            generation: crate::OrderedSourceGeneration::Counter(9),
            members: vec![crate::OrderedRegistration {
                member_id: 2,
                source_member: 3,
                epoch: 1,
            }],
        };
        assert_eq!(radio.complete(99, opened.clone()), CompletionStatus::Late);
        let ack = tokio::time::timeout(std::time::Duration::from_secs(1), requests.recv())
            .await
            .expect("late original result must not wait for a lost recovery reply")
            .unwrap();
        assert!(
            matches!(ack, RadioRequest::OrderedAcknowledgeOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
        );
        radio.complete(ack.id(), RadioCompletion::Unit);
        assert!(matches!(
            recovery.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
        ));
        assert_eq!(
            radio.retained_ordered_open(17).unwrap().completion,
            Some(opened)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn recovery_cannot_pass_an_open_still_entering_native_submit() {
        struct PausedSubmit {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
            recovery: tokio::sync::mpsc::UnboundedSender<(u64, RequestId)>,
        }
        impl PlatformRadio for PausedSubmit {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                match request {
                    RadioRequest::OrderedOpen { .. } => {
                        self.entered.send(()).unwrap();
                        self.release.lock().unwrap().recv().unwrap();
                    }
                    RadioRequest::OrderedRecoverOpen { id, group_id, .. } => {
                        self.recovery.send((group_id, id)).unwrap();
                    }
                    _ => panic!("unexpected request"),
                }
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (recovery_tx, mut recovery_rx) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(PausedSubmit {
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
                recovery: recovery_tx,
            }),
            MobilePlatform::Android,
            "test".into(),
        );
        let worker = radio.clone();
        let opening = tokio::spawn(async move {
            worker
                .call(|id| RadioRequest::OrderedOpen {
                    id,
                    group_id: 17,
                    connection_generation: "connection".into(),
                    database_generation: "database".into(),
                    snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                        owner: "native".into(),
                        attachment: 1,
                        revision: 1,
                    },
                    members: vec![crate::OrderedMember {
                        member_id: 2,
                        instance: instance_of("p", "s", 0, "c", 0),
                    }],
                    item_capacity: 4,
                    byte_capacity: 64,
                })
                .await
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let worker = radio.clone();
        let other = tokio::spawn(async move {
            worker
                .call(|id| RadioRequest::OrderedRecoverOpen {
                    id,
                    group_id: 18,
                    open_request_id: 123,
                    peer_id: "p".into(),
                })
                .await
        });
        let other_request =
            tokio::time::timeout(std::time::Duration::from_millis(100), recovery_rx.recv()).await;
        if other_request.is_err() {
            release_tx.send(()).unwrap();
        }
        let (other_key, other_id) = other_request
            .expect("unrelated group was blocked by open submission")
            .unwrap();
        assert_eq!(other_key, 18);
        radio.complete(
            other_id,
            RadioCompletion::OrderedOpenFailed {
                failure: PlatformFailure::new(crate::FailureKind::Platform, "pending"),
                ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key: 18 },
            },
        );
        assert!(other.await.unwrap().is_ok());
        let worker = radio.clone();
        let recovery = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(100), recovery_rx.recv()).await;
        release_tx.send(()).unwrap();
        assert!(
            early.is_err(),
            "recovery reached native before original submission completed"
        );
        let (key, id) = tokio::time::timeout(std::time::Duration::from_secs(2), recovery_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(key, 17);
        radio.complete(
            id,
            RadioCompletion::OrderedOpenFailed {
                failure: PlatformFailure::new(crate::FailureKind::Platform, "pending"),
                ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key: 17 },
            },
        );
        assert!(matches!(
            recovery.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Pending
        ));
        opening.abort();
    }

    #[tokio::test]
    async fn unresolved_open_recovers_by_its_retained_collection_id() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        for completion in [
            None,
            Some(RadioCompletion::Failed(PlatformFailure::new(
                crate::FailureKind::Platform,
                "unreadable completion",
            ))),
        ] {
            let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
            let radio = ForeignRadio::new(
                Arc::new(Holding(sender)),
                MobilePlatform::Android,
                "test".into(),
            );
            lock(&radio.shared.retained_ordered_opens).insert(
                17,
                RetainedOrderedOpen {
                    acknowledged: false,
                    request_id: 99,
                    peer_id: "p".into(),
                    connection_generation: "connection".into(),
                    database_generation: "database".into(),
                    members: vec![crate::OrderedMember {
                        member_id: 2,
                        instance: instance_of("p", "s", 0, "c", 0),
                    }],
                    completion,
                },
            );
            let worker = radio.clone();
            let recovery =
                tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
            let request =
                tokio::time::timeout(std::time::Duration::from_secs(1), requests.recv()).await;
            let id = match request.expect("recovery was not submitted").unwrap() {
                RadioRequest::OrderedRecoverOpen {
                    id,
                    group_id: 17,
                    open_request_id: 99,
                    ref peer_id,
                } if peer_id == "p" => id,
                other => panic!("unexpected {other:?}"),
            };
            radio.complete(
                id,
                RadioCompletion::OrderedOpened {
                    generation: crate::OrderedSourceGeneration::Counter(9),
                    members: vec![crate::OrderedRegistration {
                        member_id: 2,
                        source_member: 3,
                        epoch: 1,
                    }],
                },
            );
            let ack = requests.recv().await.unwrap();
            assert!(
                matches!(ack, RadioRequest::OrderedAcknowledgeOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
            );
            radio.complete(ack.id(), RadioCompletion::Unit);
            assert!(matches!(
                recovery.await.unwrap().unwrap(),
                ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
            ));
        }
    }

    #[tokio::test]
    async fn standalone_cancelled_open_and_recovery_retry_retire_no_effect() {
        struct Holding {
            requests: tokio::sync::mpsc::UnboundedSender<RadioRequest>,
            cancellations: tokio::sync::mpsc::UnboundedSender<RequestId>,
        }
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.requests.send(request).unwrap();
            }
            fn cancel(&self, id: RequestId) {
                self.cancellations.send(id).unwrap();
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (request_tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
            let (cancel_tx, mut cancellations) = tokio::sync::mpsc::unbounded_channel();
            let radio = ForeignRadio::new(Arc::new(Holding {
                requests: request_tx, cancellations: cancel_tx,
            }), MobilePlatform::Android, "test".into());
            let worker = radio.clone();
            let open = tokio::spawn(async move {
                worker.call(|id| RadioRequest::OrderedOpen {
                    id, group_id: 17, connection_generation: "c".into(), database_generation: "d".into(),
                    snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity { owner: "native".into(), attachment: 1, revision: 1 },
                    members: vec![crate::OrderedMember { member_id: 2, instance: instance_of("p", "s", 0, "c", 0) }],
                    item_capacity: 4, byte_capacity: 64,
                }).await
            });
            let open_id = requests.recv().await.unwrap().id();
            open.abort();
            assert!(open.await.unwrap_err().is_cancelled());
            assert_eq!(cancellations.recv().await, Some(open_id));
            let worker = radio.clone();
            let first = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
            let first_request = requests.recv().await.unwrap();
            assert!(matches!(first_request, RadioRequest::OrderedRecoverOpen { group_id: 17, open_request_id, .. } if open_request_id == open_id));
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
            assert_eq!(cancellations.recv().await, Some(first_request.id()));
            assert!(!radio.release_retained_ordered_open(17));
            let worker = radio.clone();
            let retry = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
            let request = requests.recv().await.unwrap();
            assert!(matches!(request, RadioRequest::OrderedRecoverOpen { group_id: 17, open_request_id, .. } if open_request_id == open_id));
            radio.complete(request.id(), RadioCompletion::OrderedOpenFailed {
                failure: PlatformFailure::new(crate::FailureKind::Platform, "no effect"),
                ownership: crate::OrderedOpenOwnership::NotCreated,
            });
            let ack = requests.recv().await.unwrap();
            assert!(matches!(ack, RadioRequest::OrderedAcknowledgeOpen { group_id: 17, open_request_id, .. } if open_request_id == open_id));
            assert!(!radio.release_retained_ordered_open(17));
            radio.complete(ack.id(), RadioCompletion::Unit);
            assert!(matches!(retry.await.unwrap().unwrap(), ubm_desktop::boundary::OrderedSourceRecovery::NotStarted));
            assert!(radio.retained_ordered_open(17).is_none());
        }).await.expect("standalone recovery did not complete");
    }

    #[tokio::test]
    async fn malformed_recovery_keeps_unresolved_receipt_retryable() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Apple,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::Failed(PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "unreadable",
                ))),
            },
        );
        let member = crate::OrderedRegistration {
            member_id: 2,
            source_member: 3,
            epoch: 1,
        };
        let failure = PlatformFailure::new(crate::FailureKind::Platform, "pending");
        let invalid = [
            RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Opaque(String::new()),
                members: vec![member],
            },
            RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Opaque("source".into()),
                members: vec![crate::OrderedRegistration {
                    member_id: 999,
                    ..member
                }],
            },
            RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Opaque("source".into()),
                members: vec![member, member],
            },
            RadioCompletion::OrderedOpenFailed {
                failure,
                ownership: crate::OrderedOpenOwnership::Indeterminate { recovery_key: 18 },
            },
        ];
        for completion in invalid {
            let worker = radio.clone();
            let attempt = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
            let id = requests.recv().await.unwrap().id();
            radio.complete(id, completion);
            assert!(attempt.await.unwrap().is_err());
        }
        let worker = radio.clone();
        let retry = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let id = tokio::time::timeout(std::time::Duration::from_secs(1), requests.recv())
            .await
            .expect("recovery was not retried")
            .unwrap()
            .id();
        radio.complete(
            id,
            RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Opaque("source".into()),
                members: vec![member],
            },
        );
        let ack = requests.recv().await.unwrap();
        assert!(
            matches!(ack, RadioRequest::OrderedAcknowledgeOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
        );
        radio.complete(ack.id(), RadioCompletion::Unit);
        assert!(matches!(
            retry.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
        ));
    }

    #[tokio::test]
    async fn malformed_late_open_does_not_prevent_recovery_retry() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Apple,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![crate::OrderedMember {
                    member_id: 2,
                    instance: instance_of("p", "s", 0, "c", 0),
                }],
                completion: Some(RadioCompletion::Failed(PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "unreadable",
                ))),
            },
        );
        assert_eq!(
            radio.complete(
                99,
                RadioCompletion::OrderedOpened {
                    generation: crate::OrderedSourceGeneration::Opaque(String::new()),
                    members: vec![crate::OrderedRegistration {
                        member_id: 2,
                        source_member: 3,
                        epoch: 1
                    }],
                }
            ),
            CompletionStatus::Mismatched
        );
        let worker = radio.clone();
        let retry = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let id = tokio::time::timeout(std::time::Duration::from_secs(1), requests.recv())
            .await
            .expect("recovery was not submitted")
            .unwrap()
            .id();
        radio.complete(
            id,
            RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Opaque("source".into()),
                members: vec![crate::OrderedRegistration {
                    member_id: 2,
                    source_member: 3,
                    epoch: 1,
                }],
            },
        );
        let ack = requests.recv().await.unwrap();
        assert!(
            matches!(ack, RadioRequest::OrderedAcknowledgeOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
        );
        radio.complete(ack.id(), RadioCompletion::Unit);
        assert!(matches!(
            retry.await.unwrap().unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
        ));
    }

    #[tokio::test]
    async fn recovered_partial_open_keeps_only_exact_installed_members() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Apple,
            "test".into(),
        );
        lock(&radio.shared.retained_ordered_opens).insert(
            17,
            RetainedOrderedOpen {
                acknowledged: false,
                request_id: 99,
                peer_id: "p".into(),
                connection_generation: "connection".into(),
                database_generation: "database".into(),
                members: vec![
                    crate::OrderedMember {
                        member_id: 2,
                        instance: instance_of("p", "s", 0, "c", 0),
                    },
                    crate::OrderedMember {
                        member_id: 3,
                        instance: instance_of("p", "s", 0, "d", 0),
                    },
                ],
                completion: Some(RadioCompletion::Failed(PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "unreadable",
                ))),
            },
        );
        let worker = radio.clone();
        let recovery = tokio::spawn(async move { worker.recover_ordered_source("p", 17).await });
        let id = requests.recv().await.unwrap().id();
        radio.complete(
            id,
            RadioCompletion::OrderedOpenFailed {
                failure: PlatformFailure::new(crate::FailureKind::Platform, "partial installation"),
                ownership: crate::OrderedOpenOwnership::Created {
                    generation: crate::OrderedSourceGeneration::Opaque("source".into()),
                    members: vec![crate::OrderedRegistration {
                        member_id: 2,
                        source_member: 3,
                        epoch: 1,
                    }],
                },
            },
        );
        let ack = requests.recv().await.unwrap();
        assert!(
            matches!(ack, RadioRequest::OrderedAcknowledgeOpen { group_id: 17, open_request_id: 99, ref peer_id, .. } if peer_id == "p")
        );
        radio.complete(ack.id(), RadioCompletion::Unit);
        let ubm_desktop::boundary::OrderedSourceRecovery::Ready(lease) =
            recovery.await.unwrap().unwrap()
        else {
            panic!("partial source not retained")
        };
        assert_eq!(lease.collection_id, 17);
        assert_eq!(lease.members.len(), 1);
        assert_eq!(lease.members[0].0, 2);
    }

    #[tokio::test]
    async fn mismatched_nonretained_completion_does_not_invent_native_refusal() {
        struct Holding(tokio::sync::mpsc::UnboundedSender<RadioRequest>);
        impl PlatformRadio for Holding {
            negotiated_v1!();
            fn submit(&self, request: RadioRequest) {
                self.0.send(request).unwrap();
            }
            fn cancel(&self, _: RequestId) {}
        }
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let radio = ForeignRadio::new(
            Arc::new(Holding(sender)),
            MobilePlatform::Android,
            "test".into(),
        );
        let worker = radio.clone();
        let read = tokio::spawn(async move {
            worker
                .call(|id| RadioRequest::ReadRssi {
                    id,
                    peer_id: "p".into(),
                })
                .await
        });
        let id = requests.recv().await.unwrap().id();
        assert_eq!(
            radio.complete(id, RadioCompletion::Unit),
            CompletionStatus::Mismatched
        );
        assert!(!read.is_finished());
        assert_eq!(
            radio.complete(id, RadioCompletion::Rssi(-42)),
            CompletionStatus::Delivered
        );
        assert_eq!(read.await.unwrap().unwrap(), RadioCompletion::Rssi(-42));
    }
}
