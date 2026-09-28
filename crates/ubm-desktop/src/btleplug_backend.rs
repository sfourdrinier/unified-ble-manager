//! Production btleplug backend for the desktop radio boundary.
//!
//! [`BtleplugRadio`] implements [`RadioBoundary`](crate::boundary::RadioBoundary)
//! over `btleplug::platform` (CoreBluetooth on macOS, WinRT on Windows,
//! BlueZ on Linux). It owns no scheduler and no operation map: it translates
//! calls and events, and the real [`ubm_core::central::Central`] keeps every
//! decision. Notification streams are fanned into one event channel by
//! per-subscription forwarder tasks spawned on the shared desktop executor
//! handle given to [`BtleplugRadio::open`] (never one runtime per manager).
//!
//! Radio facts btleplug cannot provide are reported honestly, never
//! synthesized: RSSI/addresses stay `None` when the OS withholds them, and
//! indications are indistinguishable from notifications on this stream (see
//! `PARITY_GAPS.md`): an enable reports [`ObservedDelivery::Unknown`] and a
//! hard delivery requirement is refused before any effect.
//!
//! GATT state is cached per connected peer (PR210-25): a verb reuses the
//! peripheral resolved for that connection and discovers services only when
//! the cached peripheral has none (BlueZ hands out a fresh, empty
//! peripheral per lookup; CoreBluetooth peripherals share their service
//! state). `discover` always rediscovers and refreshes the entry; the entry
//! is evicted on connect, disconnect, OS link loss, services-changed and
//! close.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{
    Arc, Mutex as StdMutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use btleplug::api::{
    Central as _, CentralEvent, CharPropFlags, Characteristic, Descriptor, Manager as _,
    Peripheral as _, ScanFilter, Service, ValueNotification,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::{FutureExt, StreamExt};
use tokio::sync::{Mutex, mpsc};

use crate::boundary::{
    AdapterAuthorization, AdapterPowerState, AddressType, CharacteristicAccess, CharacteristicRead,
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, GattSnapshotIdentity, InstanceKey,
    ManufacturerData, ObservedDelivery, PairOutcome, PeerSnapshot, PropertyFlags, RadioBoundary,
    RadioCloseFailure, RadioEvent, ScanFilterSpec, SecurityState, ServiceData, ServiceSnapshot,
    UnpairOutcome, WriteLimits,
};
use crate::delivery::{
    DeliveryPlan, os_answers_unflagged_subscribe, plan_delivery_for_os, platform_rule,
};
use crate::errors::DesktopError;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

/// Bound for releasing one characteristic scope during [`RadioBoundary::close`]
/// (lookup, discovery and native unsubscribe together). A scope that does not
/// finish inside it becomes a [`RadioCloseFailure`] receipt.
pub const CLOSE_SCOPE_BOUND: Duration = Duration::from_secs(5);

/// Event-stream drops that found the stream lock contended (PR210-26); see
/// [`drop_event_stream`].
static CONTENDED_RADIO_DROPS: AtomicU64 = AtomicU64::new(0);

/// How many radio drops found their event stream contended since process
/// start. Nonzero means a stream dropped with the radio instead of inside
/// the shared executor context.
#[must_use]
pub fn contended_radio_drops() -> u64 {
    CONTENDED_RADIO_DROPS.load(Ordering::Relaxed)
}

/// Outcome of [`drop_event_stream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropStream {
    /// The stream was taken and dropped inside a runtime context.
    Dropped,
    /// No stream was left to drop.
    Empty,
    /// The stream lock was held elsewhere: the stream was not taken and
    /// drops later with its owner. Counted and logged, never silent.
    Contended,
}

/// Take and drop the adapter event stream inside a runtime context (the
/// stream's own `Drop` spawns a task, and an off-runtime drop aborts the
/// host). Entering `spawn`'s context covers both a live executor (the
/// inner spawn schedules) and one shutting down (it degrades to a
/// cancelled task). A contended lock is reported, counted in
/// [`contended_radio_drops`] and logged, never passed over silently.
pub fn drop_event_stream<S>(slot: &Mutex<Option<S>>, spawn: &tokio::runtime::Handle) -> DropStream {
    let Ok(mut guard) = slot.try_lock() else {
        CONTENDED_RADIO_DROPS.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "ubm-desktop: radio dropped while its event stream was in use; \
             the stream drops with the radio, outside the shared executor context"
        );
        return DropStream::Contended;
    };
    let Some(stream) = guard.take() else {
        return DropStream::Empty;
    };
    drop(guard);
    if tokio::runtime::Handle::try_current().is_ok() {
        drop(stream);
    } else {
        let _context = spawn.enter();
        drop(stream);
    }
    DropStream::Dropped
}

/// One adapter the OS lists. `label` is the adapter's selectable identity
/// — what [`BtleplugRadio::open`] and `CentralProfile::adapter_id` select
/// by — or the error the OS returned instead (never a synthesized label):
/// the BlueZ adapter id (`hci0`) on Linux, the native adapter id
/// (`BluetoothAdapter` device id, as the legacy WinRT addon listed it) on
/// Windows, `CoreBluetooth` on macOS.
#[derive(Debug, Clone)]
pub struct AdapterListing {
    /// Position in the OS listing.
    pub index: usize,
    /// The adapter's selectable identity, or why it could not be read.
    pub label: Result<String, DesktopError>,
    /// The OS's descriptive name, when it gives one (BlueZ: the full
    /// `adapter_info` with its modalias; Windows: the device name).
    pub display_name: Option<String>,
    /// Whether this is the adapter a default open (`adapter_id: None`)
    /// selects: the sole adapter, or on Windows the default adapter. With
    /// several adapters and no default, an unnamed open is
    /// `adapter.ambiguous`. On Windows a non-default adapter opens too: its
    /// power, state changes and authorization are that adapter's, while LE
    /// traffic goes through the Windows stack, as the legacy addon did.
    pub default: bool,
    /// Windows: whether the host process is packaged (the legacy addon's
    /// per-adapter `deployment`). `None` elsewhere: the concept does not
    /// exist there.
    pub deployment: Option<crate::boundary::HostDeployment>,
}

/// List the adapters this host can open, in OS order, on the system bus
/// (BlueZ). Same as [`list_adapters_on`] with [`crate::BluezBus::System`].
pub async fn list_adapters() -> Result<Vec<AdapterListing>, DesktopError> {
    list_adapters_on(crate::boundary::BluezBus::System).await
}

/// List the adapters reachable on `bus` (BlueZ; the bus a
/// [`BtleplugRadio::open_on`] with the same bus opens on). A bus this build
/// cannot honour is `capability.unsupported` before anything opens.
pub async fn list_adapters_on(
    bus: crate::boundary::BluezBus,
) -> Result<Vec<AdapterListing>, DesktopError> {
    crate::boundary::bluez_bus_supported(bus)?;
    #[cfg(target_os = "windows")]
    {
        let adapters = crate::os::windows::list_adapters().await?;
        let deployment = crate::os::windows::deployment()?;
        Ok(adapters
            .into_iter()
            .enumerate()
            .map(|(index, adapter)| AdapterListing {
                index,
                label: Ok(adapter.id),
                display_name: adapter.name,
                default: adapter.default,
                deployment: Some(deployment),
            })
            .collect())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let manager = open_manager(bus).await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.open")
                .with_detail(error.to_string())
                .with_os(&error)
        })?;
        let adapters = manager.adapters().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.enumerate")
                .with_detail(error.to_string())
                .with_os(&error)
        })?;
        let mut listing = Vec::with_capacity(adapters.len());
        // `open(None)` selects only a sole adapter (finding 43).
        let sole = adapters.len() == 1;
        for (index, adapter) in adapters.iter().enumerate() {
            let info = adapter.adapter_info().await.map_err(|error| {
                DesktopError::adapter_unavailable("adapter.info")
                    .with_detail(error.to_string())
                    .with_os(&error)
            });
            let (label, display_name) = match info {
                Ok(info) => (Ok(adapter_identity(&info).to_owned()), Some(info)),
                Err(error) => (Err(error), None),
            };
            let default = sole && label.is_ok();
            listing.push(AdapterListing {
                index,
                label,
                display_name,
                default,
                deployment: None,
            });
        }
        Ok(listing)
    }
}

/// The btleplug manager on the chosen bus: the session bus only through
/// the vendored patch (callers check [`crate::bluez_bus_supported`] first).
async fn open_manager(bus: crate::boundary::BluezBus) -> Result<Manager, btleplug::Error> {
    match bus {
        crate::boundary::BluezBus::System => Manager::new().await,
        #[cfg(all(target_os = "linux", btleplug_ubm_bluez_session))]
        crate::boundary::BluezBus::Session => Manager::new_session_bus().await,
        #[cfg(not(all(target_os = "linux", btleplug_ubm_bluez_session)))]
        crate::boundary::BluezBus::Session => Err(btleplug::Error::NotSupported(
            "the D-Bus session bus is not available in this build".to_owned(),
        )),
    }
}

/// The selectable identity of one btleplug `adapter_info` label: the BlueZ
/// adapter id on Linux (`"hci0 (usb:…)"` → `hci0`), the label itself
/// elsewhere.
#[must_use]
pub fn adapter_identity(info: &str) -> &str {
    if cfg!(target_os = "linux") {
        crate::os::bluez_model::adapter_id_from_info(info)
    } else {
        info
    }
}

/// Choose one adapter out of an OS listing of btleplug labels (finding
/// 43; the Tauri attach path's rule): a withheld label fails the
/// selection, an empty listing is `adapter.unavailable`, an unmatched name
/// is `adapter.unavailable` (W-R2, as every legacy desktop provider
/// reported it), and no name with several adapters is `adapter.ambiguous`.
pub fn choose_adapter(
    labels: &[Result<String, DesktopError>],
    wanted: Option<&str>,
) -> Result<usize, DesktopError> {
    if labels.is_empty() {
        return Err(DesktopError::adapter_unavailable("adapter.select")
            .with_detail("the OS lists no Bluetooth adapter"));
    }
    if let Some(error) = labels.iter().find_map(|label| label.as_ref().err()) {
        return Err(error.clone());
    }
    let infos: Vec<&str> = labels
        .iter()
        .filter_map(|label| label.as_deref().ok())
        .collect();
    match wanted {
        // W-R2: a name the listing never held is `adapter.unavailable`,
        // as every legacy desktop provider reported it.
        Some(wanted) => infos
            .iter()
            .position(|info| adapter_matches(info, wanted))
            .ok_or_else(|| {
                DesktopError::adapter_unavailable("adapter.select")
                    .with_detail(format!("no adapter is named {wanted:?}"))
            }),
        None if infos.len() == 1 => Ok(0),
        None => Err(DesktopError::new(
            BleErrorCode::AdapterAmbiguous,
            BleErrorDomain::Adapter,
            "adapter.select",
        )
        .with_detail(format!(
            "{} adapters are present; name one ({})",
            infos.len(),
            infos
                .iter()
                .map(|info| adapter_identity(info))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Whether `wanted` selects the adapter with btleplug label `info`: its
/// identity, or (for callers that kept the full label) the label itself.
#[must_use]
pub fn adapter_matches(info: &str, wanted: &str) -> bool {
    wanted == info || wanted == adapter_identity(info)
}

fn power_state(state: btleplug::api::CentralState) -> AdapterPowerState {
    match state {
        btleplug::api::CentralState::PoweredOn => AdapterPowerState::PoweredOn,
        btleplug::api::CentralState::PoweredOff => AdapterPowerState::PoweredOff,
        btleplug::api::CentralState::Resetting => AdapterPowerState::Resetting,
        btleplug::api::CentralState::Unsupported => AdapterPowerState::Unsupported,
        btleplug::api::CentralState::Unauthorized => AdapterPowerState::Unauthorized,
        btleplug::api::CentralState::Unknown => AdapterPowerState::Unknown,
    }
}

/// The adapter gate the legacy backend of this OS applied (finding 58).
#[must_use]
pub const fn host_admission_policy() -> crate::boundary::AdmissionPolicy {
    if cfg!(target_os = "macos") {
        crate::boundary::AdmissionPolicy::CoreBluetooth
    } else if cfg!(target_os = "windows") {
        crate::boundary::AdmissionPolicy::WinRt
    } else {
        crate::boundary::AdmissionPolicy::LifecycleOnly
    }
}

/// Map a btleplug failure for a capability some platforms lack:
/// `NotSupported` is `capability.unsupported`, anything else a platform
/// failure — never a guessed value.
/// Finding 113: the platform's own answer behind a btleplug failure
/// (vendored patch 15), as typed fields on the error.
fn platform_detail(error: &btleplug::Error) -> Option<crate::errors::PlatformDetail> {
    let btleplug::Error::Platform(platform) = error else {
        return None;
    };
    Some(
        platform.metadata.iter().fold(
            crate::errors::PlatformDetail::new(platform.domain, platform.code.clone())
                .with_message(platform.message.clone()),
            |detail, (key, value)| {
                detail.with_metadata(*key, crate::errors::PlatformValue::Text(value.clone()))
            },
        ),
    )
}

/// Attach the platform's answer behind `cause` (finding 113).
pub(crate) trait WithOs {
    fn with_os(self, cause: &btleplug::Error) -> Self;
}

impl WithOs for DesktopError {
    /// The platform's answer rides the error without changing its identity:
    /// every operation keeps its own name (one vocabulary on every OS, owner
    /// decision 5.0). A genuine link loss, security refusal or transient
    /// connect is renamed by the central's classify chain
    /// (`classify_link_loss`, `classify_security`, `classify_connect_failure`
    /// plus `classify_establishment`), never here. This supersedes finding
    /// 124's legacy BlueZ identity (`normalizeBluezFailure` answered every
    /// D-Bus method error as `platform.failure`): the `bluez-dbus` answer is
    /// kept in `platform`, with `org.bluez.Error.Failed` when D-Bus gave no
    /// name.
    fn with_os(self, cause: &btleplug::Error) -> Self {
        // btleplug's own `NotConnected` is its answer where the OS gave none
        // (on BlueZ the synthesized detail keeps the legacy
        // `org.bluez.Error.Failed`, message `Not connected`): the link is
        // gone (owner decision, 5.0; see `classify_link_loss`).
        let not_connected = (!cfg!(target_os = "linux")
            && matches!(cause, btleplug::Error::NotConnected))
        .then(|| {
            crate::errors::PlatformDetail::new("btleplug", "not-connected")
                .with_message(cause.to_string())
        });
        let platform = platform_detail(cause).or(not_connected).or_else(|| {
            cfg!(target_os = "linux").then(|| {
                crate::errors::PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                    .with_message(cause.to_string())
            })
        });
        let Some(platform) = platform else {
            return self;
        };
        self.with_platform(platform)
    }
}

fn capability_error(operation: &'static str, error: btleplug::Error) -> DesktopError {
    let code = match error {
        btleplug::Error::NotSupported(_) => BleErrorCode::CapabilityUnsupported,
        _ => BleErrorCode::PlatformFailure,
    };
    let domain = match code {
        BleErrorCode::CapabilityUnsupported => BleErrorDomain::Capability,
        _ => BleErrorDomain::Connection,
    };
    DesktopError::new(code, domain, operation)
        .with_detail(error.to_string())
        .with_os(&error)
}

/// Resolve one peer out of an adapter listing (PR210-24/33): a failed
/// listing is `adapter.unavailable` (`peer.list`) — the adapter could not
/// answer — and only a listing without the peer is `peer.not-found`.
pub fn find_peer<P>(
    listing: Result<Vec<P>, btleplug::Error>,
    peer_id: &str,
    id_of: impl Fn(&P) -> String,
) -> Result<P, DesktopError> {
    let peers = listing.map_err(|error| {
        DesktopError::new(
            BleErrorCode::AdapterUnavailable,
            BleErrorDomain::Adapter,
            "peer.list",
        )
        .with_detail(error.to_string())
        .with_os(&error)
    })?;
    peers
        .into_iter()
        .find(|peer| id_of(peer) == peer_id)
        .ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::PeerNotFound,
                BleErrorDomain::Connection,
                "peer.lookup",
            )
            .with_detail(peer_id.to_owned())
        })
}

/// Per-peer GATT cache (PR210-25): the peripheral resolved for the current
/// connection, reused by every verb. Generic over the peripheral type so
/// the discovery discipline is testable without an adapter. The map lock
/// is never held across an await.
pub struct GattCache<P> {
    entries: StdMutex<HashMap<String, P>>,
    peer_gates: StdMutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    accepted_identities: StdMutex<HashMap<String, GattSnapshotIdentity>>,
}

type GattWork = futures_util::future::Shared<futures_util::future::BoxFuture<'static, ()>>;

#[derive(Default)]
struct GattWorkQueue {
    entries: StdMutex<HashMap<String, (GattWork, Option<GattWork>)>>,
}

impl GattWorkQueue {
    #[cfg(any(target_os = "linux", test))]
    fn enqueue(&self, key: String, work: impl Future<Output = ()> + Send + 'static) {
        let work = work.boxed().shared();
        let mut entries = self.entries.lock().expect("GATT observation work");
        if let Some((_, rerun)) = entries.get_mut(&key) {
            // One fresh reread after current work settles covers every hint
            // received during it; never reuse the current work's old answer.
            *rerun = Some(work);
        } else {
            entries.insert(key, (work, None));
        }
    }

    async fn completed(&self) {
        let pending: Vec<_> = self
            .entries
            .lock()
            .expect("GATT observation work")
            .iter()
            .map(|(key, (work, _))| (key.clone(), work.clone()))
            .collect();
        if pending.is_empty() {
            std::future::pending::<()>().await;
            return;
        }
        let futures = pending.iter().map(|(_, work)| work.clone());
        let (_, index, _) = futures_util::future::select_all(futures).await;
        let (key, work) = &pending[index];
        let mut entries = self.entries.lock().expect("GATT observation work");
        if let Some((current, rerun)) = entries.get_mut(key)
            && current.ptr_eq(work)
        {
            if let Some(next) = rerun.take() {
                *current = next;
            } else {
                entries.remove(key);
            }
        }
    }

    async fn drain(&self) {
        while !self
            .entries
            .lock()
            .expect("GATT observation work")
            .is_empty()
        {
            self.completed().await;
        }
    }
}

enum EventStep {
    Work,
    Notification(Option<RadioEvent>),
    Os(Option<RadioEvent>),
    Adapter(Option<CentralEvent>),
}

async fn select_radio_event(
    work: impl Future<Output = ()>,
    notification: impl Future<Output = Option<RadioEvent>>,
    os: impl Future<Output = Option<RadioEvent>>,
    adapter: impl Future<Output = Option<CentralEvent>>,
) -> EventStep {
    tokio::select! {
        () = work => EventStep::Work,
        event = notification => EventStep::Notification(event),
        event = os => EventStep::Os(event),
        event = adapter => EventStep::Adapter(event),
    }
}

impl<P: Clone> Default for GattCache<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Clone> GattCache<P> {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
            peer_gates: StdMutex::new(HashMap::new()),
            accepted_identities: StdMutex::new(HashMap::new()),
        }
    }

    /// Serialize publication and retirement for one peer. Weak entries are
    /// reclaimed on admission; a finished peer leaves no immortal gate owner.
    pub async fn lock_peer(&self, peer_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = {
            let mut gates = self
                .peer_gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            gates.retain(|_, gate| gate.strong_count() > 0);
            match gates.get(peer_id).and_then(std::sync::Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(Mutex::new(()));
                    gates.insert(peer_id.to_owned(), Arc::downgrade(&gate));
                    gate
                }
            }
        };
        gate.lock_owned().await
    }

    #[cfg(any(target_os = "linux", test))]
    fn accepted_identity(&self, peer: &str) -> Option<GattSnapshotIdentity> {
        self.accepted_identities
            .lock()
            .expect("accepted GATT identities")
            .get(peer)
            .cloned()
    }

    #[cfg(any(target_os = "linux", test))]
    fn remember_identity(&self, peer: &str, identity: GattSnapshotIdentity) {
        self.accepted_identities
            .lock()
            .expect("accepted GATT identities")
            .insert(peer.to_owned(), identity);
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, P>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached peripheral for `peer_id`, if any.
    pub fn get(&self, peer_id: &str) -> Option<P> {
        self.entries().get(peer_id).cloned()
    }

    /// Cache `peripheral` for `peer_id`, replacing any entry.
    pub fn insert(&self, peer_id: &str, peripheral: P) {
        self.accepted_identities
            .lock()
            .expect("accepted GATT identities")
            .remove(peer_id);
        self.entries().insert(peer_id.to_owned(), peripheral);
    }

    /// Forget `peer_id` (its connection or database ended).
    pub fn evict(&self, peer_id: &str) {
        self.entries().remove(peer_id);
        self.accepted_identities
            .lock()
            .expect("accepted GATT identities")
            .remove(peer_id);
    }

    /// Forget every peer.
    pub fn clear(&self) {
        self.entries().clear();
        self.accepted_identities
            .lock()
            .expect("accepted GATT identities")
            .clear();
    }

    /// The peripheral for one verb: the cached one, else `lookup()`, with
    /// `discover` run only when `needs_discovery` says the looked-up
    /// peripheral has no services yet. The result is cached.
    pub async fn resolve<L, LF, D, DF>(
        &self,
        peer_id: &str,
        lookup: L,
        needs_discovery: impl Fn(&P) -> bool,
        discover: D,
    ) -> Result<P, DesktopError>
    where
        L: FnOnce() -> LF,
        LF: Future<Output = Result<P, DesktopError>>,
        D: FnOnce(P) -> DF,
        DF: Future<Output = Result<P, DesktopError>>,
    {
        if let Some(peripheral) = self.get(peer_id) {
            return Ok(peripheral);
        }
        let peripheral = lookup().await?;
        let peripheral = if needs_discovery(&peripheral) {
            discover(peripheral).await?
        } else {
            peripheral
        };
        self.insert(peer_id, peripheral.clone());
        Ok(peripheral)
    }

    /// The peripheral for an explicit discovery: the cached one (else
    /// `lookup()`), always rediscovered, and the entry replaced.
    pub async fn refresh<L, LF, D, DF>(
        &self,
        peer_id: &str,
        lookup: L,
        discover: D,
    ) -> Result<P, DesktopError>
    where
        L: FnOnce() -> LF,
        LF: Future<Output = Result<P, DesktopError>>,
        D: FnOnce(P) -> DF,
        DF: Future<Output = Result<P, DesktopError>>,
    {
        let peripheral = match self.get(peer_id) {
            Some(peripheral) => peripheral,
            None => lookup().await?,
        };
        let peripheral = discover(peripheral).await?;
        self.insert(peer_id, peripheral.clone());
        Ok(peripheral)
    }

    /// Retire old notification ownership before a replacement can mutate the
    /// shared peripheral graph. The caller holds the peer publication gate.
    pub async fn refresh_retiring<L, LF, R, RF, D, DF>(
        &self,
        peer_id: &str,
        lookup: L,
        retire: R,
        discover: D,
    ) -> Result<P, DesktopError>
    where
        L: FnOnce() -> LF,
        LF: Future<Output = Result<P, DesktopError>>,
        R: FnOnce() -> RF,
        RF: Future<Output = Result<(), DesktopError>>,
        D: FnOnce(P) -> DF,
        DF: Future<Output = Result<P, DesktopError>>,
    {
        retire().await?;
        self.refresh(peer_id, lookup, discover).await
    }
}

/// Why one close-time scope release did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeRelease {
    /// The adapter listing no longer holds the peer: nothing is left to
    /// release (the only case skipped without a receipt).
    PeerGone(DesktopError),
    /// The scope could not be released; the detail says why.
    Failed(String),
}

/// A close-time scope release that outran [`CLOSE_SCOPE_BOUND`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseScopeElapsed;

/// The close receipt for one scope, if any: every outcome except a
/// released scope or a confirmed-missing peer is a [`RadioCloseFailure`].
#[must_use]
pub fn close_receipt(
    scope: &InstanceKey,
    outcome: Result<Result<(), ScopeRelease>, CloseScopeElapsed>,
) -> Option<RadioCloseFailure> {
    match outcome {
        Ok(Ok(())) | Ok(Err(ScopeRelease::PeerGone(_))) => None,
        Ok(Err(ScopeRelease::Failed(detail))) => {
            Some(RadioCloseFailure::new(scope.clone(), detail))
        }
        Err(CloseScopeElapsed) => Some(RadioCloseFailure::new(
            scope.clone(),
            format!(
                "release did not finish inside the close scope bound ({} ms)",
                CLOSE_SCOPE_BOUND.as_millis()
            ),
        )),
    }
}

async fn close_scope_debt<F, Fut>(
    debt: &StdMutex<HashSet<InstanceKey>>,
    mut release: F,
) -> Vec<RadioCloseFailure>
where
    F: FnMut(InstanceKey) -> Fut,
    Fut: std::future::Future<Output = Result<Result<(), ScopeRelease>, CloseScopeElapsed>>,
{
    // Snapshot identities without transferring ownership into this cancellable
    // future. A confirmed link end can retire debt while a release is pending.
    let mut scopes: Vec<_> = debt.lock().expect("cleanup debt").iter().cloned().collect();
    scopes.sort();
    let mut failures = Vec::new();
    for scope in scopes {
        if !debt.lock().expect("cleanup debt").contains(&scope) {
            continue;
        }
        if let Some(failure) = close_receipt(&scope, release(scope.clone()).await) {
            failures.push(failure);
        } else {
            debt.lock().expect("cleanup debt").remove(&scope);
        }
    }
    failures
}

type EventStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = CentralEvent> + Send>>;

#[cfg(any(target_os = "windows", test))]
fn maintained_connection_failure(error: DesktopError) -> DesktopError {
    let failure = DesktopError::connection_failed(format!(
        "GattSession.MaintainConnection could not be held: {}",
        error.detail().unwrap_or(error.code_str())
    ));
    match error.platform() {
        Some(platform) => failure.with_platform(platform.clone()),
        None => failure,
    }
}
/// One peripheral-wide notification stream as btleplug 0.12 yields it from
/// [`btleplug::api::Peripheral::notifications`]. Public so the
/// production-path ingress harness can inject scripted streams into the real
/// forwarder.
pub type NotificationStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = ValueNotification> + Send>>;

/// One live notification forwarder: the task fanning one characteristic
/// instance into the shared event channel, plus the instance address for
/// best-effort OS unsubscribe at teardown.
///
/// Public (with public fields) so the production-path ingress harness can
/// install a real forwarder task into a real consumer table and drive the
/// genuine teardown-folding paths; production construction is unchanged.
pub struct ForwarderEntry {
    /// Fan-out task for this instance; aborted on successful teardown,
    /// drained at a disconnect or database change (finding 129).
    pub task: ForwarderTask,
    /// Peer the subscription belongs to.
    pub peer_id: String,
    /// Owning service UUID (routing identity, F09).
    pub service_uuid: String,
    /// Occurrence among duplicate service UUIDs.
    pub service_occurrence: u64,
    /// Subscribed characteristic UUID (routing identity, F09).
    pub characteristic_uuid: String,
    /// Occurrence among duplicate characteristic UUIDs.
    pub characteristic_occurrence: u64,
    /// The subscription epoch the forwarder stamps (F10), for the loss a
    /// drain reports (finding 129).
    pub epoch: u64,
}

impl ForwarderEntry {
    /// Instance scope this forwarder owns, for ambiguity checks (F09).
    pub fn scope(&self) -> InstanceKey {
        (
            self.peer_id.clone(),
            self.service_uuid.clone(),
            self.service_occurrence,
            self.characteristic_uuid.clone(),
            self.characteristic_occurrence,
        )
    }
}

/// Bounded notification ingress (F07): 256 items / 256 KiB bytes at the first
/// owned handoff. Forwarders `try_send` (never block the runtime worker);
/// overload drops are counted, never silent, and adapter control (connect,
/// disconnect, service-change) travels a separate stream so it never starves.
///
/// Public so the production-path ingress harness
/// (`tests/production_ingress.rs`) derives its exact drop expectations from
/// the real caps instead of restating them.
///
/// Sized to the public stream maximum and the contract's backend-ingress
/// aggregate (finding 107 audit): the legacy backends queued far more than
/// 256 values before losing any (CoreBluetooth without a bound, WinRT 128
/// per addon), so the ingress is never a loss point below them.
pub const NOTIFICATION_CAP: usize = ubm_core::contracts::MAX_STREAM_ITEM_CAPACITY as usize;
/// Byte half of the bounded notification ingress (F07); see
/// [`NOTIFICATION_CAP`].
pub const NOTIFICATION_BYTES: u64 = ubm_core::contracts::BACKEND_INGRESS_AGGREGATE_BYTES;

/// Production radio backend over one btleplug adapter.
pub struct BtleplugRadio {
    adapter: Adapter,
    adapter_label: String,
    spawn: tokio::runtime::Handle,
    events: Mutex<Option<EventStream>>,
    notifications: mpsc::Sender<RadioEvent>,
    notification_rx: Mutex<mpsc::Receiver<RadioEvent>>,
    ingress_bytes: Arc<AtomicU64>,
    ingress_dropped: Arc<AtomicU64>,
    /// Link and database events held until the notifications queued ahead
    /// of them are delivered (finding 129).
    deferred: Arc<Mutex<VecDeque<RadioEvent>>>,
    forwarders: Arc<StdMutex<HashMap<String, ForwarderEntry>>>,
    gatt_work: GattWorkQueue,
    #[cfg(target_os = "linux")]
    gatt_control_failure: Arc<StdMutex<Option<DesktopError>>>,
    #[cfg(target_os = "linux")]
    gatt_peer_failures: Arc<StdMutex<HashMap<String, DesktopError>>>,
    closing_forwarders: Mutex<Vec<tokio::task::JoinHandle<u64>>>,
    /// Cleanup debt (F13): scopes whose native CCCD may be live without
    /// an installed forwarder — a failed setup rollback or a failed
    /// consumer-less teardown. Retry and dispose keep attempting the
    /// native release until it succeeds; the ambiguity check (F09)
    /// treats debt as live because the CCCD may still emit.
    cleanup_debt: Arc<StdMutex<HashSet<InstanceKey>>>,
    notification_targets: NotificationTargets<Peripheral>,
    /// Close-time release failures retained by the last [`RadioBoundary::close`]
    /// (F14 receipts): one entry per scope whose native unsubscribe did not
    /// complete. Drained by `take_close_failures` into the shutdown report.
    close_failures: StdMutex<Vec<RadioCloseFailure>>,
    /// Per-peer GATT cache (PR210-25).
    gatt: Arc<GattCache<Peripheral>>,
    /// OS control events from the narrow adapters (bond changes), a
    /// separate bounded source so a notification flood never starves them.
    /// The radio keeps one sender so the source never closes under it; the
    /// Windows adapter's `GattServicesChanged` handlers send through clones.
    #[cfg_attr(not(target_os = "windows"), allow(clippy::used_underscore_binding))]
    _os_events_tx: mpsc::Sender<RadioEvent>,
    os_events: Mutex<mpsc::Receiver<RadioEvent>>,
    /// BlueZ D-Bus adapter (Linux). An open failure is kept and answered by
    /// every call that needs it, never swallowed.
    #[cfg(target_os = "linux")]
    bluez: Result<Arc<crate::os::linux::Bluez>, DesktopError>,
    #[cfg(target_os = "linux")]
    bluez_connection_policy: Option<crate::boundary::BluezConnectionPolicy>,
    /// The BlueZ bond-change watcher task (Linux), aborted with the radio.
    #[cfg(target_os = "linux")]
    bluez_watch: Option<tokio::task::JoinHandle<()>>,
    #[cfg(target_os = "linux")]
    bluez_gatt_watch: Result<(), DesktopError>,
    /// The BlueZ adapter-presence watcher task (Linux, finding 57),
    /// aborted with the radio.
    #[cfg(target_os = "linux")]
    bluez_adapter_watch: Option<tokio::task::JoinHandle<()>>,
    /// WinRT pairing and maintained sessions (Windows).
    #[cfg(target_os = "windows")]
    winrt: crate::os::windows::WinRt,
    /// Per-connection CoreBluetooth readiness forwarders (macOS, vendored
    /// patch 4), aborted with the link.
    readiness_watchers: StdMutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// The WinRT watcher-stopped forwarder (vendored patch 5), aborted with
    /// the radio.
    scan_stopped_watch: Option<tokio::task::JoinHandle<()>>,
    /// Watcher stops this radio requested and has not seen yet: the OS
    /// reports those too, and they must not end a later scan.
    expected_scan_stops: Arc<AtomicU64>,
}

/// Bound of the OS control-event queue. Bond changes are rare; the
/// watcher waits (never drops) when the queue is full.
const OS_EVENT_CAP: usize = 64;

impl Drop for BtleplugRadio {
    /// Safety net for a radio dropped without [`RadioBoundary::close`]
    /// (e.g. a NAPI finalizer on a V8 thread): the event stream's Drop
    /// spawns a task, so an off-runtime drop aborts the host process.
    /// The stream is dropped inline inside an entered context of the
    /// shared executor — never handed off via `spawn`. A handoff is
    /// unsound here: when the executor is already shutting down (host
    /// teardown runs finalizers after executor shutdown), the spawned
    /// task is cancelled immediately and the stream is dropped with no
    /// ambient context anyway, panicking the host. Entering the context
    /// is correct in both states: on a live executor the stream's inner
    /// spawn schedules normally, and on a dead one it degrades to a
    /// silent canceled-task drop. `try_lock` cannot be contended here:
    /// the guard is only held by polls borrowing a live owner, and this
    /// runs at last-owner drop — and if it ever is, [`drop_event_stream`]
    /// reports it (counter plus diagnostic), and debug builds assert.
    fn drop(&mut self) {
        let outcome = drop_event_stream(&self.events, &self.spawn);
        debug_assert!(
            outcome != DropStream::Contended,
            "BtleplugRadio dropped while its event stream was in use"
        );
        #[cfg(target_os = "linux")]
        if let Some(watch) = self.bluez_watch.take() {
            watch.abort();
        }
        #[cfg(target_os = "linux")]
        if let Some(watch) = self.bluez_adapter_watch.take() {
            watch.abort();
        }
        if let Some(watch) = self.scan_stopped_watch.take() {
            watch.abort();
        }
        for (_, watcher) in self
            .readiness_watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
        {
            watcher.abort();
        }
    }
}

impl BtleplugRadio {
    /// Open the default btleplug adapter. When `adapter_id` is `Some`, it
    /// must equal `Adapter::adapter_info()` exactly; otherwise
    /// `adapter.unavailable` fails the open without touching another
    /// adapter. `spawn` is the shared desktop executor handle (see
    /// [`crate::executor::desktop_runtime`]).
    pub async fn open(
        spawn: tokio::runtime::Handle,
        adapter_id: Option<String>,
    ) -> Result<Self, DesktopError> {
        Self::open_on(spawn, adapter_id, crate::boundary::BluezBus::System).await
    }

    /// [`BtleplugRadio::open`] with an explicit BlueZ D-Bus bus (Linux;
    /// the legacy backend's `busKind`). A bus this build cannot honour is
    /// `capability.unsupported` before anything opens
    /// ([`crate::bluez_bus_supported`]).
    pub async fn open_on(
        spawn: tokio::runtime::Handle,
        adapter_id: Option<String>,
        bus: crate::boundary::BluezBus,
    ) -> Result<Self, DesktopError> {
        Self::open_on_with_policy(spawn, adapter_id, bus, None).await
    }

    /// Open with an explicit trusted-host BlueZ LE daemon attestation.
    /// Other platforms reject a supplied BlueZ policy before allocation.
    pub async fn open_on_with_policy(
        spawn: tokio::runtime::Handle,
        adapter_id: Option<String>,
        bus: crate::boundary::BluezBus,
        connection_policy: Option<crate::boundary::BluezConnectionPolicy>,
    ) -> Result<Self, DesktopError> {
        if let Some(policy) = &connection_policy {
            policy.validate()?;
        }
        if !cfg!(target_os = "linux") && connection_policy.is_some() {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "connection.policy",
            )
            .with_detail("a BlueZ connection policy applies to Linux only"));
        }
        crate::boundary::bluez_bus_supported(bus)?;
        let manager = open_manager(bus).await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.open")
                .with_detail(error.to_string())
                .with_os(&error)
        })?;
        let adapters = manager.adapters().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.enumerate")
                .with_detail(error.to_string())
                .with_os(&error)
        })?;
        let (adapter, adapter_label) = Self::select_adapter(adapters, adapter_id).await?;
        let events = adapter.events().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.events")
                .with_detail(error.to_string())
                .with_os(&error)
        })?;
        let (notifications, notification_rx) = mpsc::channel(NOTIFICATION_CAP);
        let (os_events_tx, os_events) = mpsc::channel(OS_EVENT_CAP);
        #[cfg(target_os = "linux")]
        let bluez = crate::os::linux::Bluez::open_with_le_owner(
            &adapter_label,
            bus,
            connection_policy.as_ref().map(|policy| match policy {
                crate::boundary::BluezConnectionPolicy::LeBearer {
                    daemon_unique_owner,
                } => daemon_unique_owner.clone(),
            }),
        )
        .await;
        #[cfg(target_os = "linux")]
        let (bluez_watch, bluez_gatt_watch) = match &bluez {
            Ok(bluez) => match bluez.watch_security(os_events_tx.clone(), &spawn).await {
                Ok(watch) => (Some(watch), Ok(())),
                Err(error) => (None, Err(error)),
            },
            Err(error) => (None, Err(error.clone())),
        };
        #[cfg(target_os = "linux")]
        let bluez_adapter_watch = bluez
            .as_ref()
            .ok()
            .map(|bluez| bluez.watch_adapter(os_events_tx.clone(), &spawn));
        #[cfg(target_os = "windows")]
        let winrt =
            crate::os::windows::WinRt::open(&adapter_label, adapter.clone(), os_events_tx.clone())?;
        let expected_scan_stops = Arc::new(AtomicU64::new(0));
        let scan_stopped_watch = watch_scan_stopped(
            &adapter,
            &spawn,
            os_events_tx.clone(),
            Arc::clone(&expected_scan_stops),
        );
        Ok(Self {
            adapter,
            adapter_label,
            spawn,
            events: Mutex::new(Some(events)),
            notifications,
            notification_rx: Mutex::new(notification_rx),
            ingress_bytes: Arc::new(AtomicU64::new(0)),
            ingress_dropped: Arc::new(AtomicU64::new(0)),
            deferred: Arc::new(Mutex::new(VecDeque::new())),
            forwarders: Arc::new(StdMutex::new(HashMap::new())),
            gatt_work: GattWorkQueue::default(),
            #[cfg(target_os = "linux")]
            gatt_control_failure: Arc::new(StdMutex::new(None)),
            #[cfg(target_os = "linux")]
            gatt_peer_failures: Arc::new(StdMutex::new(HashMap::new())),
            cleanup_debt: Arc::new(StdMutex::new(HashSet::new())),
            notification_targets: NotificationTargets::default(),
            close_failures: StdMutex::new(Vec::new()),
            closing_forwarders: Mutex::new(Vec::new()),
            gatt: Arc::new(GattCache::new()),
            _os_events_tx: os_events_tx,
            os_events: Mutex::new(os_events),
            #[cfg(target_os = "linux")]
            bluez,
            #[cfg(target_os = "linux")]
            bluez_connection_policy: connection_policy,
            #[cfg(target_os = "linux")]
            bluez_watch,
            #[cfg(target_os = "linux")]
            bluez_gatt_watch,
            #[cfg(target_os = "linux")]
            bluez_adapter_watch,
            #[cfg(target_os = "windows")]
            winrt,
            readiness_watchers: StdMutex::new(HashMap::new()),
            scan_stopped_watch,
            expected_scan_stops,
        })
    }

    /// Choose the btleplug adapter `adapter_id` names (finding 43): the
    /// same rule the Tauri attach path applies — an adapter whose identity
    /// the OS withholds fails the selection (never skipped, never labelled
    /// "unknown"), no adapter is `adapter.unavailable`, a name that matches
    /// none is `adapter.unavailable` (W-R2), and no name with several
    /// adapters is `adapter.ambiguous` instead of silently taking the first.
    #[cfg(not(target_os = "windows"))]
    async fn select_adapter(
        adapters: Vec<Adapter>,
        adapter_id: Option<String>,
    ) -> Result<(Adapter, String), DesktopError> {
        let mut labels = Vec::with_capacity(adapters.len());
        for adapter in &adapters {
            labels.push(adapter.adapter_info().await.map_err(|error| {
                DesktopError::adapter_unavailable("adapter.info")
                    .with_detail(error.to_string())
                    .with_os(&error)
            }));
        }
        let index = choose_adapter(&labels, adapter_id.as_deref())?;
        let label = labels
            .swap_remove(index)
            .map(|info| adapter_identity(&info).to_owned())?;
        let adapter = adapters.into_iter().nth(index).ok_or_else(|| {
            DesktopError::adapter_unavailable("adapter.select")
                .with_detail("the adapter listing changed during selection")
        })?;
        Ok((adapter, label))
    }

    /// Windows: the adapter is chosen by its native device id (the label)
    /// among those Windows enumerates, and the btleplug adapter is built on
    /// that adapter's own radio (`BluetoothAdapter::FromIdAsync`, legacy
    /// `SelectAdapter`), so a non-default adapter reports its own state and
    /// authorization. btleplug's per-radio listing is not used: its order
    /// says nothing about which adapter a radio belongs to.
    #[cfg(target_os = "windows")]
    async fn select_adapter(
        _radios: Vec<Adapter>,
        adapter_id: Option<String>,
    ) -> Result<(Adapter, String), DesktopError> {
        crate::os::windows::select_adapter(adapter_id.as_deref()).await
    }

    /// The label this radio's adapter was selected by.
    #[must_use]
    pub fn adapter_label(&self) -> &str {
        &self.adapter_label
    }

    #[cfg(target_os = "linux")]
    fn bluez(&self) -> Result<&Arc<crate::os::linux::Bluez>, DesktopError> {
        self.bluez.as_ref().map_err(Clone::clone)
    }

    #[cfg(target_os = "linux")]
    fn bluez_owner(&self, operation: &str) -> Result<&str, DesktopError> {
        match &self.bluez_connection_policy {
            Some(crate::boundary::BluezConnectionPolicy::LeBearer { daemon_unique_owner }) => Ok(daemon_unique_owner),
            None => Err(DesktopError::new(BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability, operation).with_detail(
                    "BlueZ LE lifecycle requires a trusted host attestation for the current unique daemon owner implementing org.bluez.Bearer.LE1; Device1 fallback is not supported")),
        }
    }

    async fn observe_link_ended(&self, peer_id: String) {
        self.gatt.evict(&peer_id);
        let retirement = if cfg!(target_os = "linux") {
            PeerRetirement::LinkEndedRetainingNotifySession
        } else {
            PeerRetirement::LinkEnded
        };
        self.drain_peer_forwarders(&peer_id, retirement).await;
        if let Err(error) = self.release_link_state(&peer_id) {
            OS_RELEASE_FAILURES.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "ubm-desktop: link state of {peer_id} not released after loss: {}",
                error.detail().unwrap_or(error.code_str())
            );
        }
        self.deferred
            .lock()
            .await
            .push_back(RadioEvent::Disconnected(peer_id));
    }

    /// Bytes currently queued in the bounded notification ingress (F07).
    #[must_use]
    pub fn ingress_queued_bytes(&self) -> u64 {
        self.ingress_bytes.load(Ordering::Relaxed)
    }

    /// Notifications dropped by explicit ingress overload (F07).
    #[must_use]
    pub fn ingress_dropped(&self) -> u64 {
        self.ingress_dropped.load(Ordering::Relaxed)
    }

    async fn peripheral_by_id(&self, peer_id: &str) -> Result<Peripheral, DesktopError> {
        let found = find_peer(self.adapter.peripherals().await, peer_id, |peripheral| {
            peripheral.id().to_string()
        });
        let peripheral = match found {
            Err(error) if error.code() == BleErrorCode::PeerNotFound => {
                // Finding 127: a peer the adapter no longer lists is resolved
                // by identity, as the legacy backends reconnected without a
                // scan (CoreBluetooth `retrievePeripheralsWithIdentifiers`,
                // WinRT by address; vendored patch 19). BlueZ asks BlueZ.
                match platform_peripheral_id(peer_id) {
                    Some(id) => self
                        .adapter
                        .add_peripheral(&id)
                        .await
                        .map_err(|cause| error.with_detail(format!("{peer_id}: {cause}"))),
                    None => Err(error),
                }
            }
            other => other,
        }?;
        #[cfg(target_os = "linux")]
        let peripheral = peripheral
            .with_le_owner(self.bluez_owner("connection.authority")?)
            .await
            .map_err(|error| DesktopError::connection_failed(error.to_string()).with_os(&error))?;
        Ok(peripheral)
    }

    async fn discover_services_on(peripheral: Peripheral) -> Result<Peripheral, DesktopError> {
        peripheral.discover_services().await.map_err(map_radio(
            "discovery.complete",
            BleErrorCode::GattDiscoveryRequired,
            BleErrorDomain::Gatt,
        ))?;
        Ok(peripheral)
    }

    /// The peripheral a GATT verb runs on (PR210-25): the one cached for
    /// this connection, discovering services only when it has none.
    async fn cached_peripheral(&self, peer_id: &str) -> Result<Peripheral, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.gatt_watch_health()?;
            // An explicit, attested discovery owns graph publication. A verb
            // or cleanup must not silently publish a replacement graph while
            // the central still owns paths from the preceding revision.
            cached_gatt_peer(&self.gatt, &self.gatt_peer_failures, peer_id)
        }
        #[cfg(not(target_os = "linux"))]
        self.gatt
            .resolve(
                peer_id,
                || self.peripheral_by_id(peer_id),
                |peripheral| peripheral.services().is_empty(),
                Self::discover_services_on,
            )
            .await
    }

    async fn discover_with_identity(
        &self,
        peer_id: &str,
    ) -> Result<(Vec<ServiceSnapshot>, Option<GattSnapshotIdentity>), DesktopError> {
        #[cfg(target_os = "linux")]
        self.gatt_watch_health()?;
        let _gate = self.gatt.lock_peer(peer_id).await;
        let peripheral = self
            .gatt
            .refresh_retiring(
                peer_id,
                || self.peripheral_by_id(peer_id),
                || self.retire_notifications_for_discovery(peer_id),
                |peripheral| async {
                    let peripheral = Self::discover_services_on(peripheral).await?;
                    #[cfg(target_os = "linux")]
                    self.gatt_watch_health()?;
                    Ok(peripheral)
                },
            )
            .await?;
        let graph = service_snapshots(&peripheral.services());
        let identity = match self.gatt_snapshot_identity(peer_id) {
            Ok(identity) => identity,
            Err(error) => {
                self.gatt.evict(peer_id);
                return Err(error);
            }
        };
        #[cfg(target_os = "linux")]
        if identity.is_none() {
            self.gatt.evict(peer_id);
            return Err(DesktopError::new(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                "discovery.complete",
            )
            .with_detail("the discovered graph has no accepted LE GATT identity"));
        }
        #[cfg(target_os = "linux")]
        {
            if let Err(error) = self.gatt_watch_health() {
                self.gatt.evict(peer_id);
                return Err(error);
            }
            if let Some(identity) = &identity {
                self.gatt_observation_state()
                    .confirm_reverification(peer_id, identity.clone());
            }
        }
        Ok((graph, identity))
    }

    #[cfg(target_os = "linux")]
    fn gatt_watch_health(&self) -> Result<(), DesktopError> {
        if let Some(error) = self
            .gatt_control_failure
            .lock()
            .expect("GATT control failure")
            .as_ref()
        {
            return Err(error.clone());
        }
        self.bluez_gatt_watch.clone()?;
        self.bluez
            .as_ref()
            .map_err(Clone::clone)?
            .gatt_watch_health()
    }

    #[cfg(target_os = "linux")]
    fn observe_gatt_hint(&self, peer_id: &str) {
        if self.gatt.get(peer_id).is_none() {
            return;
        }
        let state = self.gatt_observation_state();
        let peer_id = peer_id.to_owned();
        self.gatt_work
            .enqueue(format!("peer:{peer_id}"), async move {
                state.observe_hint(&peer_id).await;
            });
    }

    #[cfg(target_os = "linux")]
    fn gatt_observation_state(&self) -> GattObservationState {
        GattObservationState {
            gatt: Arc::clone(&self.gatt),
            forwarders: Arc::clone(&self.forwarders),
            debt: Arc::clone(&self.cleanup_debt),
            deferred: Arc::clone(&self.deferred),
            failure: Arc::clone(&self.gatt_control_failure),
            peer_failures: Arc::clone(&self.gatt_peer_failures),
            observation_bound: btleplug::platform::LE_GATT_OBSERVATION_TIMEOUT,
        }
    }

    #[cfg(target_os = "linux")]
    fn observe_gatt_watch_failure(&self, error: DesktopError) {
        let state = self.gatt_observation_state();
        retain_gatt_control_failure(&state.failure, &error);
        self.gatt_work.enqueue("watch-failure".into(), async move {
            state.fail(error).await;
        });
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone)]
struct GattObservationState<P = Peripheral> {
    gatt: Arc<GattCache<P>>,
    forwarders: Arc<StdMutex<HashMap<String, ForwarderEntry>>>,
    debt: Arc<StdMutex<HashSet<InstanceKey>>>,
    deferred: Arc<Mutex<VecDeque<RadioEvent>>>,
    failure: Arc<StdMutex<Option<DesktopError>>>,
    peer_failures: Arc<StdMutex<HashMap<String, DesktopError>>>,
    observation_bound: Duration,
}

#[cfg(any(target_os = "linux", test))]
trait GattObservationSource: Clone + Send + Sync + 'static {
    fn accepted_identity(&self) -> Result<Option<GattSnapshotIdentity>, DesktopError>;
    fn current_identity(
        &self,
    ) -> impl Future<Output = Result<GattSnapshotIdentity, DesktopError>> + Send;
}

#[cfg(target_os = "linux")]
impl GattObservationSource for Peripheral {
    fn accepted_identity(&self) -> Result<Option<GattSnapshotIdentity>, DesktopError> {
        self.accepted_le_gatt_ready_token()
            .map(|token| {
                token.map(|token| GattSnapshotIdentity {
                    owner: token.daemon_owner,
                    attachment: token.attachment,
                    revision: token.revision,
                })
            })
            .map_err(map_radio(
                "gatt.snapshot-identity",
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
            ))
    }
    async fn current_identity(&self) -> Result<GattSnapshotIdentity, DesktopError> {
        let snapshot = self
            .le_gatt_snapshot()
            .await
            .map_err(|error| capability_error("gatt.observe", error))?;
        let token = snapshot
            .ready_token()
            .map_err(btleplug::Error::from)
            .map_err(|error| capability_error("gatt.observe", error))?;
        Ok(GattSnapshotIdentity {
            owner: token.daemon_owner,
            attachment: token.attachment,
            revision: token.revision,
        })
    }
}

#[cfg(any(target_os = "linux", test))]
fn retain_gatt_control_failure(slot: &StdMutex<Option<DesktopError>>, error: &DesktopError) {
    slot.lock()
        .expect("GATT control failure")
        .get_or_insert_with(|| error.clone());
}

#[cfg(any(target_os = "linux", test))]
fn cached_gatt_peer<P: Clone>(
    gatt: &GattCache<P>,
    failures: &StdMutex<HashMap<String, DesktopError>>,
    peer: &str,
) -> Result<P, DesktopError> {
    if let Some(error) = failures
        .lock()
        .expect("GATT peer observation failures")
        .get(peer)
    {
        return Err(error.clone());
    }
    gatt.get(peer).ok_or_else(|| {
        DesktopError::new(
            BleErrorCode::GattDiscoveryRequired,
            BleErrorDomain::Gatt,
            "gatt.snapshot",
        )
        .with_detail("an explicit current LE GATT discovery is required")
    })
}

#[cfg(any(target_os = "linux", test))]
impl<P: Clone> GattObservationState<P> {
    fn confirm_reverification(&self, peer: &str, identity: GattSnapshotIdentity) {
        self.gatt.remember_identity(peer, identity);
        self.peer_failures
            .lock()
            .expect("GATT peer observation failures")
            .remove(peer);
    }
    async fn fail_peer(&self, peer: &str, identity: GattSnapshotIdentity, error: DesktopError) {
        self.peer_failures
            .lock()
            .expect("GATT peer observation failures")
            .insert(peer.to_owned(), error.clone());
        self.gatt.evict(peer);
        drain_gatt_forwarders(
            &self.forwarders,
            &self.debt,
            &self.deferred,
            peer,
            PeerRetirement::DatabaseChanged,
        )
        .await;
        self.deferred
            .lock()
            .await
            .push_back(RadioEvent::GattObservationFailed {
                peer_id: peer.to_owned(),
                identity,
                error,
            });
    }
    async fn fail(&self, error: DesktopError) {
        retain_gatt_control_failure(&self.failure, &error);
        let mut peers: BTreeSet<_> = self.gatt.entries().keys().cloned().collect();
        peers.extend(
            self.forwarders
                .lock()
                .expect("forwarder table")
                .values()
                .map(|entry| entry.peer_id.clone()),
        );
        for peer in peers {
            let _gate = self.gatt.lock_peer(&peer).await;
            self.gatt.evict(&peer);
            drain_gatt_forwarders(
                &self.forwarders,
                &self.debt,
                &self.deferred,
                &peer,
                PeerRetirement::DatabaseChanged,
            )
            .await;
        }
        self.deferred
            .lock()
            .await
            .push_back(RadioEvent::GattWatchFailed(error));
    }
}

#[cfg(any(target_os = "linux", test))]
impl<P: GattObservationSource> GattObservationState<P> {
    async fn observe_hint(&self, peer_id: &str) {
        // Capture and retire under the same gate as graph publication and
        // forwarder installation. Central consumption has a second fence:
        // this deferred R1 event must never invalidate a later R2 database.
        let _gate = self.gatt.lock_peer(peer_id).await;
        let Some(peripheral) = self.gatt.get(peer_id) else {
            return;
        };
        let Some(last_accepted) = self.gatt.accepted_identity(peer_id) else {
            return;
        };
        let accepted = peripheral.accepted_identity();
        let identity = match accepted {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                self.fail_peer(
                    peer_id,
                    last_accepted,
                    DesktopError::new(
                        BleErrorCode::ProtocolMalformed,
                        BleErrorDomain::Gatt,
                        "gatt.snapshot-identity",
                    )
                    .with_detail("a published graph lost its accepted native identity"),
                )
                .await;
                return;
            }
            Err(error) => {
                self.fail_peer(peer_id, last_accepted, error).await;
                return;
            }
        };
        let answer =
            tokio::time::timeout(self.observation_bound, peripheral.current_identity()).await;
        let current = match answer {
            Ok(Ok(current)) => current,
            Ok(Err(error)) => {
                self.fail_peer(peer_id, identity, error).await;
                return;
            }
            Err(_) => {
                self.fail_peer(
                    peer_id,
                    identity,
                    DesktopError::new(
                        BleErrorCode::OperationTimedOut,
                        BleErrorDomain::Gatt,
                        "gatt.observe",
                    )
                    .with_detail(
                        "authoritative LE GATT snapshot read exceeded its observation bound",
                    ),
                )
                .await;
                return;
            }
        };
        if current == identity {
            return;
        }
        self.gatt.evict(peer_id);
        drain_gatt_forwarders(
            &self.forwarders,
            &self.debt,
            &self.deferred,
            peer_id,
            PeerRetirement::DatabaseChanged,
        )
        .await;
        self.deferred
            .lock()
            .await
            .push_back(RadioEvent::ServicesChangedScoped {
                peer_id: peer_id.to_owned(),
                identity,
            });
    }
}

impl BtleplugRadio {
    /// Release the original immutable native target, without a new graph lookup.
    async fn release_scope(&self, scope: &InstanceKey) -> Result<(), ScopeRelease> {
        let _gate = self.gatt.lock_peer(&scope.0).await;
        let target = self.notification_targets.get(scope).ok_or_else(|| {
            ScopeRelease::Failed("original native notification cleanup target is absent".to_owned())
        })?;
        release_original_notification_target(&target)
            .await
            .map_err(|error| ScopeRelease::Failed(error.to_string()))?;
        self.notification_targets.remove_if(scope, &target);
        Ok(())
    }

    /// No replacement graph/enable may reuse a native path while an older
    /// sender-owned notify client at that path still needs retirement.
    async fn retire_notifications_for_discovery(&self, peer_id: &str) -> Result<(), DesktopError> {
        self.drain_peer_forwarders(peer_id, PeerRetirement::DatabaseChanged)
            .await;
        retire_notification_debt_for_peer(
            &self.notification_targets,
            &self.forwarders,
            &self.cleanup_debt,
            peer_id,
        )
        .await
    }

    /// A known peripheral's merged OS state (labelled
    /// [`crate::boundary::ObservationSource::DeviceState`]). An unreadable
    /// state is an error, never an empty record (finding 122).
    async fn snapshot(&self, peripheral: &Peripheral) -> Result<PeerSnapshot, DesktopError> {
        let properties = peripheral.properties().await.map_err(|error| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Platform,
                "peer.properties",
            )
            .with_detail(error.to_string())
            .with_os(&error)
        })?;
        let (service_uuids, rssi, local_name, manufacturer_data, service_data, tx_power_level) =
            properties
                .map(|facts| {
                    (
                        facts
                            .services
                            .into_iter()
                            .map(|uuid| uuid.to_string())
                            .collect(),
                        facts.rssi,
                        facts.local_name,
                        sorted_manufacturer_data(&facts.manufacturer_data),
                        sorted_service_data(&facts.service_data),
                        facts.tx_power_level,
                    )
                })
                .unwrap_or_default();
        // btleplug exposes the address type opaquely per platform; report the
        // string, never guess public-vs-random (see PARITY_GAPS.md).
        let mut extras = advertisement_extras(peripheral);
        extras.source = crate::boundary::ObservationSource::DeviceState;
        Ok(PeerSnapshot {
            id: peripheral.id().to_string(),
            address: peripheral_address(peripheral),
            service_uuids,
            rssi,
            local_name,
            manufacturer_data,
            service_data,
            tx_power_level,
            extras,
        })
    }

    /// One OS sighting as an observation, with the peripheral's address
    /// (finding 120). A peripheral btleplug no longer lists is counted and
    /// logged, never dropped silently.
    async fn sighting(
        &self,
        id: &PeripheralId,
        report: btleplug::api::AdvertisementReport,
    ) -> Option<RadioEvent> {
        match self.adapter.peripheral(id).await {
            Ok(peripheral) => Some(RadioEvent::Advertisement(observation_from_report(
                id.to_string(),
                peripheral_address(&peripheral),
                report,
            ))),
            Err(error) => {
                note_unread_sighting(&id.to_string(), &error.to_string());
                None
            }
        }
    }

    /// CoreBluetooth reports read responses and notifications through one
    /// callback; the vendored btleplug (UBM_PATCHES.md #14) says which the
    /// value can be. WinRT (`ReadValueAsync`, uncached) and BlueZ
    /// (`ReadValue`) answer a read with its own response, never a
    /// notification.
    #[cfg(target_os = "macos")]
    async fn read_with_provenance(
        peripheral: &Peripheral,
        characteristic: &btleplug::api::Characteristic,
    ) -> btleplug::Result<CharacteristicRead> {
        let (value, provenance) = peripheral.read_with_provenance(characteristic).await?;
        Ok(CharacteristicRead {
            value,
            provenance: match provenance {
                btleplug::api::ReadProvenance::ReadResponse => {
                    crate::boundary::ReadProvenance::ReadResponse
                }
                btleplug::api::ReadProvenance::ReadOrNotification => {
                    crate::boundary::ReadProvenance::ReadOrNotification
                }
            },
        })
    }

    #[cfg(not(target_os = "macos"))]
    async fn read_with_provenance(
        peripheral: &Peripheral,
        characteristic: &btleplug::api::Characteristic,
    ) -> btleplug::Result<CharacteristicRead> {
        peripheral
            .read(characteristic)
            .await
            .map(CharacteristicRead::read_response)
    }

    /// Occurrence-aware instance lookup over the cached GATT database.
    /// Services and characteristics iterate in btleplug's canonical order
    /// (UUID-first), the same order [`RadioBoundary::discover`] numbers
    /// occurrences in, so occurrence `n` selects the n-th same-UUID entry
    /// in both paths. UUID alone never identifies an instance.
    fn find_characteristic(
        peripheral: &Peripheral,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Option<Characteristic> {
        let services = peripheral.services();
        select_service(&services, service_uuid, service_occurrence).and_then(|service| {
            select_characteristic(service, characteristic_uuid, characteristic_occurrence).cloned()
        })
    }

    async fn recv_notification(&self) -> Option<RadioEvent> {
        let mut queue = self.notification_rx.lock().await;
        let event = queue.recv().await?;
        if let RadioEvent::Notification { ref value, .. } = event {
            ingress_release(&self.ingress_bytes, value.len() as u64);
        }
        Some(event)
    }

    /// Retire every live forwarder of one peer at a disconnect or database
    /// change (F10, findings 40 and 129). Each forwarder first delivers the
    /// values the OS already buffered for it, so they reach the central
    /// before the event that ends the subscription; what did not fit the
    /// ingress is queued here as that subscription's loss, ahead of the
    /// event. No OS unsubscribe runs here: released link-scoped CCCDs are
    /// forgotten; a changed database or a BlueZ client notification session
    /// retained across link loss is parked as cleanup debt
    /// ([`PeerRetirement`], finding 40).
    async fn drain_peer_forwarders(&self, peer_id: &str, retire: PeerRetirement) {
        if matches!(retire, PeerRetirement::LinkEnded) {
            self.notification_targets.retire_peer(peer_id);
        }
        drain_gatt_forwarders(
            &self.forwarders,
            &self.cleanup_debt,
            &self.deferred,
            peer_id,
            retire,
        )
        .await;
    }
}

async fn drain_gatt_forwarders(
    forwarders: &StdMutex<HashMap<String, ForwarderEntry>>,
    debt: &StdMutex<HashSet<InstanceKey>>,
    deferred: &Mutex<VecDeque<RadioEvent>>,
    peer_id: &str,
    retire: PeerRetirement,
) {
    let retired = retire_peer_forwarders(
        &mut forwarders.lock().expect("forwarder table"),
        &mut debt.lock().expect("cleanup debt"),
        peer_id,
        retire,
    );
    for entry in retired {
        let scope = entry.scope();
        let lost = entry.task.drain(FORWARDER_DRAIN_BOUND).await;
        if lost > 0 {
            let epoch = entry.epoch;
            deferred
                .lock()
                .await
                .push_back(RadioEvent::NotificationsLost {
                    peer_id: scope.0,
                    service_uuid: scope.1,
                    service_occurrence: scope.2,
                    characteristic_uuid: scope.3,
                    characteristic_occurrence: scope.4,
                    epoch,
                    lost,
                });
        }
    }
}

impl BtleplugRadio {
    /// The next event whose turn has come (finding 129): notifications
    /// already in the ingress go before a deferred link or database event,
    /// so a value that arrived before a disconnect is delivered before it.
    async fn take_deferred(&self) -> Option<RadioEvent> {
        let mut deferred = self.deferred.lock().await;
        if deferred.is_empty() {
            return None;
        }
        let mut queue = self.notification_rx.lock().await;
        if let Ok(event) = queue.try_recv() {
            if let RadioEvent::Notification { ref value, .. } = event {
                ingress_release(&self.ingress_bytes, value.len() as u64);
            }
            return Some(event);
        }
        deferred.pop_front()
    }

    /// Release per-link OS state after the link ended: the Windows
    /// maintained session and the Linux per-connection facts.
    fn release_link_state(&self, peer_id: &str) -> Result<(), DesktopError> {
        if let Some(watcher) = self
            .readiness_watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(peer_id)
        {
            watcher.abort();
        }
        #[cfg(target_os = "linux")]
        if let Ok(bluez) = self.bluez() {
            bluez.forget(peer_id);
        }
        #[cfg(target_os = "windows")]
        let released = self.winrt.release(peer_id);
        #[cfg(not(target_os = "windows"))]
        let released = {
            let _ = peer_id;
            Ok(())
        };
        released
    }

    /// Forward CoreBluetooth's readiness reports for this connection as
    /// [`RadioEvent::WriteReadiness`] (macOS, vendored patch 4). Elsewhere
    /// no platform reports readiness, so nothing is watched.
    fn watch_write_readiness(&self, peer_id: &str, peripheral: &Peripheral) {
        #[cfg(all(target_os = "macos", btleplug_ubm_write_readiness))]
        {
            let mut reports = peripheral.write_readiness_events();
            let events = self._os_events_tx.clone();
            let peer = peer_id.to_owned();
            let task = self.spawn.spawn(async move {
                loop {
                    match reports.recv().await {
                        Ok(ready) => {
                            let event = RadioEvent::WriteReadiness {
                                peer_id: peer.clone(),
                                ready,
                            };
                            if events.send(event).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            OS_EVENT_DROPS.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
            if let Some(previous) = self
                .readiness_watchers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(peer_id.to_owned(), task)
            {
                previous.abort();
            }
        }
        #[cfg(not(all(target_os = "macos", btleplug_ubm_write_readiness)))]
        {
            let _ = (peer_id, peripheral);
        }
    }

    /// Rewrite the CCCD the platform just wrote (finding 39, Windows). On
    /// success the rewritten mode is what the link has. On failure the
    /// platform's own write stands: without a requirement that is reported
    /// truthfully as observed; with one the enable is rolled back and
    /// fails, never accepting a requirement nothing enforced.
    #[allow(clippy::too_many_arguments)]
    async fn rewrite_cccd(
        &self,
        scope: &InstanceKey,
        key: &str,
        mode: DeliveryMode,
        platform_writes: DeliveryMode,
        required: bool,
    ) -> Result<ObservedDelivery, DesktopError> {
        let rewrite = self.write_cccd(scope, mode).await;
        let observed = |mode: DeliveryMode| match mode {
            DeliveryMode::Notification => ObservedDelivery::Notification,
            DeliveryMode::Indication => ObservedDelivery::Indication,
        };
        match rewrite {
            Ok(()) => Ok(observed(mode)),
            Err(_) if !required => Ok(observed(platform_writes)),
            Err(error) => {
                if let Err(rollback) = release_notification_target(
                    &self.notification_targets,
                    &self.forwarders,
                    &self.cleanup_debt,
                    key,
                    scope,
                )
                .await
                {
                    let reason = format!(
                        "{}; rolling the enable back also failed ({}), the scope is \
                         parked as cleanup debt",
                        error.detail().unwrap_or(error.code_str()),
                        rollback.detail().unwrap_or(rollback.code_str())
                    );
                    return Err(error.with_detail(reason));
                }
                Err(error)
            }
        }
    }

    /// Windows: the CCCD of exactly the subscribed instance, on the GATT
    /// object btleplug's subscription holds (vendored `winrt-cccd-mode`).
    #[cfg(target_os = "windows")]
    async fn write_cccd(
        &self,
        scope: &InstanceKey,
        mode: DeliveryMode,
    ) -> Result<(), DesktopError> {
        let (peer_id, service_uuid, service_occurrence, characteristic_uuid, occurrence) = scope;
        let peripheral = self.cached_peripheral(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            *service_occurrence,
            characteristic_uuid,
            *occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                "gatt.subscribe.delivery",
            )
            .with_detail(format!(
                "characteristic {characteristic_uuid}#{occurrence} of service \
                 {service_uuid}#{service_occurrence} is no longer discovered"
            ))
        })?;
        crate::os::windows::write_cccd(&peripheral, &characteristic, mode).await
    }

    #[cfg(not(target_os = "windows"))]
    async fn write_cccd(
        &self,
        scope: &InstanceKey,
        mode: DeliveryMode,
    ) -> Result<(), DesktopError> {
        let _ = scope;
        Err(DesktopError::new(
            BleErrorCode::CapabilityLimited,
            BleErrorDomain::Capability,
            "gatt.subscribe.delivery",
        )
        .with_detail(format!(
            "this platform cannot rewrite the CCCD ({} requested)",
            mode.as_str()
        )))
    }

    fn find_descriptor(
        peripheral: &Peripheral,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
    ) -> Option<Descriptor> {
        Self::find_characteristic(
            peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .and_then(|characteristic| {
            select_descriptor(&characteristic, descriptor_uuid, descriptor_occurrence).cloned()
        })
    }
}

/// Why one peer's forwarders retire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRetirement {
    /// The link ended: the OS released every CCCD with it, so no
    /// unsubscribe is owed and the peer's cleanup debt settles.
    LinkEnded,
    /// BlueZ keeps this D-Bus client's StartNotify session across link loss.
    /// It must be explicitly stopped before a new enable; another StartNotify
    /// alone can succeed without reacquiring the server's notification source.
    LinkEndedRetainingNotifySession,
    /// The GATT database changed under a live link (finding 40): the
    /// forwarders stop (their routing identity is stale), but the OS-side
    /// CCCDs may still be live — CoreBluetooth reports every modification
    /// while unaffected services keep their subscriptions — so each scope
    /// is parked as cleanup debt: an unsubscribe of that instance or
    /// `close` still releases it, with a receipt when it cannot.
    DatabaseChanged,
}

/// Retire every forwarder of `peer_id` (F10 + finding 40): remove them
/// from the table and settle or park the native enablements by
/// [`PeerRetirement`]. The caller drains the returned forwarders (finding
/// 129) so values the OS already buffered are delivered before the link
/// loss or database change, or aborts them.
pub fn retire_peer_forwarders(
    forwarders: &mut HashMap<String, ForwarderEntry>,
    debt: &mut HashSet<InstanceKey>,
    peer_id: &str,
    retire: PeerRetirement,
) -> Vec<ForwarderEntry> {
    let stale: Vec<String> = forwarders
        .iter()
        .filter(|(_, entry)| entry.peer_id == peer_id)
        .map(|(key, _)| key.clone())
        .collect();
    let mut scopes = Vec::with_capacity(stale.len());
    let mut retired = Vec::with_capacity(stale.len());
    for key in &stale {
        if let Some(entry) = forwarders.remove(key) {
            scopes.push(entry.scope());
            retired.push(entry);
        }
    }
    match retire {
        PeerRetirement::LinkEnded => debt.retain(|scope| scope.0 != peer_id),
        PeerRetirement::DatabaseChanged | PeerRetirement::LinkEndedRetainingNotifySession => {
            debt.extend(scopes)
        }
    }
    retired
}

/// Advertisement fields upstream btleplug drops: CoreBluetooth's
/// solicited/overflow service UUIDs and connectable flag through the
/// vendored patch (UBM_PATCHES.md #2). Other platforms (and an unpatched
/// build) report none — the legacy WinRT and BlueZ backends reported none
/// either.
fn advertisement_extras(peripheral: &Peripheral) -> crate::boundary::AdvertisementExtras {
    #[cfg(all(target_os = "macos", btleplug_ubm_advertisement_extras))]
    {
        let extras = peripheral.advertisement_extras();
        let strings = |uuids: Option<Vec<uuid::Uuid>>| {
            uuids.map(|uuids| uuids.iter().map(ToString::to_string).collect())
        };
        crate::boundary::AdvertisementExtras {
            solicited_service_uuids: strings(extras.solicited_service_uuids),
            overflow_service_uuids: strings(extras.overflow_service_uuids),
            connectable: extras.connectable,
            appearance: None,
            raw_record: None,
            source: crate::boundary::ObservationSource::DeviceState,
        }
    }
    #[cfg(not(all(target_os = "macos", btleplug_ubm_advertisement_extras)))]
    {
        let _ = peripheral;
        crate::boundary::AdvertisementExtras::default()
    }
}

/// Per-instance forwarder key: duplicate UUIDs never share a forwarder.
///
/// Public so the production-path ingress harness addresses the same table
/// slots production does.
#[must_use]
pub fn forwarder_key(
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
) -> String {
    format!(
        "{peer_id}#{service_uuid}:{service_occurrence}#{characteristic_uuid}:{characteristic_occurrence}"
    )
}

/// Identity one notification forwarder routes by (F09): the owning
/// service and the characteristic, each as UUID plus the vendored
/// attribute instance ([`ValueNotification`] carries both, UBM_PATCHES.md
/// #6). A value is routed only to the exact instance that fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Identity one notification forwarder routes by; see the field notes below.
/// Public so the production-path ingress harness filters scripted streams
/// through the genuine predicate.
pub struct NotificationRoute {
    /// Owning service identity from [`ValueNotification::service_uuid`].
    pub service_uuid: uuid::Uuid,
    /// Owning service instance from [`ValueNotification::service_instance`].
    pub service_instance: u64,
    /// Characteristic identity from [`ValueNotification::uuid`].
    pub characteristic_uuid: uuid::Uuid,
    /// Characteristic instance from [`ValueNotification::instance`].
    pub characteristic_instance: u64,
}

impl NotificationRoute {
    /// Build the route for one subscribed instance.
    #[must_use]
    pub fn new(
        service_uuid: uuid::Uuid,
        service_instance: u64,
        characteristic_uuid: uuid::Uuid,
        characteristic_instance: u64,
    ) -> Self {
        Self {
            service_uuid,
            service_instance,
            characteristic_uuid,
            characteristic_instance,
        }
    }

    /// The route for one discovered characteristic.
    #[must_use]
    pub fn of(characteristic: &Characteristic) -> Self {
        Self::new(
            characteristic.service_uuid,
            characteristic.service_instance,
            characteristic.uuid,
            characteristic.instance,
        )
    }

    /// True when this notification belongs to the subscribed instance:
    /// the service and the characteristic identity, UUID and instance,
    /// must all match, or bytes for one instance would misroute into
    /// another same-UUID subscription.
    #[must_use]
    pub fn matches(&self, note: &ValueNotification) -> bool {
        note.service_uuid == self.service_uuid
            && note.service_instance == self.service_instance
            && note.uuid == self.characteristic_uuid
            && note.instance == self.characteristic_instance
    }
}

/// Fold one native-unsubscribe outcome into the forwarder table (F13).
/// The forwarder is removed only when the native disable succeeds, so a
/// still-enabled CCCD keeps its consumer and values keep flowing until a
/// retry disables it (the central's L7 path relies on this). When no
/// consumer exists to preserve, a failed disable parks the scope as
/// cleanup debt for retry/dispose instead of vanishing.
///
/// Public so the production-path ingress harness folds scripted native
/// outcomes through the genuine teardown path.
pub fn apply_unsubscribe_outcome(
    forwarders: &mut HashMap<String, ForwarderEntry>,
    debt: &mut HashSet<InstanceKey>,
    key: &str,
    scope: &InstanceKey,
    unsubscribed: bool,
) {
    if unsubscribed {
        if let Some(entry) = forwarders.remove(key) {
            entry.task.abort();
        }
        debt.remove(scope);
    } else if !forwarders.contains_key(key) {
        // No consumer to preserve: retain the unresolved native
        // resource as cleanup debt for retry/dispose.
        debt.insert(scope.clone());
    }
    // Otherwise the forwarder stays installed: the CCCD is still live,
    // so values must keep flowing until a retry disables it.
}

/// Atomically reserve `len` bytes of bounded ingress (F07): succeeds only
/// when the post-reservation total stays within [`NOTIFICATION_BYTES`].
/// A compare-exchange loop (never load-then-add): concurrent forwarders
/// racing the same counter cannot jointly overshoot the cap, and an
/// oversized single item is refused up front. Every success must pair with
/// exactly one [`ingress_release`] (on dequeue) or one rollback (on a
/// failed `try_send`); every refusal is an explicit counted drop.
#[must_use]
pub fn ingress_try_reserve(queued_bytes: &AtomicU64, len: u64) -> bool {
    let mut current = queued_bytes.load(Ordering::Relaxed);
    loop {
        let reserved = current.saturating_add(len);
        if reserved > NOTIFICATION_BYTES {
            return false;
        }
        match queued_bytes.compare_exchange_weak(
            current,
            reserved,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// Release `len` bytes of bounded ingress on dequeue (F07): the exact
/// counterpart of a [`ingress_try_reserve`] success, keeping the queued
/// total equal to the bytes actually sitting in the channel. Called by the
/// consumer that just dequeued the owning item, mirroring
/// `BtleplugRadio::recv_notification`.
pub fn ingress_release(queued_bytes: &AtomicU64, len: u64) {
    queued_bytes.fetch_sub(len, Ordering::Relaxed);
}

/// Narrow native leaf the notification setup/teardown sequencing depends on
/// (F13 production-path seam): subscribe, stream acquisition, and
/// unsubscribe for one characteristic. The blanket implementation covers
/// every [`btleplug::api::Peripheral`]; the harness implements this trait
/// directly with scripted outcomes, so sequencing tests execute the genuine
/// production functions with only the OS leaf stubbed. Everything below
/// this trait (D-Bus session, adapter enumeration, peripheral lookup, the
/// adapter event stream) stays hardware-gated: btleplug 0.12 exposes no
/// public constructor for its platform peripheral or adapter
/// (`Peripheral::new`, `Adapter::new`, and `DeviceId::new` are all
/// `pub(crate)`), and `Manager::new` requires a live BlueZ D-Bus session.
pub trait NotificationTransport: Send + Sync {
    /// Enable the native CCCD for `characteristic`.
    fn transport_subscribe<'a>(
        &'a self,
        characteristic: &'a Characteristic,
    ) -> impl Future<Output = Result<(), btleplug::Error>> + Send + 'a;

    /// Acquire the peripheral-wide notification stream.
    fn transport_notifications(
        &self,
    ) -> impl Future<Output = Result<NotificationStream, btleplug::Error>> + Send + '_;

    /// Disable the native CCCD for `characteristic`.
    fn transport_unsubscribe<'a>(
        &'a self,
        characteristic: &'a Characteristic,
    ) -> impl Future<Output = Result<(), btleplug::Error>> + Send + 'a;
}

struct NotificationTarget<T> {
    transport: T,
    characteristic: Characteristic,
    release: StdMutex<
        Option<
            futures_util::future::Shared<
                futures_util::future::BoxFuture<'static, NativeNotificationRelease>,
            >,
        >,
    >,
}

#[derive(Clone)]
struct NativeNotificationRelease {
    outcome: Result<(), DesktopError>,
    indeterminate: bool,
}

async fn release_original_notification_target<T: NotificationTransport + Clone + 'static>(
    target: &Arc<NotificationTarget<T>>,
) -> Result<(), DesktopError> {
    let release = {
        let mut retained = target
            .release
            .lock()
            .expect("original notification release");
        if retained.as_ref().is_some_and(|reply| {
            reply
                .peek()
                .is_some_and(|result| result.outcome.is_err() && !result.indeterminate)
        }) {
            *retained = None;
        }
        retained
            .get_or_insert_with(|| {
                let transport = target.transport.clone();
                let characteristic = target.characteristic.clone();
                async move {
                    let result = transport.transport_unsubscribe(&characteristic).await;
                    let indeterminate = result
                        .as_ref()
                        .is_err_and(notification_error_is_indeterminate);
                    let outcome = match result {
                        Err(error) if notification_error_confirms_retired(&error) => Ok(()),
                        other => other.map_err(|error| {
                            DesktopError::subscribe_failed(error.to_string()).with_os(&error)
                        }),
                    };
                    NativeNotificationRelease {
                        outcome,
                        indeterminate,
                    }
                }
                .boxed()
                .shared()
            })
            .clone()
    };
    release.await.outcome
}

/// A sidecar of exact native targets, not a second obligation ledger. Live
/// forwarders or cleanup_debt own each entry until confirmed retirement.
struct NotificationTargets<T> {
    entries: StdMutex<HashMap<InstanceKey, Arc<NotificationTarget<T>>>>,
}

impl<T> Default for NotificationTargets<T> {
    fn default() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
        }
    }
}

impl<T> NotificationTargets<T> {
    fn retain(
        &self,
        scope: &InstanceKey,
        transport: T,
        characteristic: Characteristic,
    ) -> Result<Arc<NotificationTarget<T>>, DesktopError> {
        let mut entries = self.entries.lock().expect("notification targets");
        if entries.contains_key(scope) {
            return Err(DesktopError::new(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Cleanup,
                "gatt.subscribe",
            )
            .with_detail("the original native notification target still owns cleanup"));
        }
        let target = Arc::new(NotificationTarget {
            transport,
            characteristic,
            release: StdMutex::new(None),
        });
        entries.insert(scope.clone(), target.clone());
        Ok(target)
    }
    fn get(&self, scope: &InstanceKey) -> Option<Arc<NotificationTarget<T>>> {
        self.entries
            .lock()
            .expect("notification targets")
            .get(scope)
            .cloned()
    }
    fn remove_if(&self, scope: &InstanceKey, target: &Arc<NotificationTarget<T>>) -> bool {
        let mut entries = self.entries.lock().expect("notification targets");
        if entries
            .get(scope)
            .is_some_and(|held| Arc::ptr_eq(held, target))
        {
            entries.remove(scope);
            return true;
        }
        false
    }
    fn retire_peer(&self, peer_id: &str) {
        self.entries
            .lock()
            .expect("notification targets")
            .retain(|scope, _| scope.0 != peer_id);
    }
}

async fn subscribe_and_stream_owned<T: NotificationTransport + Clone>(
    transport: &T,
    characteristic: &Characteristic,
    targets: &NotificationTargets<T>,
    debt: &StdMutex<HashSet<InstanceKey>>,
    scope: &InstanceKey,
) -> Result<NotificationStream, EnableStreamError> {
    let admitted = StdMutex::new(None);
    subscribe_and_stream_admitted(
        transport,
        characteristic,
        || {
            let target = targets.retain(scope, transport.clone(), characteristic.clone())?;
            *admitted.lock().expect("admitted notification target") = Some(target);
            debt.lock().expect("cleanup debt").insert(scope.clone());
            Ok(())
        },
        |error| {
            // Only the operation's own structured pre-effect refusal retires
            // this admission. Cancellation and ambiguous failures keep ownership;
            // a failed retain must never erase a previously admitted target.
            if notification_enable_confirms_no_effect(error)
                && let Some(target) = admitted
                    .lock()
                    .expect("admitted notification target")
                    .as_ref()
                && targets.remove_if(scope, target)
            {
                debt.lock().expect("cleanup debt").remove(scope);
            }
        },
    )
    .await
}

async fn release_notification_target<T: NotificationTransport + Clone + 'static>(
    targets: &NotificationTargets<T>,
    forwarders: &StdMutex<HashMap<String, ForwarderEntry>>,
    debt: &StdMutex<HashSet<InstanceKey>>,
    key: &str,
    scope: &InstanceKey,
) -> Result<(), DesktopError> {
    let Some(target) = targets.get(scope) else {
        if debt.lock().expect("cleanup debt").contains(scope)
            || forwarders
                .lock()
                .expect("forwarder table")
                .contains_key(key)
        {
            return Err(DesktopError::subscribe_failed(
                "the original native notification cleanup target is absent",
            ));
        }
        // A failed stream acquisition admitted no native enable. Never issue
        // an unowned StopNotify just to fabricate a cleanup receipt.
        return Ok(());
    };
    let result = release_original_notification_target(&target).await;
    apply_unsubscribe_outcome(
        &mut forwarders.lock().expect("forwarder table"),
        &mut debt.lock().expect("cleanup debt"),
        key,
        scope,
        result.is_ok(),
    );
    result?;
    targets.remove_if(scope, &target);
    Ok(())
}

async fn retire_notification_debt_for_peer<T: NotificationTransport + Clone + 'static>(
    targets: &NotificationTargets<T>,
    forwarders: &StdMutex<HashMap<String, ForwarderEntry>>,
    debt: &StdMutex<HashSet<InstanceKey>>,
    peer: &str,
) -> Result<(), DesktopError> {
    let scopes: Vec<_> = debt
        .lock()
        .expect("cleanup debt")
        .iter()
        .filter(|scope| scope.0 == peer)
        .cloned()
        .collect();
    for scope in scopes {
        tokio::time::timeout(
            CLOSE_SCOPE_BOUND,
            release_notification_target(
                targets,
                forwarders,
                debt,
                &forwarder_key(&scope.0, &scope.1, scope.2, &scope.3, scope.4),
                &scope,
            ),
        )
        .await
        .map_err(|_| {
            DesktopError::subscribe_failed(
                "original notification retirement exceeded its cleanup bound",
            )
        })??;
    }
    Ok(())
}

/// Only the pinned original StopNotify object's own absence reply retires
/// its obligation. BlueZ frees all sender notify clients on characteristic
/// object destruction; cached topology absence is deliberately not proof.
fn notification_error_confirms_retired(error: &btleplug::Error) -> bool {
    matches!(error, btleplug::Error::Platform(detail)
        if detail.domain == "bluez-dbus"
        && detail.code == "org.freedesktop.DBus.Error.UnknownObject")
}

fn notification_error_is_indeterminate(error: &btleplug::Error) -> bool {
    matches!(
        error,
        btleplug::Error::TimedOut(_) | btleplug::Error::Other(_)
    ) || matches!(error, btleplug::Error::Platform(detail) if matches!(
            detail.code.as_str(), "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Timeout" | "org.freedesktop.DBus.Error.TimedOut"))
}

fn notification_enable_confirms_no_effect(error: &btleplug::Error) -> bool {
    if notification_error_is_indeterminate(error) {
        return false;
    }
    // These typed local refusals precede dispatch. Do not generalize to
    // RuntimeError, generic Failed, or InProgress (an existing notify client).
    matches!(
        error,
        btleplug::Error::NotSupported(_) | btleplug::Error::NoSuchCharacteristic
    ) || matches!(error, btleplug::Error::Platform(detail)
            if detail.domain == "bluez-dbus" && matches!(detail.code.as_str(),
                // BlueZ 5.87 gatt-client.c characteristic_start_notify returns
                // these before notify_client_create/registration.
                "org.bluez.Error.NotConnected" | "org.bluez.Error.NotPermitted"
                | "org.bluez.Error.NotSupported"
                // D-Bus dispatch refused before invoking StartNotify.
                | "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.AccessDenied"))
}

impl<T> NotificationTransport for T
where
    T: btleplug::api::Peripheral,
{
    fn transport_subscribe<'a>(
        &'a self,
        characteristic: &'a Characteristic,
    ) -> impl Future<Output = Result<(), btleplug::Error>> + Send + 'a {
        btleplug::api::Peripheral::subscribe(self, characteristic)
    }

    fn transport_notifications(
        &self,
    ) -> impl Future<Output = Result<NotificationStream, btleplug::Error>> + Send + '_ {
        btleplug::api::Peripheral::notifications(self)
    }

    fn transport_unsubscribe<'a>(
        &'a self,
        characteristic: &'a Characteristic,
    ) -> impl Future<Output = Result<(), btleplug::Error>> + Send + 'a {
        btleplug::api::Peripheral::unsubscribe(self, characteristic)
    }
}

/// How the production enable sequencing failed (F13, finding 128): the value
/// stream could not open (nothing enabled), or the native enable was
/// refused after it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnableStreamError {
    /// Native subscribe refused after the stream opened; the stream is
    /// dropped. The refusal keeps the platform's answer (finding 113).
    Subscribe(DesktopError),
    /// The value stream could not open; nothing was enabled, so nothing is
    /// owed (finding 128).
    Stream(DesktopError),
}

/// Production enable sequencing for one characteristic (F13, finding 128):
/// the value stream opens first, then the native enable, so a value the
/// peer sends the moment its CCCD is written is not lost (btleplug's stream
/// only receives values sent after it opens; Tauri 4.x and the legacy
/// CoreBluetooth path registered the consumer first too). A stream that
/// cannot open enables nothing; a refused enable drops the stream. Called
/// by [`BtleplugRadio::set_notifications`] on the real peripheral and by
/// the production-path ingress harness on a scripted transport — one shared
/// implementation, not a reimplementation.
pub async fn subscribe_and_stream<T>(
    transport: &T,
    characteristic: &Characteristic,
) -> Result<NotificationStream, EnableStreamError>
where
    T: NotificationTransport,
{
    subscribe_and_stream_admitted(transport, characteristic, || Ok(()), |_| {}).await
}

async fn subscribe_and_stream_admitted<T: NotificationTransport>(
    transport: &T,
    characteristic: &Characteristic,
    before_enable: impl FnOnce() -> Result<(), DesktopError>,
    enable_refused: impl FnOnce(&btleplug::Error),
) -> Result<NotificationStream, EnableStreamError> {
    let stream = transport.transport_notifications().await.map_err(|error| {
        EnableStreamError::Stream(DesktopError::subscribe_failed(error.to_string()).with_os(&error))
    })?;
    before_enable().map_err(EnableStreamError::Subscribe)?;
    transport
        .transport_subscribe(characteristic)
        .await
        .map_err(|error| {
            enable_refused(&error);
            EnableStreamError::Subscribe(
                DesktopError::subscribe_failed(error.to_string()).with_os(&error),
            )
        })?;
    Ok(stream)
}

/// Production disable sequencing for one characteristic (F13): the native
/// disable runs BEFORE the forwarder table is touched — only a successful
/// unsubscribe removes the consumer, so a still-enabled CCCD keeps
/// forwarding until a retry disables it. The outcome is folded through
/// [`apply_unsubscribe_outcome`] under the table locks (acquired only after
/// the native call resolves, never held across it). Called by
/// [`BtleplugRadio::set_notifications`] and by the production-path ingress
/// harness alike; the refusal returns as `gatt.subscribe-failed` with the
/// platform's answer (finding 124).
pub async fn unsubscribe_and_fold<T>(
    transport: &T,
    characteristic: &Characteristic,
    forwarders: &StdMutex<HashMap<String, ForwarderEntry>>,
    debt: &StdMutex<HashSet<InstanceKey>>,
    key: &str,
    scope: &InstanceKey,
) -> Result<(), DesktopError>
where
    T: NotificationTransport,
{
    let outcome = transport.transport_unsubscribe(characteristic).await;
    let outcome = match outcome {
        Err(error) if notification_error_confirms_retired(&error) => Ok(()),
        other => other,
    };
    apply_unsubscribe_outcome(
        &mut forwarders.lock().expect("forwarder table"),
        &mut debt.lock().expect("cleanup debt"),
        key,
        scope,
        outcome.is_ok(),
    );
    // Finding 124: a refused disable keeps the platform's answer.
    outcome.map_err(|error| DesktopError::subscribe_failed(error.to_string()).with_os(&error))
}

/// Resolve only this radio owner's parked notification scope before reuse.
/// Failed or cancelled native stops leave the obligation retryable; unrelated
/// scopes and other D-Bus clients' subscriptions are not touched.
pub async fn retire_parked_subscription<T>(
    transport: &T,
    characteristic: &Characteristic,
    forwarders: &StdMutex<HashMap<String, ForwarderEntry>>,
    debt: &StdMutex<HashSet<InstanceKey>>,
    key: &str,
    scope: &InstanceKey,
) -> Result<(), DesktopError>
where
    T: NotificationTransport,
{
    let retained = debt.lock().expect("cleanup debt").contains(scope);
    if retained {
        unsubscribe_and_fold(transport, characteristic, forwarders, debt, key, scope).await?;
    }
    Ok(())
}

/// Address one notification forwarder stamps on every value it emits (F10):
/// the subscribed instance plus the install-time epoch. Values already
/// queued still carry the dead install-time epoch and fail the central's
/// routing check after a disconnect or service change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardTarget {
    /// Peer the subscription belongs to.
    pub peer_id: String,
    /// Owning service UUID (event label).
    pub service_uuid: String,
    /// Occurrence among duplicate service UUIDs.
    pub service_occurrence: u64,
    /// Subscribed characteristic UUID (event label).
    pub characteristic_uuid: String,
    /// Occurrence among duplicate characteristic UUIDs.
    pub characteristic_occurrence: u64,
    /// Subscription epoch captured at install, never minted at dequeue.
    pub epoch: u64,
}

/// Spawn the production notification forwarder (F07/F09/F10) over an
/// injected stream: filter on the full (service, characteristic) identity,
/// reserve byte capacity atomically before the owned handoff, and `try_send`
/// (never block the runtime worker) with explicit counted drops. The CCCD
/// stays enabled across drops so later values still flow after the drain.
/// Called by [`BtleplugRadio::set_notifications`] on the genuine btleplug
/// stream and by the production-path ingress harness on scripted streams —
/// one shared implementation.
#[allow(clippy::too_many_arguments)]
pub fn spawn_notification_forwarder<S>(
    spawn: &tokio::runtime::Handle,
    stream: S,
    route: NotificationRoute,
    target: ForwardTarget,
    sender: mpsc::Sender<RadioEvent>,
    queued_bytes: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
) -> ForwarderTask
where
    S: futures_util::Stream<Item = ValueNotification> + Send + 'static,
{
    let (drain, mut drain_requested) = tokio::sync::watch::channel(false);
    let handle = spawn.spawn(async move {
        let mut stream = Box::pin(stream);
        // Notifications this subscription lost and has not reported yet:
        // values its receiver missed on the OS broadcast (vendored patch
        // 10), and values the full ingress refused (finding 131). Reported
        // on this scope as soon as the ingress has room, never dropped.
        let mut unreported_loss = 0u64;
        loop {
            let step = if unreported_loss > 0 {
                tokio::select! {
                    biased;
                    () = drain_signal(&mut drain_requested) => ForwardStep::Drain,
                    permit = sender.reserve() => match permit {
                        Ok(permit) => {
                            permit.send(loss_report(&target, unreported_loss));
                            unreported_loss = 0;
                            continue;
                        }
                        Err(_) => return unreported_loss,
                    },
                    note = stream.next() => ForwardStep::Note(note),
                }
            } else {
                tokio::select! {
                    biased;
                    () = drain_signal(&mut drain_requested) => ForwardStep::Drain,
                    note = stream.next() => ForwardStep::Note(note),
                }
            };
            match step {
                ForwardStep::Note(Some(note)) => {
                    match forward_one(
                        note,
                        &route,
                        &target,
                        &sender,
                        &queued_bytes,
                        &dropped,
                        &mut unreported_loss,
                    ) {
                        Forwarded::Open => {}
                        Forwarded::Closed => return unreported_loss,
                    }
                }
                ForwardStep::Note(None) => {
                    // The OS stream ended: its last loss is still owed to
                    // the subscription.
                    if unreported_loss > 0
                        && sender
                            .send(loss_report(&target, unreported_loss))
                            .await
                            .is_ok()
                    {
                        unreported_loss = 0;
                    }
                    return unreported_loss;
                }
                ForwardStep::Drain => {
                    // Finding 129: forward every value the OS already
                    // buffered, then stop; what does not fit returns to the
                    // drainer as this subscription's loss.
                    while let Some(Some(note)) = stream.next().now_or_never() {
                        if matches!(
                            forward_one(
                                note,
                                &route,
                                &target,
                                &sender,
                                &queued_bytes,
                                &dropped,
                                &mut unreported_loss,
                            ),
                            Forwarded::Closed
                        ) {
                            break;
                        }
                    }
                    return unreported_loss;
                }
            }
        }
    });
    ForwarderTask { handle, drain }
}

enum ForwardStep {
    Note(Option<ValueNotification>),
    Drain,
}

enum Forwarded {
    Open,
    Closed,
}

/// Resolves when a drain is requested; never when the requester is gone
/// without asking (a replaced or aborted entry).
async fn drain_signal(requested: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *requested.borrow_and_update() {
            return;
        }
        if requested.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Forward one OS notification through the bounded ingress (F07): its
/// broadcast loss and any refusal of the full ingress add to the
/// subscription's unreported loss (finding 131) as well as the global drop
/// counter.
fn forward_one(
    note: ValueNotification,
    route: &NotificationRoute,
    target: &ForwardTarget,
    sender: &mpsc::Sender<RadioEvent>,
    queued_bytes: &AtomicU64,
    dropped: &AtomicU64,
    unreported_loss: &mut u64,
) -> Forwarded {
    *unreported_loss = unreported_loss.saturating_add(note.lost_before);
    // The loss is reported before the value that follows it (patch 10).
    if *unreported_loss > 0 {
        match sender.try_send(loss_report(target, *unreported_loss)) {
            Ok(()) => *unreported_loss = 0,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Forwarded::Closed,
        }
    }
    if !route.matches(&note) {
        return Forwarded::Open;
    }
    let len = note.value.len() as u64;
    if !ingress_try_reserve(queued_bytes, len) {
        dropped.fetch_add(1, Ordering::Relaxed);
        *unreported_loss = unreported_loss.saturating_add(1);
        return Forwarded::Open;
    }
    let event = RadioEvent::Notification {
        peer_id: target.peer_id.clone(),
        service_uuid: target.service_uuid.clone(),
        service_occurrence: target.service_occurrence,
        characteristic_uuid: target.characteristic_uuid.clone(),
        characteristic_occurrence: target.characteristic_occurrence,
        epoch: target.epoch,
        value: note.value,
    };
    match sender.try_send(event) {
        Ok(()) => Forwarded::Open,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            // The reservation never materialized: release it.
            ingress_release(queued_bytes, len);
            dropped.fetch_add(1, Ordering::Relaxed);
            *unreported_loss = unreported_loss.saturating_add(1);
            Forwarded::Open
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            ingress_release(queued_bytes, len);
            Forwarded::Closed
        }
    }
}

/// Forwarders that did not drain within their bound at a disconnect
/// (finding 129). Counted and logged, never silent.
static FORWARDER_DRAIN_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Bound for draining one forwarder at a disconnect or database change: it
/// only forwards what the OS already buffered, never waits for more.
const FORWARDER_DRAIN_BOUND: Duration = Duration::from_secs(1);

/// One subscription's loss, reported on its scope and epoch.
fn loss_report(target: &ForwardTarget, lost: u64) -> RadioEvent {
    RadioEvent::NotificationsLost {
        peer_id: target.peer_id.clone(),
        service_uuid: target.service_uuid.clone(),
        service_occurrence: target.service_occurrence,
        characteristic_uuid: target.characteristic_uuid.clone(),
        characteristic_occurrence: target.characteristic_occurrence,
        epoch: target.epoch,
        lost,
    }
}

/// One subscription's forwarder task (findings 129, 131): abort it at a
/// teardown that owes nothing, or drain it at a disconnect so every value
/// the OS already buffered is delivered or returned as loss.
pub struct ForwarderTask {
    handle: tokio::task::JoinHandle<u64>,
    drain: tokio::sync::watch::Sender<bool>,
}

impl ForwarderTask {
    /// Stop now; buffered values are not forwarded.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Whether the task ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Forward what the OS already buffered, then stop. Answers the
    /// subscription's notifications that could not be delivered (the
    /// ingress was full) and were not reported yet. A drain that outruns
    /// `bound` is aborted, logged and counted (`drain_timeouts`).
    pub async fn drain(self, bound: Duration) -> u64 {
        let _ = self.drain.send(true);
        let abort = self.handle.abort_handle();
        match tokio::time::timeout(bound, self.handle).await {
            Ok(Ok(lost)) => lost,
            Ok(Err(_)) => 0,
            Err(_) => {
                abort.abort();
                FORWARDER_DRAIN_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "ubm-desktop: a notification forwarder did not drain within {} ms",
                    bound.as_millis()
                );
                0
            }
        }
    }
}

impl std::future::IntoFuture for ForwarderTask {
    type Output = Result<u64, tokio::task::JoinError>;
    type IntoFuture = tokio::task::JoinHandle<u64>;

    fn into_future(self) -> Self::IntoFuture {
        self.handle
    }
}

impl std::fmt::Debug for ForwarderTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwarderTask")
            .field("finished", &self.handle.is_finished())
            .finish()
    }
}

impl From<tokio::task::JoinHandle<()>> for ForwarderTask {
    /// A task with nothing to drain (tests, placeholders).
    fn from(handle: tokio::task::JoinHandle<()>) -> Self {
        let handle = tokio::spawn(async move {
            let _ = handle.await;
            0
        });
        Self {
            handle,
            drain: tokio::sync::watch::channel(false).0,
        }
    }
}

/// BlueZ's per-mode write limits. `WriteValue` performs the long write for
/// a with-response request, so a request carries a whole attribute value; a
/// command is one ATT payload of the MTU BlueZ reports. A BlueZ that does
/// not report the MTU (`GattCharacteristic1.MTU` is absent before 5.62)
/// gets no length gate for commands either: the legacy BlueZ backend had
/// none and let BlueZ answer (finding 97), so no guessed 23-byte MTU refuses
/// a write the OS would take.
#[cfg(any(target_os = "linux", test))]
fn bluez_write_limits(mtu: Option<u16>) -> WriteLimits {
    let unmeasured = WriteLimits {
        with_response: crate::boundary::ATT_MAX_ATTRIBUTE_VALUE,
        without_response: crate::boundary::ATT_MAX_ATTRIBUTE_VALUE,
    };
    mtu.and_then(WriteLimits::os_long_write)
        .unwrap_or(unmeasured)
}

/// The discovered database as the snapshot the central registers, in
/// discovery order (finding 96): btleplug's sets iterate UUID-first, so each
/// level is re-sorted by `instance` (the ATT handle, or the CoreBluetooth
/// discovery position, UBM_PATCHES.md #6), as the legacy backends reported
/// it. Occurrences count per UUID in that order, which is the order
/// [`select_service`] / [`select_characteristic`] resolve them in.
fn service_snapshots(services: &BTreeSet<Service>) -> Vec<ServiceSnapshot> {
    fn in_discovery_order<'a, T: 'a>(
        items: impl IntoIterator<Item = &'a T>,
        instance: impl Fn(&T) -> u64,
    ) -> Vec<&'a T> {
        let mut ordered: Vec<&T> = items.into_iter().collect();
        ordered.sort_by_key(|item| instance(item));
        ordered
    }
    fn occurrence(counts: &mut HashMap<uuid::Uuid, u64>, uuid: uuid::Uuid) -> u64 {
        let next = counts.entry(uuid).or_insert(0);
        let occurrence = *next;
        *next += 1;
        occurrence
    }
    let mut service_counts = HashMap::new();
    in_discovery_order(services, |service| service.instance)
        .into_iter()
        .map(|service| {
            let mut char_counts = HashMap::new();
            ServiceSnapshot {
                uuid: service.uuid.to_string(),
                occurrence: occurrence(&mut service_counts, service.uuid),
                characteristics: in_discovery_order(&service.characteristics, |c| c.instance)
                    .into_iter()
                    .map(|characteristic| {
                        let mut desc_counts = HashMap::new();
                        CharacteristicSnapshot {
                            uuid: characteristic.uuid.to_string(),
                            occurrence: occurrence(&mut char_counts, characteristic.uuid),
                            properties: property_flags(characteristic.properties),
                            descriptors: in_discovery_order(&characteristic.descriptors, |d| {
                                d.instance
                            })
                            .into_iter()
                            .map(|descriptor| DescriptorSnapshot {
                                uuid: descriptor.uuid.to_string(),
                                occurrence: occurrence(&mut desc_counts, descriptor.uuid),
                            })
                            .collect(),
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Select the `occurrence`-th service with `uuid` in canonical
/// (UUID-first) order. `None` when the instance does not exist.
fn select_service<'s>(
    services: &'s BTreeSet<Service>,
    uuid: &str,
    occurrence: u64,
) -> Option<&'s Service> {
    let want = usize::try_from(occurrence).ok()?;
    services
        .iter()
        .filter(|service| service.uuid.to_string() == uuid)
        .nth(want)
}

/// Select the `occurrence`-th characteristic with `uuid` under one
/// service instance, in canonical order.
fn select_characteristic<'s>(
    service: &'s Service,
    uuid: &str,
    occurrence: u64,
) -> Option<&'s Characteristic> {
    let want = usize::try_from(occurrence).ok()?;
    service
        .characteristics
        .iter()
        .filter(|characteristic| characteristic.uuid.to_string() == uuid)
        .nth(want)
}

/// Select the `occurrence`-th descriptor with `uuid` under one
/// characteristic instance, in canonical order.
fn select_descriptor<'s>(
    characteristic: &'s Characteristic,
    uuid: &str,
    occurrence: u64,
) -> Option<&'s Descriptor> {
    let want = usize::try_from(occurrence).ok()?;
    characteristic
        .descriptors
        .iter()
        .filter(|descriptor| descriptor.uuid.to_string() == uuid)
        .nth(want)
}

fn map_radio(
    operation: &'static str,
    code: ubm_core::contracts::BleErrorCode,
    domain: ubm_core::contracts::BleErrorDomain,
) -> impl Fn(btleplug::Error) -> DesktopError {
    move |error| {
        let unsupported = matches!(&error, btleplug::Error::Platform(platform)
            if (platform.domain == "bluez-le-gatt"
                && matches!(platform.code.as_str(), "unsupported" | "unsupported-version"))
                || (platform.domain == "bluez-dbus"
                    && platform.metadata.iter().any(|(key, value)| *key == "mechanism" && value == "le-gatt-snapshot")
                    && platform.metadata.iter().any(|(key, value)| *key == "capability" && value == "unsupported")));
        let (code, domain) = if operation == "discovery.complete" && unsupported {
            (
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
            )
        } else if operation == "discovery.complete" && matches!(error, btleplug::Error::TimedOut(_))
        {
            (BleErrorCode::OperationTimedOut, BleErrorDomain::Connection)
        } else {
            (code, domain)
        };
        DesktopError::new(code, domain, operation)
            .with_detail(error.to_string())
            .with_os(&error)
    }
}

#[cfg(any(target_os = "macos", test))]
fn map_directory_error(operation: &'static str) -> impl Fn(btleplug::Error) -> DesktopError {
    move |error| {
        let state = match &error {
            btleplug::Error::Platform(detail) if detail.domain == "corebluetooth" => {
                match detail.code.as_str() {
                    "manager-state-0" => Some(AdapterPowerState::Unknown),
                    "manager-state-1" => Some(AdapterPowerState::Resetting),
                    "manager-state-2" => Some(AdapterPowerState::Unsupported),
                    "manager-state-3" => Some(AdapterPowerState::Unauthorized),
                    "manager-state-4" => Some(AdapterPowerState::PoweredOff),
                    _ => None,
                }
            }
            _ => None,
        };
        state
            .and_then(|power| {
                crate::central::admission_refusal(
                    crate::boundary::AdmissionPolicy::CoreBluetooth,
                    crate::central::AdapterStatus {
                        power: Some(power),
                        authorization: None,
                        availability: if power == AdapterPowerState::Unsupported {
                            crate::boundary::AdapterAvailability::Unsupported
                        } else {
                            crate::boundary::AdapterAvailability::Available
                        },
                        lost: false,
                    },
                    operation,
                )
            })
            .unwrap_or_else(|| {
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::PlatformFailure,
                    ubm_core::contracts::BleErrorDomain::Platform,
                    operation,
                )
            })
            .with_detail(error.to_string())
            .with_os(&error)
    }
}

fn property_flags(flags: CharPropFlags) -> PropertyFlags {
    PropertyFlags {
        read: flags.contains(CharPropFlags::READ),
        write: flags.contains(CharPropFlags::WRITE),
        write_without_response: flags.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
        notify: flags.contains(CharPropFlags::NOTIFY),
        indicate: flags.contains(CharPropFlags::INDICATE),
    }
}

/// Translate one btleplug manufacturer-data map into snapshot sections
/// (F22): company IDs plus payload bytes verbatim, sorted by company ID
/// so the unordered OS map yields a deterministic snapshot. Empty payloads
/// are preserved (section present), never dropped.
/// True when a disconnect error proves the peer is no longer present
/// (T-R1, legacy `error_confirms_device_released`): the D-Bus error *name*,
/// a protocol constant rather than rendered text. Off Linux nothing reaches
/// here — CoreBluetooth and WinRT report a missing peer as `Ok(false)`
/// rather than as an error — so no `cfg` gate is needed.
fn disconnect_error_confirms_released(error: &btleplug::Error) -> bool {
    match error {
        btleplug::Error::Platform(detail) => matches!(
            detail.code.as_str(),
            "org.freedesktop.DBus.Error.UnknownObject" | "org.bluez.Error.DoesNotExist"
        ),
        _ => false,
    }
}

/// The btleplug identity a peer id names (finding 127): the CoreBluetooth
/// identifier, the WinRT address. `None` on Linux, where BlueZ resolves
/// peers itself, or for an id that names neither.
fn platform_peripheral_id(peer_id: &str) -> Option<PeripheralId> {
    #[cfg(target_vendor = "apple")]
    {
        uuid::Uuid::parse_str(peer_id).ok().map(PeripheralId::from)
    }
    #[cfg(target_os = "windows")]
    {
        peer_id
            .parse::<btleplug::api::BDAddr>()
            .ok()
            .map(PeripheralId::from)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "windows")))]
    {
        let _ = peer_id;
        None
    }
}

fn canonical_platform_peer_id(peer_id: &str) -> String {
    platform_peripheral_id(peer_id)
        .map(|identity| identity.to_string())
        .unwrap_or_else(|| peer_id.to_owned())
}

/// The peripheral's address, when the OS reports one (CoreBluetooth hides
/// it).
fn peripheral_address(peripheral: &Peripheral) -> Option<String> {
    let raw = peripheral.address().to_string();
    (!raw.is_empty() && raw != "00:00:00:00:00:00").then_some(raw)
}

fn sorted_manufacturer_data(sections: &HashMap<u16, Vec<u8>>) -> Vec<ManufacturerData> {
    let mut entries: Vec<ManufacturerData> = sections
        .iter()
        .map(|(company_id, payload)| ManufacturerData {
            company_id: *company_id,
            payload: payload.clone(),
        })
        .collect();
    entries.sort_by_key(|entry| entry.company_id);
    entries
}

/// Translate one btleplug service-data map into snapshot sections (F22):
/// UUIDs plus payload bytes verbatim, sorted by UUID so the unordered OS
/// map yields a deterministic snapshot. Empty payloads are preserved
/// (section present), never dropped.
fn sorted_service_data(sections: &HashMap<uuid::Uuid, Vec<u8>>) -> Vec<ServiceData> {
    let mut entries: Vec<ServiceData> = sections
        .iter()
        .map(|(uuid, payload)| ServiceData {
            uuid: uuid.to_string(),
            payload: payload.clone(),
        })
        .collect();
    entries.sort_by(|left, right| left.uuid.cmp(&right.uuid));
    entries
}

impl RadioBoundary for BtleplugRadio {
    async fn connected_peers(
        &self,
        services: &[String],
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        #[cfg(target_os = "macos")]
        {
            if services.is_empty() {
                return Err(DesktopError::new(BleErrorCode::CapabilityUnsupported, BleErrorDomain::Capability, "peers.connected.services-required").with_detail("CoreBluetooth requires at least one service UUID for system-connected retrieval"));
            }
            let services = services
                .iter()
                .map(|value| {
                    uuid::Uuid::parse_str(value).map_err(|_| {
                        DesktopError::new(
                            BleErrorCode::ArgumentInvalid,
                            BleErrorDomain::Connection,
                            "peers.connected.services",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let peers = self
                .adapter
                .directory_peers(Some(services), None)
                .await
                .map_err(map_directory_error("peers.connected"))?;
            Ok(peers
                .into_iter()
                .map(|(id, name)| crate::boundary::DirectoryPeer {
                    peer_id: id.to_string(),
                    name,
                    connection: "connected",
                })
                .collect())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = services;
            Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "peers.connected",
            ))
        }
    }

    async fn resolve_peer(
        &self,
        peer_id: &str,
    ) -> Result<Option<crate::boundary::DirectoryPeer>, DesktopError> {
        #[cfg(target_os = "macos")]
        {
            let id = uuid::Uuid::parse_str(peer_id).map_err(|_| {
                DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Connection,
                    "peers.resolve",
                )
            })?;
            let peers = self
                .adapter
                .directory_peers(None, Some(id))
                .await
                .map_err(map_directory_error("peers.resolve"))?;
            Ok(peers
                .into_iter()
                .find(|(candidate, _)| *candidate == id)
                .map(|(id, name)| crate::boundary::DirectoryPeer {
                    peer_id: id.to_string(),
                    name,
                    connection: "unknown",
                }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = peer_id;
            Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "peers.resolve",
            ))
        }
    }

    fn canonical_peer_id(&self, peer_id: &str) -> String {
        canonical_platform_peer_id(peer_id)
    }

    async fn adapter_name(&self) -> Result<String, DesktopError> {
        Ok(self.adapter_label.clone())
    }

    fn admission_policy(&self) -> crate::boundary::AdmissionPolicy {
        host_admission_policy()
    }

    fn connection_capability_limitation(&self) -> Option<&'static str> {
        #[cfg(target_os = "linux")]
        if self.bluez_connection_policy.is_none() {
            return Some(crate::capabilities::BLUEZ_LE_AUTHORITY_REQUIRED);
        }
        None
    }

    fn tears_down_on_adapter_loss(&self) -> bool {
        true
    }

    fn os_answers_unflagged_subscribe(&self) -> bool {
        os_answers_unflagged_subscribe()
    }

    fn ingress_notification_drops(&self) -> u64 {
        self.ingress_dropped()
    }

    async fn start_scan(&self, filter: ScanFilterSpec) -> Result<(), DesktopError> {
        let mut services = Vec::with_capacity(filter.service_uuids.len());
        for uuid in &filter.service_uuids {
            services.push(uuid::Uuid::parse_str(uuid).map_err(|_| {
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::ScanFilterInvalid,
                    ubm_core::contracts::BleErrorDomain::Scan,
                    "scan.filter",
                )
            })?);
        }
        self.adapter
            .start_scan(ScanFilter {
                services,
                allow_duplicates: Some(filter.allow_duplicates()),
                name_prefix: filter.name_prefix,
            })
            .await
            .map_err(|error| DesktopError::scan_start_failed(error.to_string()).with_os(&error))?;
        // Finding 205: known devices are re-observed once by the central at
        // scan start on every platform (`reobserve_known_peers`), so no
        // per-OS report runs here.
        Ok(())
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        // A requested stop is reported by the OS too; mark it expected so
        // it never ends a later scan (Windows, vendored patch 5).
        let observed = cfg!(all(target_os = "windows", btleplug_ubm_winrt_scan_stopped));
        if observed {
            self.expected_scan_stops.fetch_add(1, Ordering::SeqCst);
        }
        let stopped = self
            .adapter
            .stop_scan()
            .await
            .map_err(|error| DesktopError::scan_stop_failed(error.to_string()).with_os(&error));
        if observed && stopped.is_err() {
            let _ = self.expected_scan_stops.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |count| count.checked_sub(1),
            );
        }
        stopped
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        let mut out = Vec::new();
        let peripherals = self.adapter.peripherals().await.map_err(map_radio(
            "peer.list",
            ubm_core::contracts::BleErrorCode::AdapterUnavailable,
            ubm_core::contracts::BleErrorDomain::Adapter,
        ))?;
        for peripheral in &peripherals {
            out.push(self.snapshot(peripheral).await?);
        }
        Ok(out)
    }

    async fn connect(&self, peer_id: &str) -> Result<(), DesktopError> {
        #[cfg(target_os = "linux")]
        let owner = self.bluez_owner("connection.connect")?;
        self.validate_peer_identity(peer_id, "connection.connect")?;
        // A new connection gets a fresh GATT state.
        self.gatt.evict(peer_id);
        #[cfg(target_os = "linux")]
        if let Ok(bluez) = self.bluez() {
            bluez.forget(peer_id);
        }
        let peripheral = self.peripheral_by_id(peer_id).await?;
        #[cfg(target_os = "linux")]
        let connected = peripheral.connect_le(owner).await;
        #[cfg(not(target_os = "linux"))]
        let connected = peripheral.connect().await;
        connected
            .map_err(|error| DesktopError::connection_failed(error.to_string()).with_os(&error))?;
        // Windows: hold the link like the legacy addon's connect did. A
        // link that cannot be maintained fails the connect; the central
        // compensates the half-open link.
        #[cfg(target_os = "windows")]
        if let Err(error) = self
            .winrt
            .maintain(peer_id, self._os_events_tx.clone())
            .await
        {
            return Err(maintained_connection_failure(error));
        }
        self.watch_write_readiness(peer_id, &peripheral);
        Ok(())
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        // T-R2: straight to the radio, as legacy went straight to
        // `peripheral.disconnect()` — no pre-disconnect `is_connected()`
        // query (an extra D-Bus read the legacy path never made).
        #[cfg(target_os = "linux")]
        let disconnected = match &self.bluez_connection_policy {
            None => Ok(()), // No acquisition can be admitted by this radio.
            Some(crate::boundary::BluezConnectionPolicy::LeBearer {
                daemon_unique_owner,
            }) => {
                self.adapter
                    .disconnect_le(peer_id, daemon_unique_owner)
                    .await
            }
        };
        #[cfg(not(target_os = "linux"))]
        let disconnected = self.peripheral_by_id(peer_id).await?.disconnect().await;
        if let Err(error) = disconnected {
            // T-R1: a removed device object is not a failure of this
            // release — it is the answer. BlueZ drops the D-Bus object, so
            // the object never comes back and every retry would fail
            // identically; the link reports released. Reported, not
            // swallowed: the error is still the only account of why the
            // radio call failed.
            if disconnect_error_confirms_released(&error) {
                eprintln!(
                    "[ubm-desktop] the peer's device object is gone, so it is released \
                     despite the disconnect erroring: {error}"
                );
            } else {
                return Err(DesktopError::new(
                    ubm_core::contracts::BleErrorCode::ConnectionLost,
                    ubm_core::contracts::BleErrorDomain::Connection,
                    "connection.disconnect",
                )
                .with_detail(error.to_string())
                .with_os(&error));
            }
        }
        self.gatt.evict(peer_id);
        self.release_link_state(peer_id)
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        self.discover_with_identity(peer_id)
            .await
            .map(|(graph, _)| graph)
    }

    async fn discover_scoped(
        &self,
        peer_id: &str,
    ) -> Result<(Vec<ServiceSnapshot>, Option<GattSnapshotIdentity>), DesktopError> {
        self.discover_with_identity(peer_id).await
    }

    fn gatt_snapshot_identity(
        &self,
        peer_id: &str,
    ) -> Result<Option<GattSnapshotIdentity>, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.gatt
                .get(peer_id)
                .map(|peripheral| {
                    peripheral
                        .accepted_le_gatt_ready_token()
                        .map(|token| {
                            token.map(|token| GattSnapshotIdentity {
                                owner: token.daemon_owner,
                                attachment: token.attachment,
                                revision: token.revision,
                            })
                        })
                        .map_err(map_radio(
                            "gatt.snapshot-identity",
                            BleErrorCode::GattDiscoveryRequired,
                            BleErrorDomain::Gatt,
                        ))
                })
                .transpose()
                .map(Option::flatten)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = peer_id;
            Ok(None)
        }
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Result<CharacteristicRead, DesktopError> {
        let peripheral = self.cached_peripheral(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.read",
            )
        })?;
        Self::read_with_provenance(&peripheral, &characteristic)
            .await
            .map_err(|error| DesktopError::read_failed(error.to_string()).with_os(&error))
    }

    #[allow(clippy::too_many_arguments)]
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
        let peripheral = self.cached_peripheral(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.write",
            )
        })?;
        let mode = if with_response {
            btleplug::api::WriteType::WithResponse
        } else {
            btleplug::api::WriteType::WithoutResponse
        };
        peripheral
            .write(&characteristic, &value, mode)
            .await
            .map_err(|error| DesktopError::write_failed(error.to_string()).with_os(&error))
    }

    #[allow(clippy::too_many_arguments)]
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
        let peripheral = self.cached_peripheral(peer_id).await?;
        let descriptor = Self::find_descriptor(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.read-descriptor",
            )
        })?;
        peripheral
            .read_descriptor(&descriptor)
            .await
            .map_err(|error| DesktopError::read_failed(error.to_string()).with_os(&error))
    }

    #[allow(clippy::too_many_arguments)]
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
        let peripheral = self.cached_peripheral(peer_id).await?;
        let descriptor = Self::find_descriptor(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattNotFound,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.write-descriptor",
            )
        })?;
        peripheral
            .write_descriptor(&descriptor, &value)
            .await
            .map_err(|error| DesktopError::write_failed(error.to_string()).with_os(&error))
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        // Linux never calls btleplug's BlueZ `mtu()`: on a discovered
        // peripheral it unwraps the first characteristic's optional MTU
        // (`bluez/peripheral.rs` `mtu`) and panics when BlueZ does not
        // report one. The BlueZ adapter reads the same property and answers
        // `None` instead.
        #[cfg(target_os = "linux")]
        {
            self.bluez().ok()?.mtu(peer_id).await.ok().flatten()
        }
        // The peripheral lookup already failed closed upstream for unknown
        // peers; here an unknown peer is simply unmeasured, and the write
        // path fails closed as `capability.unavailable`.
        #[cfg(not(target_os = "linux"))]
        {
            self.peripheral_by_id(peer_id).await.ok().map(|p| p.mtu())
        }
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
        let _gate = self.gatt.lock_peer(peer_id).await;
        let scope: InstanceKey = (
            peer_id.to_owned(),
            service_uuid.to_owned(),
            service_occurrence,
            characteristic_uuid.to_owned(),
            characteristic_occurrence,
        );
        let key = forwarder_key(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        );
        // Cleanup uses the target captured before the original native effect,
        // even after graph eviction/replacement or watcher failure.
        if !enable {
            release_notification_target(
                &self.notification_targets,
                &self.forwarders,
                &self.cleanup_debt,
                &key,
                &scope,
            )
            .await?;
            return Ok(ObservedDelivery::Unknown);
        }
        let peripheral = self.cached_peripheral(peer_id).await?;
        let characteristic = Self::find_characteristic(
            &peripheral,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        )
        .ok_or_else(|| {
            DesktopError::subscribe_failed(format!(
                "unknown characteristic {characteristic_uuid} occurrence {characteristic_occurrence}"
            ))
        })?;
        if enable {
            // Finding 39: answer the delivery mode from the characteristic's
            // properties and the platform's documented rule, before any
            // effect (see `crate::delivery`).
            let plan = plan_delivery_for_os(
                property_flags(characteristic.properties),
                requested,
                platform_rule(),
                os_answers_unflagged_subscribe(),
            )?;
            if self
                .cleanup_debt
                .lock()
                .expect("cleanup debt")
                .contains(&scope)
            {
                release_notification_target(
                    &self.notification_targets,
                    &self.forwarders,
                    &self.cleanup_debt,
                    &key,
                    &scope,
                )
                .await?;
            }
            #[cfg(target_os = "linux")]
            let cleanup_peripheral = peripheral
                .notification_cleanup_peripheral(&characteristic)
                .map_err(|error| {
                    DesktopError::subscribe_failed(error.to_string()).with_os(&error)
                })?;
            #[cfg(not(target_os = "linux"))]
            let cleanup_peripheral = peripheral.clone();
            // F09: every value routes by the exact attribute instance
            // (UBM_PATCHES.md #6), so same-UUID siblings subscribe side by
            // side and never share bytes.
            // F13 / finding 128: shared enable sequencing — the value
            // stream opens before the native enable.
            let stream: NotificationStream = match subscribe_and_stream_owned(
                &cleanup_peripheral,
                &characteristic,
                &self.notification_targets,
                &self.cleanup_debt,
                &scope,
            )
            .await
            {
                Ok(stream) => stream,
                Err(EnableStreamError::Subscribe(refusal) | EnableStreamError::Stream(refusal)) => {
                    return Err(refusal);
                }
            };
            // The btleplug stream is peripheral-wide: filter on the full
            // (service, characteristic) identity, each with its attribute
            // instance, so one subscription never routes another
            // instance's values (UBM_PATCHES.md #6).
            let route = NotificationRoute::of(&characteristic);
            // The subscription epoch is captured at install, never minted
            // at dequeue: every value this forwarder emits is attributable
            // to exactly the enablement that installed it (F10).
            let target = ForwardTarget {
                peer_id: peer_id.to_owned(),
                service_uuid: service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid: characteristic_uuid.to_owned(),
                characteristic_occurrence,
                epoch,
            };
            let forwarder = spawn_notification_forwarder(
                &self.spawn,
                stream,
                route,
                target,
                self.notifications.clone(),
                Arc::clone(&self.ingress_bytes),
                Arc::clone(&self.ingress_dropped),
            );
            // Defensive replace: a live entry under the same per-instance
            // key is aborted before overwrite so no forwarder ever leaks.
            let replaced = self.forwarders.lock().expect("forwarder table").insert(
                key.clone(),
                ForwarderEntry {
                    task: forwarder,
                    peer_id: peer_id.to_owned(),
                    service_uuid: service_uuid.to_owned(),
                    service_occurrence,
                    characteristic_uuid: characteristic_uuid.to_owned(),
                    characteristic_occurrence,
                    epoch,
                },
            );
            if let Some(stale) = replaced {
                stale.task.abort();
            }
            // A full enable supersedes any parked setup debt for this
            // scope: the new forwarder owns the native CCCD now.
            self.cleanup_debt
                .lock()
                .expect("cleanup debt")
                .remove(&scope);
            return match plan {
                DeliveryPlan::Platform(observed) => Ok(observed),
                DeliveryPlan::AdapterWrites {
                    mode,
                    platform_writes,
                } => {
                    self.rewrite_cccd(&scope, &key, mode, platform_writes, requested.is_some())
                        .await
                }
            };
        }
        Ok(ObservedDelivery::Unknown)
    }

    /// Teardown hook (M3): abort every live forwarder and best-effort
    /// release every OS-side CCCD, including parked cleanup debt (F13):
    /// an orphaned native enablement is still owed its unsubscribe.
    /// Infallible by contract: per-scope release failures are retained as
    /// receipts (F14), drained by `take_close_failures` into the shutdown
    /// report. Each scope is bounded by [`CLOSE_SCOPE_BOUND`] (PR210-25).
    /// Only a confirmed-missing peer is skipped without a receipt; a failed
    /// adapter listing, a failed discovery, a missing characteristic, a
    /// refused unsubscribe and an elapsed bound each leave a receipt.
    async fn close(&self) {
        // NOTE: the adapter event stream is deliberately NOT taken here.
        // The scan loop holds the events guard across its select until
        // loop_stop (sent after this returns), so taking it here deadlocks
        // shutdown. finish_close releases it after the loop is joined,
        // so the transport cleanup receipt includes its stream lease.
        let entries: Vec<ForwarderEntry> = self
            .forwarders
            .lock()
            .expect("forwarder table")
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in &entries {
            entry.task.abort();
        }
        self.cleanup_debt
            .lock()
            .expect("cleanup debt")
            .extend(entries.iter().map(ForwarderEntry::scope));
        self.closing_forwarders
            .lock()
            .await
            .extend(entries.into_iter().map(|entry| entry.task.handle));
        let failures = close_scope_debt(&self.cleanup_debt, |scope| async move {
            tokio::time::timeout(CLOSE_SCOPE_BOUND, self.release_scope(&scope))
                .await
                .map_err(|_| CloseScopeElapsed)
        })
        .await;
        self.gatt.clear();
        *self.close_failures.lock().expect("close failures") = failures;
    }

    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        std::mem::take(&mut self.close_failures.lock().expect("close failures"))
    }

    async fn finish_close(&self) -> Vec<DesktopError> {
        let mut failures = Vec::new();
        if tokio::time::timeout(CLOSE_SCOPE_BOUND, self.gatt_work.drain())
            .await
            .is_err()
        {
            failures.push(
                DesktopError::new(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Cleanup,
                    "radio.close.gatt-observation",
                )
                .with_detail(
                    "owned GATT observation work remains pending; retry transport cleanup",
                ),
            );
        }
        // Cancellation of the bounded waiter leaves unjoined handles here.
        // A subsequent close retries; no late stream Drop escapes accounting.
        let mut forwarders = self.closing_forwarders.lock().await;
        while let Some(task) = forwarders.last_mut() {
            let outcome = task.await;
            forwarders.pop();
            if outcome.is_err_and(|error| !error.is_cancelled()) {
                failures.push(
                    DesktopError::new(
                        BleErrorCode::PlatformFailure,
                        BleErrorDomain::Cleanup,
                        "radio.close.transport",
                    )
                    .with_detail("Notification forwarder failed during transport teardown"),
                );
            }
        }
        drop(forwarders);
        drop(self.events.lock().await.take());
        #[cfg(target_os = "linux")]
        if let Ok(bluez) = self.bluez()
            && let Err(error) = bluez.finish_discovery().await
        {
            failures.push(error);
        }
        #[cfg(target_os = "linux")]
        if let Err(error) = self.adapter.drain_match_cleanup().await {
            failures.push(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Cleanup,
                    "radio.close.transport",
                )
                .with_detail(error.to_string())
                .with_os(&error),
            );
        }
        #[cfg(target_os = "windows")]
        {
            for (_, error) in self.winrt.release_all() {
                OS_RELEASE_FAILURES.fetch_add(1, Ordering::Relaxed);
                failures.push(error);
            }
            for error in self.winrt.stop_adapter_watch() {
                OS_RELEASE_FAILURES.fetch_add(1, Ordering::Relaxed);
                failures.push(error);
            }
        }
        failures
    }

    /// Connected RSSI is a link measurement only on CoreBluetooth
    /// (`CBPeripheral.readRSSI`). btleplug 0.12's BlueZ answer is the
    /// discovery-time `Device1.RSSI` (`bluez/peripheral.rs` `read_rssi`)
    /// and its WinRT answer the last advertisement's RSSI
    /// (`winrtble/peripheral.rs` `last_rssi`): neither measures the link,
    /// so both platforms answer `capability.unsupported` (the legacy BlueZ
    /// and WinRT backends had no connected RSSI either).
    async fn read_rssi(&self, peer_id: &str) -> Result<i16, DesktopError> {
        if !cfg!(target_os = "macos") {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "peer.rssi",
            )
            .with_detail(
                "this platform reports advertisement RSSI only; the link is not measured",
            ));
        }
        let peripheral = self.peripheral_by_id(peer_id).await?;
        peripheral
            .read_rssi()
            .await
            .map_err(|error| capability_error("peer.rssi", error))
    }

    /// Effective ATT MTU of the live link, as the OS reports it (finding
    /// 217 follow-up). macOS derives
    /// `maximumWriteValueLength(.withResponse) + 3` through the vendored
    /// write-length patch — the same derivation as the Apple React Native
    /// route, so both hosts report the same value; without the patch
    /// btleplug's CoreBluetooth `mtu()` never leaves 23 and nothing
    /// measured exists. Windows reads the `GattSession.MaxPduSize`
    /// btleplug already tracks as the ATT MTU
    /// (`winrtble/ble/device.rs`). Linux reads the
    /// `org.bluez.GattCharacteristic1` MTU through the BlueZ adapter; a
    /// link BlueZ withholds it on is `capability.unavailable`, never a
    /// guessed 23.
    async fn read_effective_mtu(&self, peer_id: &str) -> Result<u16, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            let mtu = self.bluez()?.mtu(peer_id).await?;
            mtu.ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::CapabilityUnavailable,
                    BleErrorDomain::Platform,
                    "connection.effective-mtu",
                )
                .with_detail("BlueZ reported no GattCharacteristic1 MTU for this link")
            })
        }
        #[cfg(target_os = "windows")]
        {
            let peripheral = self.peripheral_by_id(peer_id).await?;
            Ok(peripheral.mtu())
        }
        #[cfg(all(target_os = "macos", btleplug_ubm_write_length))]
        {
            let peripheral = self.peripheral_by_id(peer_id).await?;
            let (with_response, _) = peripheral
                .maximum_write_value_lengths()
                .await
                .map_err(|error| capability_error("connection.effective-mtu", error))?;
            if with_response == 0 {
                return Err(DesktopError::new(
                    BleErrorCode::CapabilityUnavailable,
                    BleErrorDomain::Platform,
                    "connection.effective-mtu",
                )
                .with_detail("CoreBluetooth reported no write limit for this link"));
            }
            Ok(with_response.saturating_add(3))
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "windows",
            all(target_os = "macos", btleplug_ubm_write_length)
        )))]
        {
            let _ = peer_id;
            Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "connection.effective-mtu",
            )
            .with_detail(
                "unpatched btleplug 0.12: CoreBluetooth mtu() stays 23, so no measured ATT MTU exists; the vendored write-length patch (vendor/btleplug) provides it",
            ))
        }
    }

    /// Linux reads `Adapter1.Powered` through the BlueZ adapter: btleplug
    /// 0.12's BlueZ `adapter_state` answers `PoweredOff` when its own read
    /// fails (`bluez/adapter.rs`), which would report a failure as a fact.
    async fn adapter_state(&self) -> Result<AdapterPowerState, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.adapter_power().await
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.adapter
                .adapter_state()
                .await
                .map(power_state)
                .map_err(|error| capability_error("adapter.state", error))
        }
    }

    async fn adapter_authorization(&self) -> Result<AdapterAuthorization, DesktopError> {
        #[cfg(target_os = "macos")]
        {
            crate::os::macos::authorization()
        }
        // The Windows label is the selected adapter's native id.
        #[cfg(target_os = "windows")]
        {
            crate::os::windows::adapter_authorization(&self.adapter_label)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "adapter.authorization",
            )
            .with_detail(
                "BlueZ has no per-application authorization; D-Bus policy refuses the adapter instead",
            ))
        }
    }

    async fn write_limits(&self, peer_id: &str) -> Option<WriteLimits> {
        #[cfg(target_os = "linux")]
        {
            // BlueZ `WriteValue` performs a long write for a request larger
            // than one ATT payload (`src/gatt-client.c`
            // `characteristic_write_value` / `descriptor_write_value`), so
            // a with-response write carries a whole attribute value; a
            // command must fit one ATT payload of the negotiated MTU.
            let mtu = match self.bluez() {
                Ok(bluez) => bluez.mtu(peer_id).await.ok().flatten(),
                Err(_) => None,
            };
            Some(bluez_write_limits(mtu))
        }
        // WinRT `WriteValueAsync` with `WriteWithResponse` (characteristic)
        // and descriptor `WriteValueAsync` perform the long write
        // themselves, as the legacy WinRT addon and Tauri 4.x relied on
        // (finding 81); a command is one ATT payload of btleplug's MTU,
        // which starts at the LE default and follows `MaxPduSizeChanged`.
        #[cfg(target_os = "windows")]
        {
            self.mtu(peer_id).await.and_then(WriteLimits::os_long_write)
        }
        // macOS with the vendored patch: CoreBluetooth's own per-type answer
        // (`maximumWriteValueLengthForType:`), which also moves btleplug's
        // `mtu()` off its initial 23 (UBM_PATCHES.md #1).
        #[cfg(all(target_os = "macos", btleplug_ubm_write_length))]
        {
            let peripheral = self.peripheral_by_id(peer_id).await.ok()?;
            match peripheral.maximum_write_value_lengths().await {
                Ok((with_response, without_response))
                    if with_response > 0 && without_response > 0 =>
                {
                    Some(WriteLimits {
                        with_response,
                        without_response,
                    })
                }
                Ok(_) => None,
                Err(error) => {
                    eprintln!(
                        "ubm-desktop: CoreBluetooth maximum write length unavailable for {peer_id}: {error}"
                    );
                    None
                }
            }
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "windows",
            all(target_os = "macos", btleplug_ubm_write_length)
        )))]
        {
            self.mtu(peer_id).await.and_then(WriteLimits::from_mtu)
        }
    }

    /// CoreBluetooth's `canSendWriteWithoutResponse` (macOS, vendored
    /// patch 4). No other platform reports write readiness — btleplug
    /// hands a command to the OS stack directly — so they answer
    /// `capability.unsupported` (the legacy WinRT and BlueZ backends had
    /// no readiness either).
    async fn write_without_response_ready(&self, peer_id: &str) -> Result<bool, DesktopError> {
        #[cfg(all(target_os = "macos", btleplug_ubm_write_readiness))]
        {
            let peripheral = self.peripheral_by_id(peer_id).await?;
            peripheral
                .can_send_write_without_response()
                .await
                .map_err(|error| capability_error("gatt.write-readiness", error))
        }
        #[cfg(not(all(target_os = "macos", btleplug_ubm_write_readiness)))]
        {
            let _ = peer_id;
            Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "gatt.write-readiness",
            )
            .with_detail("this platform reports no write-without-response readiness"))
        }
    }

    fn reports_security_changes(&self) -> bool {
        cfg!(target_os = "linux")
    }

    async fn security_state(&self, peer_id: &str) -> Result<SecurityState, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.security_state(peer_id).await
        }
        #[cfg(target_os = "windows")]
        {
            self.winrt.security_state(peer_id).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = peer_id;
            Err(security_unsupported("security.state"))
        }
    }

    async fn pair(&self, peer_id: &str) -> Result<PairOutcome, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.pair(peer_id).await
        }
        #[cfg(target_os = "windows")]
        {
            self.winrt.pair(peer_id).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = peer_id;
            Err(security_unsupported("security.pair"))
        }
    }

    async fn cancel_pairing(&self, peer_id: &str) -> Result<(), DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.cancel_pairing(peer_id).await
        }
        #[cfg(target_os = "windows")]
        {
            self.winrt.cancel_pairing(peer_id)
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = peer_id;
            Err(security_unsupported("security.cancel-pairing"))
        }
    }

    async fn unpair(&self, peer_id: &str) -> Result<UnpairOutcome, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.unpair(peer_id).await
        }
        #[cfg(target_os = "windows")]
        {
            self.winrt.unpair(peer_id).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = peer_id;
            Err(security_unsupported("security.unpair"))
        }
    }

    async fn resolve_address(
        &self,
        address: &str,
        address_type: AddressType,
    ) -> Result<String, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.resolve_address(address, address_type).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (address, address_type);
            Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "peer.address-targeting",
            )
            .with_detail(if cfg!(target_os = "macos") {
                "CoreBluetooth hides peer addresses"
            } else {
                "address targeting has no adapter on this platform (the legacy backend had none)"
            }))
        }
    }

    /// btleplug reports the address type the OS gives it (BlueZ
    /// `Device1.AddressType`, the WinRT advertisement's address type).
    /// CoreBluetooth hides addresses, so macOS answers
    /// `capability.unsupported`.
    async fn address_type(&self, peer_id: &str) -> Result<Option<AddressType>, DesktopError> {
        if cfg!(target_os = "macos") {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "peer.address-type",
            )
            .with_detail("CoreBluetooth hides peer addresses"));
        }
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let properties = peripheral
            .properties()
            .await
            .map_err(|error| capability_error("peer.address-type", error))?;
        Ok(properties
            .and_then(|facts| facts.address_type)
            .map(|kind| match kind {
                btleplug::api::AddressType::Public => AddressType::Public,
                btleplug::api::AddressType::Random => AddressType::Random,
            }))
    }

    /// Characteristic facts beyond the core bits. Linux reads BlueZ
    /// `Flags` (every fact known); elsewhere btleplug's property byte gives
    /// broadcast, signed writes and extended properties, and the rest stays
    /// unknown (`None`).
    async fn characteristic_access(
        &self,
        peer_id: &str,
    ) -> Result<HashMap<InstanceKey, CharacteristicAccess>, DesktopError> {
        #[cfg(target_os = "linux")]
        {
            self.bluez()?.characteristic_access(peer_id).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let peripheral = self.cached_peripheral(peer_id).await?;
            Ok(property_byte_access(peer_id, &peripheral.services()))
        }
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        use EventStep as Step;
        loop {
            if let Some(event) = self.take_deferred().await {
                return Some(event);
            }
            // Both guards drop at the end of the block, before any
            // snapshot lookup awaits, so a slow OS lookup never wedges
            // the event source.
            let step = {
                let mut events = self.events.lock().await;
                let stream = events.as_mut()?;
                // Deliberately unbiased: a notification flood must never
                // starve adapter events (a delayed DeviceDisconnected is
                // a stale link, not a slow one).
                let mut os_events = self.os_events.lock().await;
                select_radio_event(
                    self.gatt_work.completed(),
                    self.recv_notification(),
                    os_events.recv(),
                    stream.next(),
                )
                .await
            };
            match step {
                Step::Work => {}
                Step::Notification(notified) => return notified,
                #[cfg(target_os = "linux")]
                Step::Os(Some(RadioEvent::GattInvalidationHint(peer_id))) => {
                    self.observe_gatt_hint(&peer_id);
                }
                #[cfg(target_os = "linux")]
                Step::Os(Some(RadioEvent::GattWatchFailed(detail))) => {
                    self.observe_gatt_watch_failure(detail);
                }
                // The radio holds a sender, so the OS source never closes
                // while the radio lives.
                Step::Os(Some(RadioEvent::ServicesChanged(peer_id))) => {
                    // Same invalidation as btleplug's own services-modified
                    // event: the cached database and its forwarders are dead,
                    // after the values they already buffered (finding 129).
                    self.gatt.evict(&peer_id);
                    self.drain_peer_forwarders(&peer_id, PeerRetirement::DatabaseChanged)
                        .await;
                    self.deferred
                        .lock()
                        .await
                        .push_back(RadioEvent::ServicesChanged(peer_id));
                }
                Step::Os(Some(RadioEvent::Disconnected(peer_id))) => {
                    self.observe_link_ended(peer_id).await;
                }
                Step::Os(Some(event)) => return Some(event),
                Step::Os(None) => {}
                Step::Adapter(None) => return None,
                // Findings 120 and 122: every OS sighting is an observation
                // with its own data (vendored patch 17); discovery and update
                // events only mark the peripheral known.
                Step::Adapter(Some(CentralEvent::Advertisement { id, report })) => {
                    if let Some(event) = self.sighting(&id, report).await {
                        return Some(event);
                    }
                }
                Step::Adapter(Some(CentralEvent::AdvertisementUnread { id, detail })) => {
                    note_unread_sighting(&id.to_string(), &detail);
                }
                // A changed GATT database invalidates discovered paths:
                // surface it as its own event so the central invalidates
                // generations instead of re-reading stale handles. The
                // next discovery refreshes the snapshot.
                Step::Adapter(Some(CentralEvent::DeviceServicesModified(id))) => {
                    let peer_id = id.to_string();
                    #[cfg(target_os = "linux")]
                    {
                        self.observe_gatt_hint(&peer_id);
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        self.gatt.evict(&peer_id);
                        self.drain_peer_forwarders(&peer_id, PeerRetirement::DatabaseChanged)
                            .await;
                        self.deferred
                            .lock()
                            .await
                            .push_back(RadioEvent::ServicesChanged(peer_id));
                    }
                }
                // Vendored patch 10: the adapter event broadcast outran this
                // receiver; the lost events are reported, never skipped.
                Step::Adapter(Some(CentralEvent::EventsLost { skipped })) => {
                    return Some(RadioEvent::EventsLost { skipped });
                }
                Step::Adapter(Some(CentralEvent::StateUpdate(state))) => {
                    // macOS: CoreBluetooth re-reports its state when the
                    // user answers the Bluetooth prompt; the authorization
                    // that answer set follows the state (finding 58), so a
                    // cached `not-determined` never outlives the decision.
                    #[cfg(target_os = "macos")]
                    match crate::os::macos::authorization() {
                        Ok(authorization) => {
                            if self
                                ._os_events_tx
                                .try_send(RadioEvent::AdapterAuthorization(authorization))
                                .is_err()
                            {
                                OS_EVENT_DROPS.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(error) => {
                            OS_EVENT_DROPS.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "ubm-desktop: CoreBluetooth authorization unread after a state change: {}",
                                error.detail().unwrap_or(error.code_str())
                            );
                        }
                    }
                    return Some(RadioEvent::AdapterState(power_state(state)));
                }
                Step::Adapter(Some(CentralEvent::DeviceConnected(id))) => {
                    if cfg!(target_os = "linux") {
                        continue;
                    }
                    return Some(RadioEvent::Connected(id.to_string()));
                }
                Step::Adapter(Some(CentralEvent::DeviceDisconnected(id))) => {
                    if cfg!(target_os = "linux") {
                        continue;
                    }
                    self.observe_link_ended(id.to_string()).await;
                }
                Step::Adapter(Some(_)) => {}
            }
        }
    }
}

/// The vendored btleplug patches this build links
/// (`vendor/btleplug/UBM_PATCHES.md`); empty with crates.io btleplug.
#[must_use]
pub fn vendored_btleplug_patches() -> Vec<&'static str> {
    env!("UBM_BTLEPLUG_PATCHES")
        .split(',')
        .map(str::trim)
        .filter(|patch| !patch.is_empty())
        .collect()
}

/// OS-side per-link state (Windows maintained sessions) that could not be
/// released after a link loss or at close. Counted and logged, never
/// silent.
static OS_RELEASE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// OS reports dropped because a forwarder lagged its source (readiness,
/// watcher stops). Counted, never silent.
static OS_EVENT_DROPS: AtomicU64 = AtomicU64::new(0);

/// Forward the WinRT watcher's own stops as [`RadioEvent::ScanTerminated`]
/// (Windows, vendored patch 5), skipping the stops this radio requested.
/// Elsewhere the OS ends a scan only by closing the event source, which the
/// central already settles.
fn watch_scan_stopped(
    adapter: &Adapter,
    spawn: &tokio::runtime::Handle,
    events: mpsc::Sender<RadioEvent>,
    expected: Arc<AtomicU64>,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(all(target_os = "windows", btleplug_ubm_winrt_scan_stopped))]
    {
        let mut stops = match adapter.scan_stopped_events() {
            Ok(stops) => stops,
            Err(error) => {
                OS_EVENT_DROPS.fetch_add(1, Ordering::Relaxed);
                eprintln!("ubm-desktop: WinRT watcher stops cannot be observed: {error}");
                return None;
            }
        };
        Some(spawn.spawn(async move {
            loop {
                match stops.recv().await {
                    Ok(stopped) => {
                        let requested = stopped.error == 0
                            && expected
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                                    count.checked_sub(1)
                                })
                                .is_ok();
                        if requested {
                            continue;
                        }
                        let event = RadioEvent::ScanTerminated {
                            aborted: stopped.error != 0,
                            detail: format!(
                                "the WinRT advertisement watcher stopped ({})",
                                stopped.error_name
                            ),
                        };
                        if events.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        OS_EVENT_DROPS.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }))
    }
    #[cfg(not(all(target_os = "windows", btleplug_ubm_winrt_scan_stopped)))]
    {
        let _ = (adapter, spawn, events, expected);
        None
    }
}

/// Windows: whether this process runs with package identity (the legacy
/// WinRT addon's `deployment` diagnostic, `addon.cpp` `AdapterDeployment`,
/// from `GetCurrentPackageFullName`).
///
/// `Ok(None)` off Windows, where there is no package identity to report.
pub fn host_deployment() -> Result<Option<crate::boundary::HostDeployment>, DesktopError> {
    #[cfg(target_os = "windows")]
    {
        crate::os::windows::deployment().map(Some)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(None)
    }
}

/// OS-adapter work that could not complete, since process start. Every
/// entry was also logged when it happened; none is ever dropped silently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OsAdapterFailures {
    /// Per-link OS state (Windows maintained sessions) not released after
    /// a link loss or at close.
    pub link_state_release: u64,
    /// OS change reports (Windows `GattServicesChanged`) dropped because
    /// the OS event queue was full.
    pub event_drops: u64,
    /// BlueZ watcher and discovery-session failures (a change that could
    /// not be read back, a watch that did not start, a discovery stop that
    /// failed), and Windows adapter presence watch failures (an adapter id
    /// that could not be read, a returned radio that could not be rebound,
    /// a watch that ended on its own or did not stop).
    pub watch_failures: u64,
    /// Sightings whose data the OS could not return (finding 122: an
    /// unreadable BlueZ `Device1`, an unreadable WinRT advertisement, a
    /// peripheral btleplug no longer lists). Each is also logged.
    pub advertisement_read_failures: u64,
}

/// OS-adapter failure counters since process start.
#[must_use]
pub fn os_adapter_failures() -> OsAdapterFailures {
    OsAdapterFailures {
        link_state_release: OS_RELEASE_FAILURES.load(Ordering::Relaxed),
        #[cfg(target_os = "windows")]
        event_drops: crate::os::windows::services_changed_drops()
            + OS_EVENT_DROPS.load(Ordering::Relaxed),
        #[cfg(not(target_os = "windows"))]
        event_drops: OS_EVENT_DROPS.load(Ordering::Relaxed),
        #[cfg(target_os = "linux")]
        watch_failures: crate::os::linux::security_watch_failures(),
        #[cfg(target_os = "windows")]
        watch_failures: crate::os::windows::adapter_watch_failures(),
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        watch_failures: 0,
        advertisement_read_failures: ADVERTISEMENT_READ_FAILURES.load(Ordering::Relaxed),
    }
}

/// Sightings whose data could not be read (finding 122). Counted and
/// logged, never silent.
static ADVERTISEMENT_READ_FAILURES: AtomicU64 = AtomicU64::new(0);

fn note_unread_sighting(peer_id: &str, detail: &str) {
    ADVERTISEMENT_READ_FAILURES.fetch_add(1, Ordering::Relaxed);
    eprintln!("ubm-desktop: a sighting of {peer_id} could not be read: {detail}");
}

/// One OS sighting as a scan observation (findings 120, 122): the report's
/// own data, labelled with where it came from; never the peripheral's
/// merged properties when the OS reported the advertisement itself.
pub fn observation_from_report(
    peer_id: String,
    address: Option<String>,
    report: btleplug::api::AdvertisementReport,
) -> PeerSnapshot {
    let strings = |uuids: Option<Vec<uuid::Uuid>>| {
        uuids.map(|uuids| uuids.iter().map(ToString::to_string).collect())
    };
    PeerSnapshot {
        id: peer_id,
        address,
        service_uuids: report.services.iter().map(ToString::to_string).collect(),
        rssi: report.rssi,
        local_name: report.local_name,
        manufacturer_data: sorted_manufacturer_data(&report.manufacturer_data),
        service_data: sorted_service_data(&report.service_data),
        tx_power_level: report.tx_power_level,
        extras: crate::boundary::AdvertisementExtras {
            solicited_service_uuids: strings(report.solicited_services),
            overflow_service_uuids: strings(report.overflow_services),
            connectable: report.connectable,
            appearance: None,
            raw_record: None,
            source: match report.source {
                btleplug::api::ReportSource::Advertisement => {
                    crate::boundary::ObservationSource::Advertisement
                }
                btleplug::api::ReportSource::DeviceState => {
                    crate::boundary::ObservationSource::DeviceState
                }
            },
        },
    }
}

/// `capability.unsupported` for link security on a platform whose OS
/// adapter has none (CoreBluetooth exposes no pairing API; the legacy
/// CoreBluetooth backend had none either).
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn security_unsupported(operation: &str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::CapabilityUnsupported,
        BleErrorDomain::Capability,
        operation,
    )
    .with_detail("CoreBluetooth exposes no pairing API; the OS pairs on demand")
}

/// Characteristic facts btleplug's property byte carries (broadcast,
/// signed writes, extended properties), per instance in the same
/// canonical order `discover` numbers occurrences in. The rest of
/// [`CharacteristicAccess`] is unknown on these platforms.
#[must_use]
pub fn property_byte_access(
    peer_id: &str,
    services: &BTreeSet<Service>,
) -> HashMap<InstanceKey, CharacteristicAccess> {
    let mut out = HashMap::new();
    let mut service_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
    for service in services {
        let service_occurrence = service_counts.entry(service.uuid).or_insert(0);
        let service_occ = *service_occurrence;
        *service_occurrence += 1;
        let mut char_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
        for characteristic in &service.characteristics {
            let char_occurrence = char_counts.entry(characteristic.uuid).or_insert(0);
            let char_occ = *char_occurrence;
            *char_occurrence += 1;
            let flags = characteristic.properties;
            out.insert(
                (
                    peer_id.to_owned(),
                    service.uuid.to_string(),
                    service_occ,
                    characteristic.uuid.to_string(),
                    char_occ,
                ),
                CharacteristicAccess {
                    broadcast: Some(flags.contains(CharPropFlags::BROADCAST)),
                    authenticated_signed_writes: Some(
                        flags.contains(CharPropFlags::AUTHENTICATED_SIGNED_WRITES),
                    ),
                    extended_properties: Some(flags.contains(CharPropFlags::EXTENDED_PROPERTIES)),
                    ..CharacteristicAccess::default()
                },
            );
        }
    }
    out
}

/// Core property bits for one characteristic's flags; same as
/// [`PropertyFlags::core_bits`], kept for existing callers.
#[must_use]
pub fn core_property_bits(flags: PropertyFlags) -> u8 {
    flags.core_bits()
}

#[cfg(test)]
mod tests {
    #[test]
    fn discovery_reports_missing_le_gatt_mechanism_without_losing_platform_answer() {
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        let error = super::map_radio(
            "discovery.complete",
            BleErrorCode::GattDiscoveryRequired,
            BleErrorDomain::Gatt,
        )(btleplug::Error::Platform(btleplug::PlatformError {
            domain: "bluez-dbus",
            code: "org.freedesktop.DBus.Error.UnknownMethod".into(),
            message: "GetSnapshot is absent".into(),
            metadata: vec![
                ("capability", "unsupported".into()),
                ("mechanism", "le-gatt-snapshot".into()),
            ],
        }));
        assert_eq!(error.code(), BleErrorCode::CapabilityUnsupported);
        assert_eq!(
            error.platform().unwrap().code,
            "org.freedesktop.DBus.Error.UnknownMethod"
        );
    }
    #[tokio::test]
    async fn gatt_peer_gate_serializes_one_peer_without_blocking_another() {
        use futures_util::FutureExt;
        let cache = super::GattCache::<u8>::new();
        let first = cache.lock_peer("first").await;
        let same = cache.lock_peer("first");
        tokio::pin!(same);
        assert!(same.as_mut().now_or_never().is_none());
        let other = cache
            .lock_peer("other")
            .now_or_never()
            .expect("unrelated peer must remain usable");
        drop(other);
        drop(first);
        let acquired = same.await;
        drop(acquired);
        assert_eq!(
            cache
                .peer_gates
                .lock()
                .unwrap()
                .values()
                .filter(|gate| gate.strong_count() > 0)
                .count(),
            0
        );
    }

    #[test]
    fn directory_nonplatform_failures_preserve_diagnostic_detail() {
        for detail in ["Channel closed", "Unexpected directory reply"] {
            let cause = btleplug::Error::Other(detail.into());
            let rendered = cause.to_string();
            let error = super::map_directory_error("peers.connected")(cause);
            assert_eq!(
                error.code(),
                ubm_core::contracts::BleErrorCode::PlatformFailure
            );
            assert_eq!(error.detail(), Some(rendered.as_str()));
        }
    }

    #[test]
    fn directory_queued_manager_state_preserves_specific_refusal() {
        for (state, code) in [
            (
                "manager-state-3",
                ubm_core::contracts::BleErrorCode::PermissionDenied,
            ),
            (
                "manager-state-4",
                ubm_core::contracts::BleErrorCode::AdapterPoweredOff,
            ),
        ] {
            let cause = btleplug::Error::Platform(btleplug::PlatformError::new(
                "corebluetooth",
                state,
                "state changed before lookup",
            ));
            let error = super::map_directory_error("peers.connected")(cause);
            assert_eq!(error.code(), code);
            assert_eq!(error.platform().unwrap().code, state);
        }
    }
    use std::collections::{BTreeSet, HashMap};

    use btleplug::api::{CharPropFlags, Characteristic, Descriptor, Service, ValueNotification};

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        CloseScopeElapsed, DropStream, ForwarderEntry, GattCache, NotificationRoute, ScopeRelease,
        apply_unsubscribe_outcome, close_receipt, contended_radio_drops, core_property_bits,
        drop_event_stream, find_peer, forwarder_key, select_characteristic, select_descriptor,
        select_service, service_snapshots, sorted_manufacturer_data, sorted_service_data,
    };
    use crate::boundary::PropertyFlags;
    use ubm_core::central::{
        GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE,
    };

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
    const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).expect("fixture uuid")
    }

    /// One characteristic of service instance 0x10 at ATT handle `handle`.
    fn characteristic(
        service: &str,
        char: &str,
        handle: u64,
        flags: CharPropFlags,
    ) -> Characteristic {
        Characteristic {
            uuid: uuid(char),
            instance: handle,
            service_uuid: uuid(service),
            service_instance: 0x10,
            properties: flags,
            descriptors: BTreeSet::new(),
        }
    }

    fn service_with(uuid_text: &str, handle: u64, chars: Vec<Characteristic>) -> Service {
        Service {
            uuid: uuid(uuid_text),
            instance: handle,
            primary: true,
            characteristics: chars.into_iter().collect(),
        }
    }

    /// Finding 127: a peer id names the identity the OS re-resolves it by
    /// (CoreBluetooth identifier, WinRT address); BlueZ needs none.
    #[test]
    fn f127_a_peer_id_names_its_os_identity() {
        let resolved = super::platform_peripheral_id("5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11");
        let address = super::platform_peripheral_id("AA:BB:CC:DD:EE:FF");
        if cfg!(target_vendor = "apple") {
            assert_eq!(
                resolved.map(|id| id.to_string()).as_deref(),
                Some("5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11")
            );
            assert!(address.is_none());
        } else if cfg!(target_os = "windows") {
            assert!(resolved.is_none());
            assert_eq!(
                address.map(|id| id.to_string()).as_deref(),
                Some("AA:BB:CC:DD:EE:FF")
            );
        } else {
            assert!(resolved.is_none() && address.is_none());
        }
        assert!(super::platform_peripheral_id("not an id").is_none());
    }

    /// Finding 127: the OS identity a peer id names round-trips — the
    /// string the adapter lists a peripheral under is what the resolve
    /// path (`add_peripheral`) re-resolves it by, with no scan first.
    #[test]
    fn platform_peer_identity_uses_the_native_event_spelling_without_folding_opaque_ids() {
        assert_eq!(super::canonical_platform_peer_id("opaque-A"), "opaque-A");
        #[cfg(target_vendor = "apple")]
        {
            assert_eq!(
                super::canonical_platform_peer_id("00E2CE71-3BA4-6569-E3DE-3081CE0C95FB"),
                "00e2ce71-3ba4-6569-e3de-3081ce0c95fb"
            );
            assert_eq!(
                super::canonical_platform_peer_id("00e2ce71-3ba4-6569-e3de-3081ce0c95fb"),
                "00e2ce71-3ba4-6569-e3de-3081ce0c95fb"
            );
        }
        #[cfg(target_os = "windows")]
        assert_eq!(
            super::canonical_platform_peer_id("aa:bb:cc:dd:ee:ff"),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn f127_a_listed_identity_resolves_without_a_scan() {
        #[cfg(target_vendor = "apple")]
        {
            use btleplug::platform::PeripheralId;
            let peer = "5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11";
            let id = PeripheralId::from(uuid::Uuid::parse_str(peer).expect("fixture uuid"));
            assert_eq!(id.to_string(), peer);
            assert_eq!(
                super::platform_peripheral_id(peer).map(|id| id.to_string()),
                Some(peer.to_owned())
            );
            assert!(super::platform_peripheral_id("AA:BB:CC:DD:EE:FF").is_none());
        }
        #[cfg(target_os = "windows")]
        {
            use btleplug::platform::PeripheralId;
            let peer = "AA:BB:CC:DD:EE:FF";
            let address: btleplug::api::BDAddr = peer.parse().expect("fixture address");
            let id = PeripheralId::from(address);
            assert_eq!(id.to_string(), peer);
            assert_eq!(
                super::platform_peripheral_id(peer).map(|id| id.to_string()),
                Some(peer.to_owned())
            );
            assert!(
                super::platform_peripheral_id("5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11").is_none()
            );
        }
        #[cfg(not(any(target_vendor = "apple", target_os = "windows")))]
        {
            // BlueZ resolves peers itself; no peer id names an OS identity.
            assert!(super::platform_peripheral_id("AA:BB:CC:DD:EE:FF").is_none());
            assert!(
                super::platform_peripheral_id("5e0b1c9a-6c0f-4f60-a1c1-3b5f2a0e7d11").is_none()
            );
        }
    }

    /// T-R1: a removed BlueZ device object confirms the peer is released.
    /// The D-Bus error name is the protocol constant, not rendered text.
    #[test]
    fn t_r1_a_gone_device_object_confirms_the_peer_is_released() {
        for name in [
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.bluez.Error.DoesNotExist",
        ] {
            assert!(
                super::disconnect_error_confirms_released(&btleplug::Error::Platform(
                    btleplug::PlatformError::bluez_dbus(Some(name), Some("device object is gone")),
                )),
                "{name} proves the peer is gone",
            );
        }
    }

    /// T-R1: transport failures are not release evidence; ownership stays.
    #[test]
    fn t_r1_a_transport_failure_is_not_release_evidence() {
        for name in [
            "org.freedesktop.DBus.Error.Timeout",
            "org.freedesktop.DBus.Error.NoReply",
            "org.bluez.Error.Failed",
        ] {
            assert!(
                !super::disconnect_error_confirms_released(&btleplug::Error::Platform(
                    btleplug::PlatformError::bluez_dbus(Some(name), Some("no answer")),
                )),
                "{name} does not prove the peer is gone",
            );
        }
        assert!(!super::disconnect_error_confirms_released(
            &btleplug::Error::NotConnected
        ));
        assert!(!super::disconnect_error_confirms_released(
            &btleplug::Error::DeviceNotFound
        ));
    }

    /// Findings 120 and 122: an OS sighting becomes an observation with its
    /// own data and an honest label, whatever the peripheral's merged
    /// state holds.
    #[test]
    fn f122_a_sighting_carries_its_own_data_and_label() {
        use crate::boundary::ObservationSource;
        let heart_rate = uuid(HRM_SERVICE);
        let report = btleplug::api::AdvertisementReport {
            source: btleplug::api::ReportSource::Advertisement,
            local_name: Some("Polar H10".to_owned()),
            rssi: Some(-58),
            tx_power_level: Some(4),
            manufacturer_data: [(0x006b, vec![0x00, 0x80])].into_iter().collect(),
            service_data: [(heart_rate, vec![0x01])].into_iter().collect(),
            services: vec![heart_rate],
            solicited_services: Some(vec![heart_rate]),
            overflow_services: Some(Vec::new()),
            connectable: Some(true),
        };
        let observation = super::observation_from_report(
            "peer-1".to_owned(),
            Some("AA:BB:CC:DD:EE:FF".to_owned()),
            report.clone(),
        );
        assert_eq!(observation.id, "peer-1");
        assert_eq!(observation.address.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(observation.local_name.as_deref(), Some("Polar H10"));
        assert_eq!(observation.rssi, Some(-58));
        assert_eq!(observation.tx_power_level, Some(4));
        assert_eq!(observation.service_uuids, vec![HRM_SERVICE.to_owned()]);
        assert_eq!(observation.manufacturer_data[0].company_id, 0x006b);
        assert_eq!(observation.manufacturer_data[0].payload, vec![0x00, 0x80]);
        assert_eq!(observation.service_data[0].payload, vec![0x01]);
        assert_eq!(observation.extras.connectable, Some(true));
        assert_eq!(
            observation.extras.solicited_service_uuids,
            Some(vec![HRM_SERVICE.to_owned()])
        );
        assert_eq!(observation.extras.overflow_service_uuids, Some(Vec::new()));
        assert_eq!(observation.extras.source, ObservationSource::Advertisement);
        let merged = super::observation_from_report(
            "peer-1".to_owned(),
            None,
            btleplug::api::AdvertisementReport {
                source: btleplug::api::ReportSource::DeviceState,
                ..report
            },
        );
        assert_eq!(merged.extras.source, ObservationSource::DeviceState);
        assert_eq!(ObservationSource::DeviceState.as_str(), "device-state");
    }

    /// Finding 113: the platform's answer behind a btleplug failure rides
    /// the desktop error as typed fields, for each legacy identity.
    #[test]
    fn f113_platform_answers_become_typed_error_fields() {
        use super::WithOs;
        use crate::errors::{PlatformDetail, PlatformValue};
        let cases = [
            (
                btleplug::PlatformError::new("winrt", "gatt-status", "read failed")
                    .with("gattStatus", "protocol-error"),
                PlatformDetail::new("winrt", "gatt-status")
                    .with_message("read failed")
                    .with_metadata("gattStatus", PlatformValue::Text("protocol-error".into())),
            ),
            (
                btleplug::PlatformError::new("corebluetooth", "413", "ambiguous")
                    .with("nsErrorDomain", "UBMCoreBluetooth"),
                PlatformDetail::new("corebluetooth", "413")
                    .with_message("ambiguous")
                    .with_metadata(
                        "nsErrorDomain",
                        PlatformValue::Text("UBMCoreBluetooth".into()),
                    ),
            ),
            (
                btleplug::PlatformError::bluez_dbus(
                    Some("org.bluez.Error.NotPermitted"),
                    Some("Read not permitted"),
                ),
                PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotPermitted")
                    .with_message("Read not permitted"),
            ),
        ];
        for (platform, expected) in cases {
            let cause = btleplug::Error::Platform(platform);
            let error = crate::errors::DesktopError::read_failed(cause.to_string()).with_os(&cause);
            // One vocabulary on every OS: the read keeps its own name, the
            // platform's answer (BlueZ included) riding in `platform`.
            assert_eq!(error.code_str(), "gatt.read-failed");
            assert_eq!(error.operation(), "gatt.read");
            assert_eq!(error.platform(), Some(&expected));
        }
        let local = btleplug::Error::RuntimeError("local".to_owned());
        let error = crate::errors::DesktopError::read_failed(local.to_string()).with_os(&local);
        if cfg!(target_os = "linux") {
            // On BlueZ the synthesized answer rides the error without
            // renaming it: the read keeps its own name.
            assert_eq!(error.code_str(), "gatt.read-failed");
            assert_eq!(
                error.platform(),
                Some(
                    &PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                        .with_message(local.to_string())
                )
            );
        } else {
            assert_eq!(
                error.platform(),
                None,
                "a local failure has no platform answer"
            );
        }
    }

    /// One vocabulary on every OS: a BlueZ `Failed` answer for a GATT or
    /// connect operation keeps the operation's own name, the platform's
    /// answer riding in `platform` — exactly as on macOS and Windows. Only
    /// a genuine link loss, security refusal or transient connect is
    /// renamed, by the central's classify chain, never here.
    #[test]
    fn a_bluez_failed_answer_keeps_the_operation_name_on_every_os() {
        use super::WithOs;
        use crate::errors::{DesktopError, PlatformDetail};
        let failed = || {
            btleplug::Error::Platform(btleplug::PlatformError::bluez_dbus(
                Some("org.bluez.Error.Failed"),
                Some("Failed to start notify"),
            ))
        };
        let expected = PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
            .with_message("Failed to start notify");
        let subscribe = DesktopError::subscribe_failed(failed().to_string()).with_os(&failed());
        assert_eq!(subscribe.code_str(), "gatt.subscribe-failed");
        assert_eq!(subscribe.operation(), "gatt.subscribe");
        assert_eq!(subscribe.platform(), Some(&expected));
        let read = DesktopError::read_failed(failed().to_string()).with_os(&failed());
        assert_eq!(read.code_str(), "gatt.read-failed");
        assert_eq!(read.operation(), "gatt.read");
        assert_eq!(read.platform(), Some(&expected));
        let write = DesktopError::write_failed(failed().to_string()).with_os(&failed());
        assert_eq!(write.code_str(), "gatt.write-failed");
        assert_eq!(write.operation(), "gatt.write");
        assert_eq!(write.platform(), Some(&expected));
        let connect = DesktopError::connection_failed(failed().to_string()).with_os(&failed());
        assert_eq!(connect.code_str(), "connection.failed");
        assert_eq!(connect.operation(), "connection.connect");
        assert_eq!(connect.platform(), Some(&expected));
    }

    /// The same rule for a failure with no platform answer (no D-Bus
    /// name): the operation keeps its own name on every OS. On Linux the
    /// BlueZ answer is synthesized without renaming.
    #[test]
    fn a_failure_without_a_platform_answer_keeps_the_operation_name_on_every_os() {
        use super::WithOs;
        use crate::errors::DesktopError;
        let cause = btleplug::Error::NotSupported("no cccd".to_owned());
        let error = DesktopError::subscribe_failed(cause.to_string()).with_os(&cause);
        assert_eq!(
            error.code_str(),
            "gatt.subscribe-failed",
            "no platform answer, no second opinion"
        );
        assert_eq!(error.operation(), "gatt.subscribe");
    }

    /// Owner decision (5.0): every desktop radio's link-loss answer is one
    /// the central recognizes (`classify_link_loss`), so an operation on a
    /// lost link is `connection.lost` on every host, as on Android, with the
    /// platform's answer kept (btleplug's own `NotConnected` included). A
    /// connect keeps its identity.
    #[test]
    fn a_link_operation_on_a_lost_link_is_connection_lost() {
        use super::WithOs;
        use crate::errors::PlatformDetail;
        let causes = [
            btleplug::Error::NotConnected,
            btleplug::Error::Platform(
                btleplug::PlatformError::new(
                    "corebluetooth",
                    "7",
                    "The specified device has disconnected from us.",
                )
                .with("nsErrorDomain", "CBErrorDomain"),
            ),
            btleplug::Error::Platform(
                btleplug::PlatformError::new("winrt", "gatt-status", "read failed")
                    .with("gattStatus", "unreachable"),
            ),
            btleplug::Error::Platform(btleplug::PlatformError::bluez_dbus(
                Some("org.bluez.Error.Failed"),
                Some("Not connected"),
            )),
        ];
        for cause in &causes {
            let read = crate::errors::DesktopError::read_failed(cause.to_string())
                .with_os(cause)
                .classify_link_loss();
            assert_eq!(read.code_str(), "connection.lost", "{cause:?}");
            assert_eq!(read.domain().as_str(), "connection");
            assert_eq!(read.operation(), "gatt.read");
            assert!(read.platform().is_some(), "the platform's answer is kept");
        }
        let local = crate::errors::DesktopError::read_failed("x")
            .with_os(&btleplug::Error::NotConnected)
            .classify_link_loss();
        let expected = if cfg!(target_os = "linux") {
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_message("Not connected")
        } else {
            PlatformDetail::new("btleplug", "not-connected").with_message("Not connected")
        };
        assert_eq!(local.platform(), Some(&expected));
        assert_eq!(local.code_str(), "connection.lost");
        let connect = crate::errors::DesktopError::connection_failed("x")
            .with_os(&btleplug::Error::NotConnected)
            .classify_link_loss();
        assert_ne!(
            connect.code_str(),
            "connection.lost",
            "a connect keeps its identity"
        );
    }

    /// Finding 97: an MTU BlueZ reports bounds commands; an MTU it
    /// withholds bounds nothing below the attribute ceiling (the legacy
    /// BlueZ backend let BlueZ answer); requests are long writes either way.
    #[test]
    fn f97_bluez_bounds_commands_only_by_a_reported_mtu() {
        let limits = |mtu| {
            let limits = super::bluez_write_limits(mtu);
            (limits.with_response, limits.without_response)
        };
        assert_eq!(limits(Some(185)), (512, 182));
        assert_eq!(limits(Some(23)), (512, 20));
        assert_eq!(limits(None), (512, 512));
    }

    /// Finding 96: the snapshot is in discovery (handle) order, not the
    /// UUID-first order of btleplug's sets, as the legacy backends reported
    /// it; per-UUID occurrences still follow handle order, so they agree
    /// with the occurrence-aware lookup.
    #[test]
    fn f96_the_snapshot_is_in_discovery_order_with_per_uuid_occurrences() {
        const BATTERY: &str = "0000180f-0000-1000-8000-00805f9b34fb";
        const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
        const BODY_SENSOR: &str = "00002a38-0000-1000-8000-00805f9b34fb";
        const DESC_B: &str = "00002902-0000-1000-8000-00805f9b34fb";
        const DESC_A: &str = "00002901-0000-1000-8000-00805f9b34fb";
        let descriptor = |uuid_text: &str, instance| Descriptor {
            uuid: uuid(uuid_text),
            instance,
            service_uuid: uuid(HRM_SERVICE),
            service_instance: 0x10,
            characteristic_uuid: uuid(BODY_SENSOR),
            characteristic_instance: 0x12,
        };
        let mut body_sensor = characteristic(HRM_SERVICE, BODY_SENSOR, 0x12, CharPropFlags::READ);
        body_sensor.descriptors = [descriptor(DESC_B, 0x13), descriptor(DESC_A, 0x14)]
            .into_iter()
            .collect();
        let services: BTreeSet<Service> = [
            // Discovery order: HRM (0x10) before Battery (0x30), though
            // Battery's UUID sorts first; a second HRM at 0x40.
            service_with(
                HRM_SERVICE,
                0x10,
                vec![
                    body_sensor,
                    characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x16, CharPropFlags::NOTIFY),
                    characteristic(HRM_SERVICE, BODY_SENSOR, 0x18, CharPropFlags::READ),
                ],
            ),
            service_with(
                BATTERY,
                0x30,
                vec![characteristic(
                    BATTERY,
                    BATTERY_LEVEL,
                    0x32,
                    CharPropFlags::READ,
                )],
            ),
            service_with(HRM_SERVICE, 0x40, Vec::new()),
        ]
        .into_iter()
        .collect();
        let snapshot = service_snapshots(&services);
        let order: Vec<(&str, u64)> = snapshot
            .iter()
            .map(|service| (service.uuid.as_str(), service.occurrence))
            .collect();
        assert_eq!(
            order,
            vec![(HRM_SERVICE, 0), (BATTERY, 0), (HRM_SERVICE, 1)]
        );
        let characteristics: Vec<(&str, u64)> = snapshot[0]
            .characteristics
            .iter()
            .map(|characteristic| (characteristic.uuid.as_str(), characteristic.occurrence))
            .collect();
        assert_eq!(
            characteristics,
            vec![(BODY_SENSOR, 0), (HRM_MEASUREMENT, 0), (BODY_SENSOR, 1)]
        );
        let descriptors: Vec<&str> = snapshot[0].characteristics[0]
            .descriptors
            .iter()
            .map(|descriptor| descriptor.uuid.as_str())
            .collect();
        assert_eq!(descriptors, vec![DESC_B, DESC_A]);
        assert_eq!(
            select_service(&services, HRM_SERVICE, 1).map(|service| service.instance),
            Some(0x40),
            "the lookup agrees with the reported occurrence"
        );
    }

    #[test]
    fn occurrence_selects_among_duplicate_characteristics() {
        let service = service_with(
            HRM_SERVICE,
            0x10,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x13, CharPropFlags::READ),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x15, CharPropFlags::NOTIFY),
            ],
        );
        let first = select_characteristic(&service, HRM_MEASUREMENT, 0).expect("instance 0");
        assert!(first.properties.contains(CharPropFlags::READ));
        let second = select_characteristic(&service, HRM_MEASUREMENT, 1).expect("instance 1");
        assert!(second.properties.contains(CharPropFlags::NOTIFY));
        assert!(
            select_characteristic(&service, HRM_MEASUREMENT, 2).is_none(),
            "missing instance selects nothing, never instance 0"
        );
    }

    #[test]
    fn manufacturer_sections_sort_by_company_with_verbatim_payloads() {
        let sections = HashMap::from([
            (0x02b2u16, vec![0xb2, 0x02, 0x01]),
            (0x006bu16, vec![]),
            (0xffffu16, vec![0x00]),
        ]);
        let sorted = sorted_manufacturer_data(&sections);
        let ids: Vec<u16> = sorted.iter().map(|entry| entry.company_id).collect();
        assert_eq!(ids, vec![0x006b, 0x02b2, 0xffff]);
        assert_eq!(sorted[0].payload, Vec::<u8>::new());
        assert_eq!(sorted[1].payload, vec![0xb2, 0x02, 0x01]);
        assert_eq!(sorted[2].payload, vec![0x00]);
        assert!(
            sorted_manufacturer_data(&HashMap::new()).is_empty(),
            "no sections is empty, never synthesized"
        );
    }

    #[test]
    fn service_sections_sort_by_uuid_with_verbatim_payloads() {
        let high = uuid::Uuid::parse_str("0000fef5-0000-1000-8000-00805f9b34fb").expect("uuid");
        let low = uuid::Uuid::parse_str("0000180d-0000-1000-8000-00805f9b34fb").expect("uuid");
        let sections = HashMap::from([(high, vec![0x01]), (low, vec![])]);
        let sorted = sorted_service_data(&sections);
        let uuids: Vec<&str> = sorted.iter().map(|entry| entry.uuid.as_str()).collect();
        assert_eq!(
            uuids,
            vec![
                "0000180d-0000-1000-8000-00805f9b34fb",
                "0000fef5-0000-1000-8000-00805f9b34fb",
            ]
        );
        assert_eq!(sorted[0].payload, Vec::<u8>::new());
        assert_eq!(sorted[1].payload, vec![0x01]);
        assert!(
            sorted_service_data(&HashMap::new()).is_empty(),
            "no sections is empty, never synthesized"
        );
    }

    #[test]
    fn occurrence_counts_per_uuid_not_flat_index() {
        // Flat indexing would number BATTERY_LEVEL as 2; per-UUID
        // counting (matching discover()) numbers it 0.
        let service = service_with(
            HRM_SERVICE,
            0x20,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x17, CharPropFlags::READ),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x19, CharPropFlags::NOTIFY),
                characteristic(HRM_SERVICE, BATTERY_LEVEL, 0x1b, CharPropFlags::READ),
            ],
        );
        let battery = select_characteristic(&service, BATTERY_LEVEL, 0).expect("battery");
        assert_eq!(battery.uuid, uuid(BATTERY_LEVEL));
        assert!(
            select_characteristic(&service, BATTERY_LEVEL, 1).is_none(),
            "second battery instance does not exist"
        );
    }

    #[test]
    fn occurrence_selects_among_duplicate_services() {
        let services: BTreeSet<Service> = [
            service_with(
                HRM_SERVICE,
                0x30,
                vec![characteristic(
                    HRM_SERVICE,
                    HRM_MEASUREMENT,
                    0x1d,
                    CharPropFlags::READ,
                )],
            ),
            service_with(
                HRM_SERVICE,
                0x40,
                vec![characteristic(
                    HRM_SERVICE,
                    HRM_MEASUREMENT,
                    0x1f,
                    CharPropFlags::NOTIFY,
                )],
            ),
        ]
        .into_iter()
        .collect();
        // The two services share a UUID but differ in characteristics, so
        // the cache holds both: occurrence selects the instance.
        assert_eq!(services.len(), 2, "distinct duplicates both survive");
        let first = select_service(&services, HRM_SERVICE, 0).expect("instance 0");
        let second = select_service(&services, HRM_SERVICE, 1).expect("instance 1");
        assert_ne!(
            first.characteristics, second.characteristics,
            "occurrences address different instances"
        );
        assert!(
            select_service(&services, HRM_SERVICE, 2).is_none(),
            "missing instance selects nothing, never instance 0"
        );
    }

    #[test]
    fn occurrence_selects_among_duplicate_descriptors() {
        let with_desc = |flags: CharPropFlags| {
            let mut descriptors = BTreeSet::new();
            descriptors.insert(Descriptor {
                uuid: uuid(USER_DESCRIPTION),
                instance: 0x13,
                service_uuid: uuid(HRM_SERVICE),
                service_instance: 0x10,
                characteristic_uuid: uuid(HRM_MEASUREMENT),
                characteristic_instance: 0x11,
            });
            Characteristic {
                uuid: uuid(HRM_MEASUREMENT),
                instance: 0x11,
                service_uuid: uuid(HRM_SERVICE),
                service_instance: 0x10,
                properties: flags,
                descriptors,
            }
        };
        // Descriptors differing only by UUID-collapsed identity stay one
        // entry; distinct-UUID descriptors number per UUID.
        let characteristic = with_desc(CharPropFlags::READ);
        assert!(
            select_descriptor(&characteristic, USER_DESCRIPTION, 0).is_some(),
            "descriptor instance 0"
        );
        assert!(
            select_descriptor(&characteristic, USER_DESCRIPTION, 1).is_none(),
            "no phantom descriptor instance"
        );
        assert!(
            select_descriptor(&characteristic, BATTERY_LEVEL, 0).is_none(),
            "unknown descriptor selects nothing"
        );
    }

    #[test]
    fn forwarder_keys_are_per_instance() {
        let first = forwarder_key("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let second = forwarder_key("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 1);
        assert_ne!(first, second, "duplicate UUIDs never share a forwarder");
    }

    #[test]
    fn property_bits_cover_read_write_notify_indicate() {
        let bits = core_property_bits(PropertyFlags {
            read: true,
            write: true,
            write_without_response: true,
            notify: true,
            indicate: true,
        });
        assert!(bits & GATT_PROP_READ != 0);
        assert!(bits & GATT_PROP_WRITE != 0);
        assert!(bits & GATT_PROP_NOTIFY != 0);
        assert!(bits & GATT_PROP_INDICATE != 0);
    }

    #[test]
    fn empty_properties_map_to_no_bits() {
        let bits = core_property_bits(PropertyFlags {
            read: false,
            write: false,
            write_without_response: false,
            notify: false,
            indicate: false,
        });
        assert_eq!(bits, 0);
    }

    #[test]
    fn f08_write_capabilities_map_to_independent_bits() {
        use ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE;

        let mapping = |write: bool, without_response: bool| {
            core_property_bits(PropertyFlags {
                read: false,
                write,
                write_without_response: without_response,
                notify: false,
                indicate: false,
            })
        };
        assert_eq!(mapping(false, false), 0, "neither capability sets no bit");
        assert_eq!(
            mapping(true, false),
            GATT_PROP_WRITE,
            "request-write sets only the write bit"
        );
        assert_eq!(
            mapping(false, true),
            GATT_PROP_WRITE_NO_RESPONSE,
            "command-write sets only the no-response bit, never the write bit"
        );
        assert_eq!(
            mapping(true, true),
            GATT_PROP_WRITE | GATT_PROP_WRITE_NO_RESPONSE,
            "both capabilities survive independently"
        );
    }

    fn notification(service: &str, char: &str, value: Vec<u8>) -> ValueNotification {
        ValueNotification {
            uuid: uuid(char),
            instance: 0,
            service_uuid: uuid(service),
            service_instance: 0,
            value,
            lost_before: 0,
        }
    }

    #[test]
    fn f09_same_characteristic_uuid_under_two_services_routes_only_to_owner() {
        // The same characteristic UUID appears under two services — the
        // peripheral-wide OS stream delivers both to every forwarder, so
        // each forwarder must filter on the service identity too. Bytes
        // for service A must never reach the service B subscription.
        let route_hrm = NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0);
        let route_battery =
            NotificationRoute::new(uuid(BATTERY_SERVICE), 0, uuid(HRM_MEASUREMENT), 0);
        let note_hrm = notification(HRM_SERVICE, HRM_MEASUREMENT, vec![0x01]);
        let note_battery = notification(BATTERY_SERVICE, HRM_MEASUREMENT, vec![0x02]);
        assert!(
            route_hrm.matches(&note_hrm),
            "owner route accepts its own service bytes"
        );
        assert!(
            route_battery.matches(&note_battery),
            "owner route accepts its own service bytes"
        );
        assert!(
            !route_hrm.matches(&note_battery),
            "cross-service bytes must not route to the wrong subscription"
        );
        assert!(
            !route_battery.matches(&note_hrm),
            "cross-service bytes must not route to the wrong subscription"
        );
        let other_char = notification(HRM_SERVICE, BATTERY_LEVEL, vec![0x03]);
        assert!(
            !route_hrm.matches(&other_char),
            "another characteristic under the same service still filters out"
        );
    }

    #[test]
    fn f61_same_uuid_instances_route_by_instance() {
        // Two same-UUID characteristics (handles 0x12 and 0x15) under two
        // same-UUID services (0x10, 0x20): each value reaches only the
        // instance that fired it.
        let at = |service_instance, instance| ValueNotification {
            uuid: uuid(HRM_MEASUREMENT),
            instance,
            service_uuid: uuid(HRM_SERVICE),
            service_instance,
            value: vec![0x01],
            lost_before: 0,
        };
        let route = NotificationRoute::new(uuid(HRM_SERVICE), 0x10, uuid(HRM_MEASUREMENT), 0x12);
        assert!(route.matches(&at(0x10, 0x12)));
        assert!(!route.matches(&at(0x10, 0x15)), "sibling characteristic");
        assert!(!route.matches(&at(0x20, 0x12)), "sibling service");
        let characteristic =
            characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x12, CharPropFlags::NOTIFY);
        assert_eq!(NotificationRoute::of(&characteristic), route);
    }

    #[test]
    fn f61_identical_same_uuid_instances_stay_distinct_in_handle_order() {
        // Upstream collapsed same-UUID attributes whose facts were equal;
        // with the attribute instance (UBM_PATCHES.md #6) both survive and
        // occurrence follows handle order.
        let service = service_with(
            HRM_SERVICE,
            0x10,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x15, CharPropFlags::NOTIFY),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x12, CharPropFlags::NOTIFY),
            ],
        );
        assert_eq!(service.characteristics.len(), 2);
        let first = select_characteristic(&service, HRM_MEASUREMENT, 0).expect("occurrence 0");
        let second = select_characteristic(&service, HRM_MEASUREMENT, 1).expect("occurrence 1");
        assert_eq!((first.instance, second.instance), (0x12, 0x15));
        let services: BTreeSet<Service> = [
            service_with(HRM_SERVICE, 0x20, Vec::new()),
            service_with(HRM_SERVICE, 0x10, Vec::new()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            services.len(),
            2,
            "identical same-UUID services both survive"
        );
        assert_eq!(
            select_service(&services, HRM_SERVICE, 1).map(|service| service.instance),
            Some(0x20)
        );
    }

    fn scope(
        peer: &str,
        service: &str,
        service_occurrence: u64,
        char: &str,
        char_occurrence: u64,
    ) -> crate::boundary::InstanceKey {
        (
            peer.to_owned(),
            service.to_owned(),
            service_occurrence,
            char.to_owned(),
            char_occurrence,
        )
    }

    #[derive(Clone, Default)]
    struct ExactNotificationTransport {
        subscribed: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
        unsubscribed: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
        refuse_release: std::sync::Arc<std::sync::atomic::AtomicBool>,
        refuse_stream: std::sync::Arc<std::sync::atomic::AtomicBool>,
        subscribe_refusal: std::sync::Arc<std::sync::Mutex<Option<btleplug::PlatformError>>>,
        subscribe_local_refusal: std::sync::Arc<std::sync::atomic::AtomicU8>,
        object_gone: std::sync::Arc<std::sync::atomic::AtomicBool>,
        release_no_reply: std::sync::Arc<std::sync::atomic::AtomicBool>,
        release_gate: Option<std::sync::Arc<tokio::sync::Notify>>,
        enable_gate: Option<std::sync::Arc<tokio::sync::Notify>>,
    }

    impl super::NotificationTransport for ExactNotificationTransport {
        async fn transport_subscribe(
            &self,
            characteristic: &Characteristic,
        ) -> Result<(), btleplug::Error> {
            self.subscribed
                .lock()
                .unwrap()
                .push(characteristic.instance);
            if let Some(gate) = &self.enable_gate {
                gate.notified().await;
            }
            if let Some(error) = self.subscribe_refusal.lock().unwrap().clone() {
                return Err(btleplug::Error::Platform(error));
            }
            match self
                .subscribe_local_refusal
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                1 => {
                    return Err(btleplug::Error::NotSupported(
                        "local predispatch refusal".into(),
                    ));
                }
                2 => return Err(btleplug::Error::NoSuchCharacteristic),
                3 => return Err(btleplug::Error::TimedOut(std::time::Duration::from_secs(1))),
                4 => return Err(btleplug::Error::Other("unclassified native effect".into())),
                _ => {}
            }
            Ok(())
        }
        async fn transport_notifications(
            &self,
        ) -> Result<super::NotificationStream, btleplug::Error> {
            if self.refuse_stream.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(btleplug::Error::NotSupported(
                    "fixture stream refusal".into(),
                ));
            }
            Ok(Box::pin(futures_util::stream::empty()))
        }
        async fn transport_unsubscribe(
            &self,
            characteristic: &Characteristic,
        ) -> Result<(), btleplug::Error> {
            self.unsubscribed
                .lock()
                .unwrap()
                .push(characteristic.instance);
            if self.object_gone.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(btleplug::Error::Platform(
                    btleplug::PlatformError::bluez_dbus(
                        Some("org.freedesktop.DBus.Error.UnknownObject"),
                        Some("original characteristic no longer exists"),
                    ),
                ));
            }
            if self
                .release_no_reply
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(btleplug::Error::Platform(
                    btleplug::PlatformError::bluez_dbus(
                        Some("org.freedesktop.DBus.Error.NoReply"),
                        Some("accepted release result unknown"),
                    ),
                ));
            }
            if self
                .refuse_release
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(btleplug::Error::NotSupported(
                    "fixture cleanup refusal".into(),
                ));
            }
            if let Some(gate) = &self.release_gate {
                gate.notified().await;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn rediscovery_retires_original_debt_before_reused_native_path_enable() {
        let transport = ExactNotificationTransport::default();
        let old = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let replacement = characteristic(HRM_SERVICE, BATTERY_LEVEL, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let next = scope("peer", HRM_SERVICE, 0, BATTERY_LEVEL, 0);
        let targets = super::NotificationTargets::default();
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        let _stream = super::subscribe_and_stream_owned(&transport, &old, &targets, &debt, &owned)
            .await
            .unwrap();
        let cache = GattCache::new();
        cache.insert("peer", 1);
        cache
            .refresh_retiring(
                "peer",
                || async { Ok(1) },
                || async {
                    super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
                        .await
                },
                |_| async {
                    assert_eq!(
                        *transport.unsubscribed.lock().unwrap(),
                        [0x20],
                        "old native path must settle before replacement publication/enable"
                    );
                    let _stream = super::subscribe_and_stream_owned(
                        &transport,
                        &replacement,
                        &targets,
                        &debt,
                        &next,
                    )
                    .await
                    .unwrap();
                    Ok(2)
                },
            )
            .await
            .unwrap();
        // A delayed R1 logical cleanup cannot StopNotify the R2 reuse.
        super::release_notification_target(
            &targets,
            &forwarders,
            &debt,
            &scope_key(&owned),
            &owned,
        )
        .await
        .unwrap();
        assert_eq!(*transport.unsubscribed.lock().unwrap(), [0x20]);
        assert!(targets.get(&next).is_some());
        assert!(debt.lock().unwrap().contains(&next));
        assert_eq!(cache.get("peer"), Some(2));
    }

    #[tokio::test]
    async fn rediscovery_refused_original_debt_prevents_graph_publication() {
        let transport = ExactNotificationTransport::default();
        let old = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let other = scope("other", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let targets = super::NotificationTargets::default();
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        for scope in [&owned, &other] {
            let _stream =
                super::subscribe_and_stream_owned(&transport, &old, &targets, &debt, scope)
                    .await
                    .unwrap();
        }
        transport
            .refuse_release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let published = std::sync::atomic::AtomicBool::new(false);
        let cache = GattCache::new();
        cache.insert("peer", 1);
        let result = cache
            .refresh_retiring(
                "peer",
                || async { Ok(1) },
                || async {
                    super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
                        .await
                },
                |_| async {
                    published.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(2)
                },
            )
            .await;
        assert!(result.is_err());
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(cache.get("peer"), Some(1));
        assert!(targets.get(&owned).is_some());
        assert!(targets.get(&other).is_some());
        assert_eq!(debt.lock().unwrap().len(), 2);
        transport
            .refuse_release
            .store(false, std::sync::atomic::Ordering::SeqCst);
        super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
            .await
            .unwrap();
        assert!(targets.get(&owned).is_none());
        assert!(targets.get(&other).is_some());
        assert_eq!(debt.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn original_notification_target_exact_object_retirement_settles_only_owned_scope() {
        let transport = ExactNotificationTransport::default();
        let old = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let other = scope("other", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let targets = super::NotificationTargets::default();
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        for scope in [&owned, &other] {
            let _stream =
                super::subscribe_and_stream_owned(&transport, &old, &targets, &debt, scope)
                    .await
                    .unwrap();
        }
        transport
            .object_gone
            .store(true, std::sync::atomic::Ordering::SeqCst);
        super::release_notification_target(
            &targets,
            &forwarders,
            &debt,
            &scope_key(&owned),
            &owned,
        )
        .await
        .unwrap();
        assert!(targets.get(&owned).is_none());
        assert!(!debt.lock().unwrap().contains(&owned));
        assert!(targets.get(&other).is_some());
        assert!(debt.lock().unwrap().contains(&other));
        assert_eq!(*transport.unsubscribed.lock().unwrap(), [0x20]);
    }

    #[tokio::test]
    async fn original_notification_target_survives_graph_replacement() {
        let transport = ExactNotificationTransport::default();
        let old = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let new = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x30, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let key = scope_key(&owned);
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        let debt = std::sync::Mutex::new(std::collections::HashSet::from([owned.clone()]));
        let targets = super::NotificationTargets::default();
        let _stream = super::subscribe_and_stream_owned(&transport, &old, &targets, &debt, &owned)
            .await
            .unwrap();
        assert_ne!(old.instance, new.instance);
        super::release_notification_target(&targets, &forwarders, &debt, &key, &owned)
            .await
            .unwrap();
        assert_eq!(
            *transport.unsubscribed.lock().unwrap(),
            vec![0x20],
            "cleanup must address the original native attribute, never R2"
        );
        assert!(debt.lock().unwrap().is_empty());
        assert!(targets.get(&owned).is_none());
        super::release_notification_target(&targets, &forwarders, &debt, &key, &owned)
            .await
            .unwrap();
        assert_eq!(
            *transport.unsubscribed.lock().unwrap(),
            vec![0x20],
            "already-confirmed cleanup cannot stop an unrelated scope"
        );
    }

    #[tokio::test]
    async fn rediscovery_retires_forwarders_before_new_graph_publication() {
        let cache = super::GattCache::new();
        cache.insert("peer", 1_u64);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let other = scope("other", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let task = super::spawn_notification_forwarder(
            &tokio::runtime::Handle::current(),
            futures_util::stream::iter(vec![notification(
                HRM_SERVICE,
                HRM_MEASUREMENT,
                vec![0x51],
            )]),
            super::NotificationRoute::new(uuid(HRM_SERVICE), 0, uuid(HRM_MEASUREMENT), 0),
            super::ForwardTarget {
                peer_id: "peer".into(),
                service_uuid: HRM_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HRM_MEASUREMENT.into(),
                characteristic_occurrence: 0,
                epoch: 1,
            },
            tx,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        let mut entry = forwarder(tokio::spawn(async {}), &owned);
        entry.task.abort();
        entry.task = task;
        let forwarders =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::from([
                (scope_key(&owned), entry),
                (scope_key(&other), forwarder(tokio::spawn(async {}), &other)),
            ])));
        let debt = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let graph = forwarders.clone();
        let retire = forwarders.clone();
        let retired_debt = debt.clone();
        cache
            .refresh_retiring(
                "peer",
                || async { Ok(1) },
                move || async move {
                    let entries = super::retire_peer_forwarders(
                        &mut retire.lock().unwrap(),
                        &mut retired_debt.lock().unwrap(),
                        "peer",
                        super::PeerRetirement::DatabaseChanged,
                    );
                    for entry in entries {
                        assert_eq!(entry.task.drain(super::FORWARDER_DRAIN_BOUND).await, 0);
                    }
                    Ok(())
                },
                move |_| async move {
                    assert!(
                        !graph
                            .lock()
                            .unwrap()
                            .values()
                            .any(|entry| entry.peer_id == "peer"),
                        "R1 forwarder must retire before reading/publishing R2"
                    );
                    Ok(2)
                },
            )
            .await
            .unwrap();
        assert_eq!(cache.get("peer"), Some(2));
        assert!(debt.lock().unwrap().contains(&owned));
        assert!(forwarders.lock().unwrap().contains_key(&scope_key(&other)));
        assert!(
            matches!(rx.recv().await, Some(super::RadioEvent::Notification { value, .. }) if value == vec![0x51]),
            "R1 buffered FIFO must survive retirement"
        );
    }

    #[tokio::test]
    async fn held_enable_hint_keeps_unrelated_notification_and_link_event_live() {
        use super::RadioEvent;
        use std::{future::Future, task::Poll};
        let cache = std::sync::Arc::new(GattCache::<usize>::new());
        let enable_gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let transport = ExactNotificationTransport {
            enable_gate: Some(enable_gate.clone()),
            ..Default::default()
        };
        let targets = super::NotificationTargets::default();
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let owned = scope("held", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let characteristic =
            characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let enabling = async {
            let _gate = cache.lock_peer("held").await;
            super::subscribe_and_stream_owned(&transport, &characteristic, &targets, &debt, &owned)
                .await
        };
        tokio::pin!(enabling);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(enabling.as_mut().poll(cx).is_pending())).await
        );
        assert_eq!(*transport.subscribed.lock().unwrap(), [0x20]);
        let queue = super::GattWorkQueue::default();
        let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = retired.clone();
        let work_cache = cache.clone();
        queue.enqueue("held".into(), async move {
            let _gate = work_cache.lock_peer("held").await;
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel(1);
        let (os_tx, mut os_rx) = tokio::sync::mpsc::channel(1);
        notify_tx
            .send(RadioEvent::Notification {
                peer_id: "other-peer".into(),
                service_uuid: HRM_SERVICE.into(),
                service_occurrence: 0,
                characteristic_uuid: HRM_MEASUREMENT.into(),
                characteristic_occurrence: 0,
                epoch: 1,
                value: vec![0, 81],
            })
            .await
            .unwrap();
        os_tx
            .send(RadioEvent::Disconnected("other-peer".into()))
            .await
            .unwrap();
        let mut saw_value = false;
        let mut saw_link = false;
        for _ in 0..2 {
            let event = super::select_radio_event(
                queue.completed(),
                notify_rx.recv(),
                os_rx.recv(),
                std::future::pending(),
            );
            tokio::pin!(event);
            let step = std::future::poll_fn(|cx| Poll::Ready(event.as_mut().poll(cx))).await;
            assert!(
                matches!(
                    step,
                    Poll::Ready(super::EventStep::Notification(Some(_)))
                        | Poll::Ready(super::EventStep::Os(Some(_)))
                ),
                "held peer invalidation must not stall independent event sources"
            );
            match step {
                Poll::Ready(super::EventStep::Notification(Some(RadioEvent::Notification {
                    value,
                    peer_id,
                    ..
                }))) => {
                    assert_eq!(value, [0, 81]);
                    assert_eq!(peer_id, "other-peer");
                    saw_value = true;
                }
                Poll::Ready(super::EventStep::Os(Some(RadioEvent::Disconnected(peer)))) => {
                    assert_eq!(peer, "other-peer");
                    saw_link = true;
                }
                _ => panic!("unexpected independent event"),
            }
        }
        assert!(saw_value && saw_link);
        assert!(!retired.load(std::sync::atomic::Ordering::SeqCst));
        enable_gate.notify_one();
        let _stream = enabling.await.unwrap();
        queue.drain().await;
        assert!(retired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[derive(Clone)]
    struct ObservedGatt {
        accepted: Result<Option<super::GattSnapshotIdentity>, super::DesktopError>,
        current: Result<super::GattSnapshotIdentity, super::DesktopError>,
        calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
        held: bool,
    }

    impl super::GattObservationSource for ObservedGatt {
        fn accepted_identity(
            &self,
        ) -> Result<Option<super::GattSnapshotIdentity>, super::DesktopError> {
            self.accepted.clone()
        }
        async fn current_identity(
            &self,
        ) -> Result<super::GattSnapshotIdentity, super::DesktopError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.held {
                std::future::pending::<()>().await;
            }
            self.current.clone()
        }
    }

    fn observation_fixture(
        source: ObservedGatt,
        identity: super::GattSnapshotIdentity,
    ) -> super::GattObservationState<ObservedGatt> {
        let gatt = std::sync::Arc::new(GattCache::new());
        gatt.insert("affected", source.clone());
        gatt.remember_identity("affected", identity.clone());
        gatt.insert(
            "unrelated",
            ObservedGatt {
                accepted: Ok(Some(identity.clone())),
                current: Ok(identity.clone()),
                calls: Default::default(),
                held: false,
            },
        );
        gatt.remember_identity("unrelated", identity);
        super::GattObservationState {
            gatt,
            forwarders: Default::default(),
            debt: Default::default(),
            deferred: Default::default(),
            failure: Default::default(),
            peer_failures: Default::default(),
            observation_bound: std::time::Duration::from_millis(10),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gatt_observation_failures_are_peer_scoped_typed_and_reverifiable() {
        use super::{BleErrorCode, BleErrorDomain, DesktopError, RadioEvent};
        let identity = super::GattSnapshotIdentity {
            owner: ":1.2".into(),
            attachment: 1,
            revision: 1,
        };
        let malformed = DesktopError::new(
            BleErrorCode::ProtocolMalformed,
            BleErrorDomain::Gatt,
            "gatt.observe",
        )
        .with_detail("actual malformed snapshot reply");
        for (accepted, current, held, expected, expected_calls) in [
            (
                Ok(Some(identity.clone())),
                Err(malformed.clone()),
                false,
                BleErrorCode::ProtocolMalformed,
                1,
            ),
            (
                Err(malformed.clone()),
                Ok(identity.clone()),
                false,
                BleErrorCode::ProtocolMalformed,
                0,
            ),
            (
                Ok(None),
                Ok(identity.clone()),
                false,
                BleErrorCode::ProtocolMalformed,
                0,
            ),
            (
                Ok(Some(identity.clone())),
                Ok(identity.clone()),
                true,
                BleErrorCode::OperationTimedOut,
                1,
            ),
        ] {
            let source = ObservedGatt {
                accepted,
                current,
                calls: Default::default(),
                held,
            };
            let state = observation_fixture(source.clone(), identity.clone());
            state.observe_hint("affected").await;
            assert!(
                state.failure.lock().unwrap().is_none(),
                "peer read refusal must not fence host"
            );
            assert!(state.gatt.get("affected").is_none());
            assert!(
                super::cached_gatt_peer(&state.gatt, &state.peer_failures, "unrelated").is_ok()
            );
            let error = super::cached_gatt_peer(&state.gatt, &state.peer_failures, "affected")
                .err()
                .unwrap();
            assert_eq!(error.code(), expected);
            assert_eq!(
                source.calls.load(std::sync::atomic::Ordering::SeqCst),
                expected_calls
            );
            let event = state.deferred.lock().await.pop_front().unwrap();
            assert!(
                matches!(event, RadioEvent::GattObservationFailed { peer_id, identity: captured, error: cause }
                if peer_id == "affected" && captured == identity && cause == error),
                "read/getter failure must retain its own cause, never manufacture ServicesChanged"
            );
            let good = ObservedGatt {
                accepted: Ok(Some(identity.clone())),
                current: Ok(identity.clone()),
                calls: Default::default(),
                held: false,
            };
            state.gatt.insert("affected", good.clone());
            state.confirm_reverification("affected", identity.clone());
            state.observe_hint("affected").await;
            assert!(super::cached_gatt_peer(&state.gatt, &state.peer_failures, "affected").is_ok());
            assert_eq!(good.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(state.deferred.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn gatt_observation_timeout_is_not_a_physical_change() {
        let identity = super::GattSnapshotIdentity {
            owner: ":1.2".into(),
            attachment: 1,
            revision: 1,
        };
        let source = ObservedGatt {
            accepted: Ok(Some(identity.clone())),
            current: Ok(identity.clone()),
            calls: Default::default(),
            held: true,
        };
        let state = observation_fixture(source, identity.clone());
        state.observe_hint("affected").await;
        assert!(matches!(state.deferred.lock().await.pop_front(),
            Some(super::RadioEvent::GattObservationFailed { error, identity: captured, .. })
            if error.code() == super::BleErrorCode::OperationTimedOut && captured == identity));
        assert!(state.failure.lock().unwrap().is_none());
        assert!(state.gatt.get("unrelated").is_some());
    }

    #[tokio::test]
    async fn gatt_observation_getter_refusal_is_not_silent() {
        let identity = super::GattSnapshotIdentity {
            owner: ":1.2".into(),
            attachment: 1,
            revision: 1,
        };
        let error = super::DesktopError::new(
            super::BleErrorCode::ProtocolMalformed,
            super::BleErrorDomain::Gatt,
            "gatt.snapshot-identity",
        )
        .with_detail("actual accepted identity getter refusal");
        let state = observation_fixture(
            ObservedGatt {
                accepted: Err(error.clone()),
                current: Ok(identity.clone()),
                calls: Default::default(),
                held: false,
            },
            identity.clone(),
        );
        state.observe_hint("affected").await;
        assert!(matches!(state.deferred.lock().await.pop_front(),
            Some(super::RadioEvent::GattObservationFailed { identity: captured, error: cause, .. })
            if captured == identity && cause == error));
        assert!(state.gatt.get("affected").is_none());
        assert!(state.gatt.get("unrelated").is_some());
    }

    #[tokio::test]
    async fn gatt_observation_host_watch_failure_retains_first_cause_and_retires_all() {
        let identity = super::GattSnapshotIdentity {
            owner: ":1.2".into(),
            attachment: 1,
            revision: 1,
        };
        let source = ObservedGatt {
            accepted: Ok(Some(identity.clone())),
            current: Ok(identity.clone()),
            calls: Default::default(),
            held: false,
        };
        let state = observation_fixture(source, identity);
        let first = super::DesktopError::new(
            super::BleErrorCode::PlatformFailure,
            super::BleErrorDomain::Platform,
            "gatt.watch",
        )
        .with_detail("actual watch transport closed");
        state.fail(first.clone()).await;
        let second = super::DesktopError::new(
            super::BleErrorCode::ProtocolMalformed,
            super::BleErrorDomain::Platform,
            "gatt.watch",
        )
        .with_detail("later watch diagnostic");
        state.fail(second.clone()).await;
        assert_eq!(*state.failure.lock().unwrap(), Some(first.clone()));
        assert!(state.gatt.get("affected").is_none() && state.gatt.get("unrelated").is_none());
        let mut events = state.deferred.lock().await;
        assert_eq!(
            events.pop_front(),
            Some(super::RadioEvent::GattWatchFailed(first))
        );
        assert_eq!(
            events.pop_front(),
            Some(super::RadioEvent::GattWatchFailed(second))
        );
    }

    #[tokio::test]
    async fn gatt_observation_new_ready_identity_is_actual_scoped_change() {
        let old = super::GattSnapshotIdentity {
            owner: ":1.2".into(),
            attachment: 1,
            revision: 1,
        };
        let next = super::GattSnapshotIdentity {
            revision: 2,
            ..old.clone()
        };
        let source = ObservedGatt {
            accepted: Ok(Some(old.clone())),
            current: Ok(next),
            calls: Default::default(),
            held: false,
        };
        let state = observation_fixture(source, old.clone());
        state.observe_hint("affected").await;
        assert!(matches!(state.deferred.lock().await.pop_front(),
            Some(super::RadioEvent::ServicesChangedScoped { peer_id, identity }) if peer_id == "affected" && identity == old));
        assert!(state.peer_failures.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn gatt_work_cancellation_coalescing_and_close_are_owned_and_bounded() {
        use std::{future::Future, task::Poll};
        let queue = super::GattWorkQueue::default();
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let called = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let first_gate = gate.clone();
        let first_count = called.clone();
        queue.enqueue("peer".into(), async move {
            first_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            first_gate.notified().await;
        });
        {
            let work = queue.completed();
            tokio::pin!(work);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(work.as_mut().poll(cx).is_pending())).await
            );
        }
        assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 1);
        let reread = called.clone();
        queue.enqueue("peer".into(), async move {
            reread.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(
            tokio::time::timeout(super::CLOSE_SCOPE_BOUND, queue.drain())
                .await
                .is_err()
        );
        assert_eq!(
            queue.entries.lock().unwrap().len(),
            1,
            "bounded close must retain unfinished work"
        );
        gate.notify_one();
        queue.drain().await;
        assert_eq!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            11,
            "cancel/retry must retain initial call and fresh rerun"
        );
        assert!(queue.entries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn original_notification_target_indeterminate_release_fences_retry_and_new_graph() {
        let transport = ExactNotificationTransport::default();
        let old = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let targets = super::NotificationTargets::default();
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        let _stream = super::subscribe_and_stream_owned(&transport, &old, &targets, &debt, &owned)
            .await
            .unwrap();
        transport
            .release_no_reply
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = super::release_notification_target(
            &targets,
            &forwarders,
            &debt,
            &scope_key(&owned),
            &owned,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.platform().unwrap().code,
            "org.freedesktop.DBus.Error.NoReply"
        );
        transport
            .release_no_reply
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let cache = GattCache::new();
        let candidate = std::sync::atomic::AtomicBool::new(false);
        assert!(
            cache
                .refresh_retiring(
                    "peer",
                    || async { Ok(1) },
                    || async {
                        super::retire_notification_debt_for_peer(
                            &targets,
                            &forwarders,
                            &debt,
                            "peer",
                        )
                        .await
                    },
                    |_| async {
                        candidate.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(2)
                    }
                )
                .await
                .is_err()
        );
        assert!(!candidate.load(std::sync::atomic::Ordering::SeqCst));
        assert!(cache.get("peer").is_none());
        assert!(targets.get(&owned).is_some() && debt.lock().unwrap().contains(&owned));
        assert_eq!(*transport.unsubscribed.lock().unwrap(), [0x20]);
    }

    #[tokio::test]
    async fn original_notification_target_retains_refused_cancelled_and_unrelated_cleanup() {
        use std::{future::Future, task::Poll};
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let transport = ExactNotificationTransport {
            release_gate: Some(gate.clone()),
            ..Default::default()
        };
        let original = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let other = scope("other", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let key = scope_key(&owned);
        let targets = super::NotificationTargets::default();
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let _owned =
            super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &owned)
                .await
                .unwrap();
        let _other =
            super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &other)
                .await
                .unwrap();
        transport
            .refuse_release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            super::release_notification_target(&targets, &forwarders, &debt, &key, &owned)
                .await
                .is_err()
        );
        assert!(targets.get(&owned).is_some() && debt.lock().unwrap().contains(&owned));
        transport
            .refuse_release
            .store(false, std::sync::atomic::Ordering::SeqCst);
        {
            let release =
                super::release_notification_target(&targets, &forwarders, &debt, &key, &owned);
            tokio::pin!(release);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(release.as_mut().poll(cx).is_pending()))
                    .await
            );
        }
        assert!(targets.get(&owned).is_some() && debt.lock().unwrap().contains(&owned));
        gate.notify_one();
        super::release_notification_target(&targets, &forwarders, &debt, &key, &owned)
            .await
            .unwrap();
        assert!(targets.get(&owned).is_none());
        assert!(targets.get(&other).is_some() && debt.lock().unwrap().contains(&other));
        assert_eq!(*transport.unsubscribed.lock().unwrap(), vec![0x20, 0x20]);
    }

    #[tokio::test]
    async fn original_notification_target_enable_answer_matrix_preserves_uncertain_ownership() {
        let platform_answers = [
            ("bluez-dbus", "org.bluez.Error.NotSupported", true),
            ("bluez-dbus", "org.bluez.Error.NotPermitted", true),
            ("bluez-dbus", "org.bluez.Error.NotConnected", true),
            (
                "bluez-dbus",
                "org.freedesktop.DBus.Error.UnknownObject",
                true,
            ),
            (
                "bluez-dbus",
                "org.freedesktop.DBus.Error.UnknownInterface",
                true,
            ),
            (
                "bluez-dbus",
                "org.freedesktop.DBus.Error.UnknownMethod",
                true,
            ),
            (
                "bluez-dbus",
                "org.freedesktop.DBus.Error.AccessDenied",
                true,
            ),
            ("bluez-dbus", "org.bluez.Error.Failed", false),
            ("bluez-dbus", "org.bluez.Error.InProgress", false),
            ("bluez-dbus", "org.freedesktop.DBus.Error.NoReply", false),
            ("bluez-dbus", "org.freedesktop.DBus.Error.Timeout", false),
            ("bluez-dbus", "org.freedesktop.DBus.Error.TimedOut", false),
            ("other-native", "org.bluez.Error.NotSupported", false),
        ];
        for (platform, local, no_effect) in platform_answers
            .into_iter()
            .map(|(domain, code, no_effect)| {
                let mut error =
                    btleplug::PlatformError::bluez_dbus(Some(code), Some("fixture native answer"));
                error.domain = domain;
                (Some(error), 0, no_effect)
            })
            .chain([
                (None, 1, true),
                (None, 2, true),
                (None, 3, false),
                (None, 4, false),
            ])
        {
            let transport = ExactNotificationTransport::default();
            *transport.subscribe_refusal.lock().unwrap() = platform;
            transport
                .subscribe_local_refusal
                .store(local, std::sync::atomic::Ordering::SeqCst);
            transport
                .refuse_release
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let original =
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
            let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
            let targets = super::NotificationTargets::default();
            let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
            let debt = std::sync::Mutex::new(std::collections::HashSet::new());
            assert!(
                super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &owned)
                    .await
                    .is_err()
            );
            assert_eq!(targets.get(&owned).is_none(), no_effect);
            assert_eq!(!debt.lock().unwrap().contains(&owned), no_effect);
            let retirement =
                super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
                    .await;
            assert_eq!(retirement.is_ok(), no_effect);
            assert_eq!(
                transport.unsubscribed.lock().unwrap().len(),
                usize::from(!no_effect)
            );
            *transport.subscribe_refusal.lock().unwrap() = None;
            transport
                .subscribe_local_refusal
                .store(0, std::sync::atomic::Ordering::SeqCst);
            if !no_effect {
                let old_target = targets.get(&owned).unwrap();
                assert!(
                    super::subscribe_and_stream_owned(
                        &transport, &original, &targets, &debt, &owned
                    )
                    .await
                    .is_err()
                );
                assert!(std::sync::Arc::ptr_eq(
                    &targets.get(&owned).unwrap(),
                    &old_target
                ));
                assert_eq!(
                    transport.subscribed.lock().unwrap().len(),
                    1,
                    "unsettled ownership fences new native enable"
                );
                transport
                    .refuse_release
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
                    .await
                    .unwrap();
                assert_eq!(*transport.unsubscribed.lock().unwrap(), vec![0x20, 0x20]);
            }
            drop(
                super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &owned)
                    .await
                    .unwrap(),
            );
            transport
                .refuse_release
                .store(false, std::sync::atomic::Ordering::SeqCst);
            super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
                .await
                .unwrap();
            assert!(targets.get(&owned).is_none() && debt.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn original_notification_target_definite_enable_refusal_allows_retry_and_graph_retirement()
     {
        let transport = ExactNotificationTransport::default();
        *transport.subscribe_refusal.lock().unwrap() = Some(btleplug::PlatformError::bluez_dbus(
            Some("org.bluez.Error.NotSupported"),
            Some("no notify property"),
        ));
        transport
            .refuse_release
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let original = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let unrelated = scope("other", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let targets = super::NotificationTargets::default();
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        let other = ExactNotificationTransport::default();
        drop(
            super::subscribe_and_stream_owned(&other, &original, &targets, &debt, &unrelated)
                .await
                .unwrap(),
        );
        let error =
            super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &owned)
                .await
                .err()
                .unwrap();
        assert!(
            matches!(error, super::EnableStreamError::Subscribe(ref cause)
            if cause.platform().is_some_and(|p| p.code == "org.bluez.Error.NotSupported"))
        );
        assert!(
            targets.get(&owned).is_none(),
            "pre-effect refusal must not invent a native cleanup target"
        );
        assert!(!debt.lock().unwrap().contains(&owned));
        super::release_notification_target(
            &targets,
            &forwarders,
            &debt,
            &scope_key(&owned),
            &owned,
        )
        .await
        .unwrap();
        assert!(
            transport.unsubscribed.lock().unwrap().is_empty(),
            "no unowned StopNotify"
        );
        *transport.subscribe_refusal.lock().unwrap() = None;
        transport
            .refuse_release
            .store(false, std::sync::atomic::Ordering::SeqCst);
        drop(
            super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &owned)
                .await
                .unwrap(),
        );
        super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "peer")
            .await
            .unwrap();
        assert_eq!(*transport.subscribed.lock().unwrap(), vec![0x20, 0x20]);
        assert_eq!(*transport.unsubscribed.lock().unwrap(), vec![0x20]);
        assert!(targets.get(&unrelated).is_some() && debt.lock().unwrap().contains(&unrelated));
        super::retire_notification_debt_for_peer(&targets, &forwarders, &debt, "other")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn original_notification_target_stream_refusal_admits_no_native_cleanup() {
        let transport = ExactNotificationTransport::default();
        transport
            .refuse_stream
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let original = characteristic(HRM_SERVICE, HRM_MEASUREMENT, 0x20, CharPropFlags::NOTIFY);
        let owned = scope("peer", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let targets = super::NotificationTargets::default();
        let forwarders = std::sync::Mutex::new(std::collections::HashMap::new());
        let debt = std::sync::Mutex::new(std::collections::HashSet::new());
        assert!(
            super::subscribe_and_stream_owned(&transport, &original, &targets, &debt, &owned)
                .await
                .is_err()
        );
        super::release_notification_target(
            &targets,
            &forwarders,
            &debt,
            &scope_key(&owned),
            &owned,
        )
        .await
        .unwrap();
        assert!(transport.subscribed.lock().unwrap().is_empty());
        assert!(transport.unsubscribed.lock().unwrap().is_empty());
        assert!(targets.get(&owned).is_none() && debt.lock().unwrap().is_empty());
    }

    fn forwarder(
        task: tokio::task::JoinHandle<()>,
        scope: &crate::boundary::InstanceKey,
    ) -> ForwarderEntry {
        ForwarderEntry {
            task: task.into(),
            peer_id: scope.0.clone(),
            service_uuid: scope.1.clone(),
            service_occurrence: scope.2,
            characteristic_uuid: scope.3.clone(),
            characteristic_occurrence: scope.4,
            epoch: 0,
        }
    }

    fn scope_key(scope: &crate::boundary::InstanceKey) -> String {
        forwarder_key(&scope.0, &scope.1, scope.2, &scope.3, scope.4)
    }

    #[test]
    fn f43_adapter_selection_refuses_ambiguity_and_never_skips_a_withheld_identity() {
        use super::choose_adapter;

        let one = [Ok("hci0 (usb:v1D6Bp0246d0540)".to_owned())];
        assert_eq!(choose_adapter(&one, None).expect("single adapter"), 0);
        let two = [
            Ok("hci0 (usb:v1D6Bp0246d0540)".to_owned()),
            Ok("hci1 (usb:v0A12p0001d8891)".to_owned()),
        ];
        let ambiguous = choose_adapter(&two, None).expect_err("two adapters, no name");
        assert_eq!(ambiguous.code_str(), "adapter.ambiguous");
        if cfg!(target_os = "linux") {
            assert_eq!(choose_adapter(&two, Some("hci1")).expect("by id"), 1);
        }
        assert_eq!(
            choose_adapter(&two, Some("hci1 (usb:v0A12p0001d8891)")).expect("by full label"),
            1
        );
        // W-R2: a name the listing never held is `adapter.unavailable`,
        // as every legacy desktop provider reported it.
        let missing = choose_adapter(&two, Some("hci9")).expect_err("no such adapter");
        assert_eq!(missing.code_str(), "adapter.unavailable");
        let withheld = [
            Ok("hci0 (usb:v1D6Bp0246d0540)".to_owned()),
            Err(crate::errors::DesktopError::adapter_unavailable(
                "adapter.info",
            )),
        ];
        assert_eq!(
            choose_adapter(&withheld, Some("hci0 (usb:v1D6Bp0246d0540)"))
                .expect_err("a withheld identity fails the selection")
                .code_str(),
            "adapter.unavailable"
        );
        assert_eq!(
            choose_adapter(&[], None)
                .expect_err("no adapter")
                .code_str(),
            "adapter.unavailable"
        );
    }

    /// This workspace links the vendored btleplug (root `[patch.crates-io]`),
    /// so the patched CoreBluetooth write-length API must be detected; a
    /// silently unpatched build would report a 20-byte write limit on macOS.
    #[test]
    fn the_vendored_btleplug_patch_set_is_detected() {
        assert!(
            super::vendored_btleplug_patches()
                == [
                    "corebluetooth-write-length",
                    "corebluetooth-advertisement-extras",
                    "bluez-session-bus",
                    "corebluetooth-write-readiness",
                    "winrt-scan-stopped",
                    "attribute-instances",
                    "central-state-detail",
                    "scan-policy",
                    "winrt-uncached-discovery",
                    "winrt-cccd-mode",
                    "winrt-adapter-by-id",
                    "stream-lag-reported",
                    "winrt-passive-scan",
                    "bluez-name-pattern",
                    "event-capacity",
                    "corebluetooth-read-notify",
                    "platform-errors",
                    "winrt-service-filter",
                    "advertisement-reports",
                    "bluez-device-changes",
                    "disconnect-lifecycle",
                    "winrt-att-error",
                    "bluez-optional-modalias",
                    "bluez-match-cleanup",
                ],
            "DEP_BTLEPLUG_UBM_PATCHES missing: the vendored btleplug is not linked"
        );
    }

    #[test]
    fn find_peer_splits_listing_failure_from_a_real_miss() {
        let listing: Result<Vec<String>, btleplug::Error> =
            Err(btleplug::Error::RuntimeError("dbus gone".to_owned()));
        let error = find_peer(listing, "peer-1", String::clone).expect_err("listing failed");
        // One vocabulary on every OS: a listing failure is the adapter's
        // (`adapter.unavailable`), BlueZ included — the BlueZ answer rides
        // in `platform`. Neither is a miss.
        assert_eq!(
            error.code_str(),
            "adapter.unavailable",
            "a listing failure is not a miss"
        );
        if cfg!(target_os = "linux") {
            let platform = error.platform().expect("the BlueZ answer rides the error");
            assert_eq!(platform.domain, "bluez-dbus");
            assert_eq!(platform.code, "org.bluez.Error.Failed");
        }
        assert_eq!(error.operation(), "peer.list");
        let miss = find_peer(Ok(vec!["peer-2".to_owned()]), "peer-1", String::clone)
            .expect_err("peer absent");
        assert_eq!(
            miss.code_str(),
            "peer.not-found",
            "only a real miss is not-found"
        );
        assert_eq!(
            find_peer(Ok(vec!["peer-1".to_owned()]), "peer-1", String::clone).expect("found"),
            "peer-1"
        );
    }

    #[tokio::test]
    async fn gatt_cache_discovers_once_per_connection_not_per_verb() {
        let cache: GattCache<Vec<u8>> = GattCache::new();
        let lookups = AtomicUsize::new(0);
        let discoveries = AtomicUsize::new(0);
        // A fresh BlueZ-shaped peripheral: empty service set until discovered.
        let resolve = || {
            cache.resolve(
                "peer-1",
                || async {
                    lookups.fetch_add(1, Ordering::SeqCst);
                    Ok(Vec::new())
                },
                Vec::is_empty,
                |_empty| async {
                    discoveries.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![1u8])
                },
            )
        };
        for _ in 0..5 {
            assert_eq!(resolve().await.expect("resolve"), vec![1u8]);
        }
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            1,
            "one lookup per connection"
        );
        assert_eq!(
            discoveries.load(Ordering::SeqCst),
            1,
            "one discovery per connection"
        );
        cache.evict("peer-1");
        resolve().await.expect("after eviction");
        assert_eq!(
            discoveries.load(Ordering::SeqCst),
            2,
            "eviction forces rediscovery"
        );
        // A CoreBluetooth-shaped peripheral keeps its services: no discovery.
        cache.clear();
        let shared = cache
            .resolve(
                "peer-2",
                || async { Ok(vec![7u8]) },
                Vec::is_empty,
                |_| async { panic!("populated peripheral never rediscovers") },
            )
            .await
            .expect("populated");
        assert_eq!(shared, vec![7u8]);
    }

    #[tokio::test]
    async fn gatt_cache_refresh_always_rediscovers_and_replaces() {
        let cache: GattCache<Vec<u8>> = GattCache::new();
        cache.insert("peer-1", vec![1u8]);
        let refreshed = cache
            .refresh(
                "peer-1",
                || async { panic!("a cached peripheral is reused") },
                |mut old: Vec<u8>| async move {
                    old.push(2);
                    Ok(old)
                },
            )
            .await
            .expect("refresh");
        assert_eq!(refreshed, vec![1u8, 2]);
        assert_eq!(
            cache.get("peer-1"),
            Some(vec![1u8, 2]),
            "refresh replaces the entry"
        );
    }

    #[tokio::test]
    async fn contended_event_stream_drop_is_observable() {
        let slot: tokio::sync::Mutex<Option<Vec<u8>>> = tokio::sync::Mutex::new(Some(vec![1]));
        let handle = tokio::runtime::Handle::current();
        let before = contended_radio_drops();
        {
            let _held = slot.lock().await;
            assert_eq!(drop_event_stream(&slot, &handle), DropStream::Contended);
        }
        assert_eq!(contended_radio_drops(), before + 1, "contention is counted");
        assert_eq!(drop_event_stream(&slot, &handle), DropStream::Dropped);
        assert_eq!(drop_event_stream(&slot, &handle), DropStream::Empty);
    }

    #[test]
    fn maintained_connect_preserves_every_structured_cleanup_failure() {
        use crate::errors::{DesktopError, PlatformDetail, PlatformValue};
        use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
        let errors = ["maintain", "close"].map(|operation| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Platform,
                operation,
            )
            .with_platform(
                PlatformDetail::new("winrt", "hresult")
                    .with_metadata("hresult", PlatformValue::Text(operation.into())),
            )
        });
        let original = crate::os::winrt_cleanup::cleanup_result(errors.into()).unwrap_err();
        let mapped = super::maintained_connection_failure(original.clone());
        assert_eq!(mapped.code(), BleErrorCode::ConnectionFailed);
        assert_eq!(mapped.operation(), "connection.connect");
        assert_eq!(mapped.platform(), original.platform());
        assert!(
            mapped
                .platform()
                .unwrap()
                .metadata
                .contains_key("failure.1.platform.metadata.hresult")
        );
    }

    #[test]
    fn close_scope_outcomes_skip_only_a_confirmed_missing_peer() {
        let scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let missing = crate::errors::DesktopError::new(
            ubm_core::contracts::BleErrorCode::PeerNotFound,
            ubm_core::contracts::BleErrorDomain::Connection,
            "peer.lookup",
        );
        assert_eq!(
            close_receipt(&scope, Ok(Err(ScopeRelease::PeerGone(missing)))),
            None
        );
        let receipt = close_receipt(
            &scope,
            Ok(Err(ScopeRelease::Failed(
                "characteristic not found".to_owned(),
            ))),
        )
        .expect("an unresolvable scope is a receipt");
        assert_eq!(receipt.scope, scope);
        let timed_out =
            close_receipt(&scope, Err(CloseScopeElapsed)).expect("a hung scope is a receipt");
        assert!(
            timed_out.detail.contains("close scope bound"),
            "{}",
            timed_out.detail
        );
        assert_eq!(
            close_receipt(&scope, Ok(Ok(()))),
            None,
            "a released scope has no receipt"
        );
    }

    #[tokio::test]
    async fn close_retains_failed_scopes_until_actual_retry_succeeds() {
        let failed = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let released = scope("peer-2", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let debt = std::sync::Mutex::new(std::collections::HashSet::from([
            failed.clone(),
            released.clone(),
        ]));
        let failures = super::close_scope_debt(&debt, |owned| {
            let refused = owned == failed;
            async move {
                if refused {
                    Ok(Err(ScopeRelease::Failed("refused".into())))
                } else {
                    Ok(Ok(()))
                }
            }
        })
        .await;
        assert_eq!(failures.len(), 1);
        assert_eq!(
            *debt.lock().unwrap(),
            std::collections::HashSet::from([failed.clone()])
        );
        let mut calls = Vec::new();
        assert!(
            super::close_scope_debt(&debt, |owned| {
                calls.push(owned);
                async { Ok(Ok(())) }
            })
            .await
            .is_empty()
        );
        assert_eq!(calls, vec![failed]);
        assert!(debt.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_close_keeps_every_pending_scope_owned() {
        let first = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let second = scope("peer-2", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let expected = std::collections::HashSet::from([first, second]);
        let debt = std::sync::Mutex::new(expected.clone());
        let mut calls = 0;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            super::close_scope_debt(&debt, |_| {
                calls += 1;
                std::future::pending()
            }),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert_eq!(*debt.lock().unwrap(), expected);
    }

    #[tokio::test]
    async fn close_timeout_retains_debt_but_late_failure_never_revives_released_parent() {
        let owned = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let debt = std::sync::Mutex::new(std::collections::HashSet::from([owned.clone()]));
        assert_eq!(
            super::close_scope_debt(&debt, |_| async { Err(CloseScopeElapsed) })
                .await
                .len(),
            1
        );
        assert!(debt.lock().unwrap().contains(&owned));
        let failures = super::close_scope_debt(&debt, |_| {
            debt.lock().unwrap().remove(&owned);
            async { Ok(Err(ScopeRelease::Failed("late refusal".into()))) }
        })
        .await;
        assert_eq!(failures.len(), 1, "attempt failure remains diagnostic");
        assert!(
            debt.lock().unwrap().is_empty(),
            "confirmed parent release stays retired"
        );
    }

    #[tokio::test]
    async fn f13_disable_failure_preserves_forwarder() {
        // The CCCD is still live when the native disable fails, so the
        // forwarder must stay: values keep flowing until a retry
        // disables it. Only a successful unsubscribe removes the
        // consumer — and a retry then completes the teardown.
        let scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let key = scope_key(&scope);
        let mut forwarders = std::collections::HashMap::new();
        forwarders.insert(key.clone(), forwarder(tokio::spawn(async {}), &scope));
        let mut debt = std::collections::HashSet::new();
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, false);
        assert!(
            forwarders.contains_key(&key),
            "failed disable keeps the forwarder: the CCCD is still live"
        );
        assert!(
            debt.is_empty(),
            "no separate debt while the retained forwarder owns cleanup"
        );
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, true);
        assert!(
            !forwarders.contains_key(&key),
            "retry success removes the forwarder"
        );
        assert!(debt.is_empty(), "retry success holds no debt");
    }

    #[tokio::test]
    async fn f40_a_database_change_parks_live_cccds_and_a_link_loss_settles_them() {
        use super::{PeerRetirement, retire_peer_forwarders};

        let first = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let other = scope("peer-2", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let mut forwarders = std::collections::HashMap::new();
        forwarders.insert(scope_key(&first), forwarder(tokio::spawn(async {}), &first));
        forwarders.insert(scope_key(&other), forwarder(tokio::spawn(async {}), &other));
        let mut debt = std::collections::HashSet::new();
        retire_peer_forwarders(
            &mut forwarders,
            &mut debt,
            "peer-1",
            PeerRetirement::DatabaseChanged,
        );
        assert!(
            !forwarders.contains_key(&scope_key(&first)),
            "the forwarder stops"
        );
        assert!(
            forwarders.contains_key(&scope_key(&other)),
            "other peers untouched"
        );
        assert_eq!(
            debt,
            std::collections::HashSet::from([first.clone()]),
            "the possibly-live CCCD stays owed an unsubscribe"
        );
        retire_peer_forwarders(
            &mut forwarders,
            &mut debt,
            "peer-1",
            PeerRetirement::LinkEnded,
        );
        assert!(debt.is_empty(), "a lost link released the CCCD with it");
    }

    #[test]
    fn f13_unresolved_teardown_without_consumer_parks_debt() {
        // Disabling a scope with no installed forwarder (retry of a
        // half-torn-down enablement): success clears any debt, failure
        // parks it so dispose still attempts the native release.
        let scope = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let key = scope_key(&scope);
        let mut forwarders = std::collections::HashMap::new();
        let mut debt = std::collections::HashSet::new();
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, false);
        assert_eq!(
            debt,
            std::collections::HashSet::from([scope.clone()]),
            "failed consumer-less disable retains the unresolved resource"
        );
        apply_unsubscribe_outcome(&mut forwarders, &mut debt, &key, &scope, true);
        assert!(debt.is_empty(), "retry success clears the debt");
    }

    #[tokio::test]
    async fn bluez_link_loss_keeps_notify_session_owned_until_explicit_retirement() {
        let owned = scope("peer-1", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let other = scope("peer-2", HRM_SERVICE, 0, HRM_MEASUREMENT, 0);
        let mut forwarders = std::collections::HashMap::from([
            (scope_key(&owned), forwarder(tokio::spawn(async {}), &owned)),
            (scope_key(&other), forwarder(tokio::spawn(async {}), &other)),
        ]);
        let mut debt = std::collections::HashSet::new();
        super::retire_peer_forwarders(
            &mut forwarders,
            &mut debt,
            "peer-1",
            super::PeerRetirement::LinkEndedRetainingNotifySession,
        );
        assert_eq!(debt, std::collections::HashSet::from([owned]));
        assert_eq!(forwarders.len(), 1);
        assert!(forwarders.contains_key(&scope_key(&other)));
    }
}
