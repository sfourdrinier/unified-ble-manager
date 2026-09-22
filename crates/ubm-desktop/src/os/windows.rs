//! Windows WinRT adapter: what btleplug 0.12 does not expose (PR210
//! decision 7, PARITY-INVENTORY §2), mirroring the legacy addon
//! (`native/electron/winrt/src/addon.cpp`, `winrt-boundary.inc`).
//!
//! - Link security through `DeviceInformationPairing`: `IsPaired`,
//!   `CanPair`, `PairAsync` (cancelled through its `IAsyncOperation`),
//!   `UnpairAsync`.
//! - `GattSession.MaintainConnection(true)` held for each connection and
//!   released with it (legacy connect sequence, `winrt-boundary.inc`).
//! - CCCD mode selection: btleplug writes `Indicate` whenever a
//!   characteristic can indicate; this adapter rewrites the CCCD of the
//!   exact subscribed instance (vendored `winrt-cccd-mode`) so the legacy
//!   notify preference and a hard requirement both hold.
//! - Adapter listing by native adapter id (`BluetoothAdapter` device
//!   selector), selection of any listed adapter by that id with its own
//!   radio (legacy `SelectAdapter`), the selected adapter's presence
//!   (removal and return), and the `deployment` diagnostic.
//!
//! Selecting a non-default adapter changes which adapter's state and
//! authorization are reported, as it did in the legacy addon; scanning and
//! connections run through the Windows Bluetooth LE stack, which takes no
//! adapter (`BluetoothLEAdvertisementWatcher`,
//! `BluetoothLEDevice::FromBluetoothAddressAsync`), in both.
//!
//! Every object here is opened by address or device id beside btleplug's
//! own: Windows shares one GATT session per device and application, so the
//! maintained session applies to the link btleplug uses.
//!
//! Evidence level: type-checked from macOS (`cargo check --target
//! x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc`); the translation
//! rules are unit-tested in `os::winrt_model`. Behaviour against a live
//! Windows Bluetooth stack is unproven here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattClientCharacteristicConfigurationDescriptorValue, GattSession,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothLEDevice};
use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationUpdate, DevicePairingResult, DeviceWatcher,
    DeviceWatcherStatus,
};
use windows::Foundation::TypedEventHandler;
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::core::HSTRING;
use windows_future::IAsyncOperation;

use super::winrt_model::{
    AdapterPresence, ListedAdapter, PairingStatus, PresenceChange, PresenceReport, RadioAccess,
    UnpairingStatus, address_of_peer, deployment_from_status, pairing_status, radio_access,
    select_listed, unpairing_status,
};
use crate::boundary::{
    AdapterLossCause, BondState, DeliveryMode, HostDeployment, PairOutcome, RadioEvent,
    SecurityState, UnpairOutcome,
};
use crate::errors::DesktopError;

/// A WinRT failure the platform reported only as text.
fn winrt_text(operation: &str, detail: impl std::fmt::Display) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(detail.to_string())
}

/// A WinRT failure with the platform's answer (finding 113): the legacy
/// addon's `{domain:"winrt", code:"hresult", metadata:{hresult}}`.
fn winrt(operation: &str, error: windows::core::Error) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformFailure,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(error.to_string())
    .with_platform(
        crate::errors::PlatformDetail::new("winrt", "hresult")
            .with_message(error.message().to_string())
            .with_metadata(
                "hresult",
                crate::errors::PlatformValue::Text(btleplug::ubm::hresult_code(error.code().0)),
            ),
    )
}

fn security(operation: &str, detail: impl Into<String>) -> DesktopError {
    DesktopError::new(
        BleErrorCode::PlatformSecurity,
        BleErrorDomain::Platform,
        operation,
    )
    .with_detail(detail)
}

fn address(peer_id: &str, operation: &str) -> Result<u64, DesktopError> {
    address_of_peer(peer_id).ok_or_else(|| {
        DesktopError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            operation,
        )
        .with_detail(format!("{peer_id} is not a WinRT Bluetooth address"))
    })
}

async fn device(peer_id: &str, operation: &str) -> Result<BluetoothLEDevice, DesktopError> {
    let address = address(peer_id, operation)?;
    BluetoothLEDevice::FromBluetoothAddressAsync(address)
        .map_err(|error| winrt(operation, error))?
        .await
        .map_err(|error| winrt(operation, error))
}

/// One maintained connection: the GATT session held with
/// `MaintainConnection(true)` and the device whose `GattServicesChanged`
/// handler reports database changes (legacy `winrt-boundary.inc`).
struct Maintained {
    session: GattSession,
    device: BluetoothLEDevice,
    services_changed: i64,
}

/// `GattServicesChanged` reports that found the event queue full. Counted,
/// never silent (the queue holds 64 control events).
static SERVICES_CHANGED_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Dropped `GattServicesChanged` reports since process start.
#[must_use]
pub(crate) fn services_changed_drops() -> u64 {
    SERVICES_CHANGED_DROPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Per-radio WinRT state: in-flight pairings (for cancellation), the
/// maintained GATT sessions of live connections, and the selected
/// adapter's presence watch (stopped at close, or when the radio drops).
pub(crate) struct WinRt {
    pairings: StdMutex<HashMap<String, IAsyncOperation<DevicePairingResult>>>,
    sessions: StdMutex<HashMap<String, Maintained>>,
    adapter_watch: StdMutex<Option<AdapterWatch>>,
}

impl WinRt {
    /// The WinRT state of a radio opened on `adapter_id`, watching that
    /// adapter's presence. A watch that cannot start fails the open: the
    /// adapter's removal would otherwise go unreported.
    pub(crate) fn open(
        adapter_id: &str,
        adapter: btleplug::platform::Adapter,
        events: tokio::sync::mpsc::Sender<RadioEvent>,
    ) -> Result<Self, DesktopError> {
        Ok(Self {
            pairings: StdMutex::new(HashMap::new()),
            sessions: StdMutex::new(HashMap::new()),
            adapter_watch: StdMutex::new(Some(AdapterWatch::start(adapter_id, adapter, events)?)),
        })
    }

    /// Stop the adapter presence watch (radio close). Stopping twice is
    /// nothing to do.
    pub(crate) fn stop_adapter_watch(&self) -> Result<(), DesktopError> {
        let watch = self
            .adapter_watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match watch {
            Some(mut watch) => watch.stop(),
            None => Ok(()),
        }
    }

    fn pairings(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, IAsyncOperation<DevicePairingResult>>> {
        self.pairings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Maintained>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Legacy `ReadWinRtSecurityState`: bond from `IsPaired`, pairing
    /// possibility from `CanPair`.
    pub(crate) async fn security_state(
        &self,
        peer_id: &str,
    ) -> Result<SecurityState, DesktopError> {
        let device = device(peer_id, "security.state").await?;
        let pairing = device
            .DeviceInformation()
            .and_then(|information| information.Pairing())
            .map_err(|error| winrt("security.state", error))?;
        let paired = pairing
            .IsPaired()
            .map_err(|error| winrt("security.state", error))?;
        let can_pair = pairing
            .CanPair()
            .map_err(|error| winrt("security.state", error))?;
        Ok(SecurityState {
            bond: if paired {
                BondState::Bonded
            } else {
                BondState::NotBonded
            },
            pairing_possible: Some(can_pair),
        })
    }

    /// Legacy `PairWinRtPeer`.
    pub(crate) async fn pair(&self, peer_id: &str) -> Result<PairOutcome, DesktopError> {
        let device = device(peer_id, "security.pair").await?;
        let pairing = device
            .DeviceInformation()
            .and_then(|information| information.Pairing())
            .map_err(|error| winrt("security.pair", error))?;
        if pairing
            .IsPaired()
            .map_err(|error| winrt("security.pair", error))?
        {
            return Ok(PairOutcome::AlreadyPaired(
                self.security_state(peer_id).await?,
            ));
        }
        if !pairing
            .CanPair()
            .map_err(|error| winrt("security.pair", error))?
        {
            return Ok(PairOutcome::Rejected(Some(
                "Windows reported that pairing is unavailable".to_owned(),
            )));
        }
        let operation = pairing
            .PairAsync()
            .map_err(|error| winrt("security.pair", error))?;
        self.pairings()
            .insert(peer_id.to_owned(), operation.clone());
        let result = operation.clone().await;
        let cancelled = self.pairings().remove(peer_id).is_none();
        let result = match result {
            Ok(result) => result,
            // A cancelled `IAsyncOperation` completes with an error; the
            // cancellation is this adapter's own request (removed below).
            Err(_) if cancelled => return Ok(PairOutcome::Cancelled),
            Err(error) => return Err(winrt("security.pair", error)),
        };
        let status = result
            .Status()
            .map_err(|error| winrt("security.pair", error))?;
        match pairing_status(status.0, &format!("{status:?}")) {
            PairingStatus::Paired => Ok(PairOutcome::Paired(self.security_state(peer_id).await?)),
            PairingStatus::AlreadyPaired => Ok(PairOutcome::AlreadyPaired(
                self.security_state(peer_id).await?,
            )),
            PairingStatus::Cancelled => Ok(PairOutcome::Cancelled),
            PairingStatus::Rejected(reason) => Ok(PairOutcome::Rejected(Some(reason))),
        }
    }

    /// Cancel the in-flight `PairAsync` for `peer_id`. Nothing in flight is
    /// nothing to cancel.
    pub(crate) fn cancel_pairing(&self, peer_id: &str) -> Result<(), DesktopError> {
        let Some(operation) = self.pairings().remove(peer_id) else {
            return Ok(());
        };
        operation
            .Cancel()
            .map_err(|error| security("security.cancel-pairing", error.to_string()))
    }

    /// Legacy `UnpairWinRtPeer`.
    pub(crate) async fn unpair(&self, peer_id: &str) -> Result<UnpairOutcome, DesktopError> {
        let device = device(peer_id, "security.unpair").await?;
        let result = device
            .DeviceInformation()
            .and_then(|information| information.Pairing())
            .and_then(|pairing| pairing.UnpairAsync())
            .map_err(|error| winrt("security.unpair", error))?
            .await
            .map_err(|error| winrt("security.unpair", error))?;
        let status = result
            .Status()
            .map_err(|error| winrt("security.unpair", error))?;
        match unpairing_status(status.0, &format!("{status:?}")) {
            UnpairingStatus::Unpaired => Ok(UnpairOutcome::Unpaired),
            UnpairingStatus::AlreadyUnpaired => Ok(UnpairOutcome::AlreadyUnpaired),
            UnpairingStatus::Refused(reason) => Err(security("security.unpair", reason)),
        }
    }

    /// Hold the link: `GattSession.MaintainConnection(true)` for the
    /// connection to `peer_id` (legacy connect sequence), and report the
    /// device's `GattServicesChanged` as [`RadioEvent::ServicesChanged`]
    /// (btleplug 0.12's WinRT backend never reports service changes).
    pub(crate) async fn maintain(
        &self,
        peer_id: &str,
        events: tokio::sync::mpsc::Sender<RadioEvent>,
    ) -> Result<(), DesktopError> {
        let device = device(peer_id, "connection.maintain").await?;
        let id = device
            .BluetoothDeviceId()
            .map_err(|error| winrt("connection.maintain", error))?;
        let session = GattSession::FromDeviceIdAsync(&id)
            .map_err(|error| winrt("connection.maintain", error))?
            .await
            .map_err(|error| winrt("connection.maintain", error))?;
        session
            .SetMaintainConnection(true)
            .map_err(|error| winrt("connection.maintain", error))?;
        let peer = peer_id.to_owned();
        let handler = TypedEventHandler::<BluetoothLEDevice, windows::core::IInspectable>::new(
            move |_, _| {
                if events
                    .try_send(RadioEvent::ServicesChanged(peer.clone()))
                    .is_err()
                {
                    SERVICES_CHANGED_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            },
        );
        let services_changed = match device.GattServicesChanged(&handler) {
            Ok(token) => token,
            Err(error) => {
                let _ = release_session(&session);
                return Err(winrt("connection.maintain", error));
            }
        };
        let maintained = Maintained {
            session,
            device,
            services_changed,
        };
        if let Some(previous) = self.sessions().insert(peer_id.to_owned(), maintained) {
            release_maintained(&previous)?;
        }
        Ok(())
    }

    /// Release the maintained session of `peer_id`, if any.
    pub(crate) fn release(&self, peer_id: &str) -> Result<(), DesktopError> {
        match self.sessions().remove(peer_id) {
            Some(maintained) => release_maintained(&maintained),
            None => Ok(()),
        }
    }

    /// Release every maintained session (radio close). Each failure is
    /// returned with its peer, never dropped.
    pub(crate) fn release_all(&self) -> Vec<(String, DesktopError)> {
        let sessions: Vec<(String, Maintained)> = self.sessions().drain().collect();
        sessions
            .into_iter()
            .filter_map(|(peer, maintained)| {
                release_maintained(&maintained)
                    .err()
                    .map(|error| (peer, error))
            })
            .collect()
    }
}

fn release_maintained(maintained: &Maintained) -> Result<(), DesktopError> {
    let handler = maintained
        .device
        .RemoveGattServicesChanged(maintained.services_changed)
        .map_err(|error| winrt("connection.maintain.release", error));
    let session = release_session(&maintained.session);
    handler.and(session)
}

fn release_session(session: &GattSession) -> Result<(), DesktopError> {
    session
        .SetMaintainConnection(false)
        .and_then(|()| session.Close())
        .map_err(|error| winrt("connection.maintain.release", error))
}

/// Legacy `ReadAdapter` authorization: `DeviceAccessInformation` for the
/// selected adapter's device id.
pub(crate) fn adapter_authorization(
    adapter_id: &str,
) -> Result<crate::boundary::AdapterAuthorization, DesktopError> {
    const OPERATION: &str = "adapter.authorization";
    let status = windows::Devices::Enumeration::DeviceAccessInformation::CreateFromId(
        &windows::core::HSTRING::from(adapter_id),
    )
    .and_then(|access| access.CurrentStatus())
    .map_err(|error| winrt(OPERATION, error))?;
    super::winrt_model::authorization_from_access_status(status.0)
        .map_err(|detail| winrt_text(OPERATION, detail))
}

/// One Windows Bluetooth adapter: native id (the selectable identity, as
/// the legacy addon's `nativeAdapterId`), display name, and whether it is
/// the OS default adapter (the one an unnamed open selects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WinRtAdapter {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    pub(crate) default: bool,
}

/// Legacy `ReadAdapters` plus the default adapter mark.
pub(crate) async fn list_adapters() -> Result<Vec<WinRtAdapter>, DesktopError> {
    const OPERATION: &str = "adapter.enumerate";
    let default_id = match BluetoothAdapter::GetDefaultAsync()
        .map_err(|error| winrt(OPERATION, error))?
        .await
    {
        Ok(adapter) => Some(
            adapter
                .DeviceId()
                .map_err(|error| winrt(OPERATION, error))?
                .to_string(),
        ),
        Err(_) => None,
    };
    let selector =
        BluetoothAdapter::GetDeviceSelector().map_err(|error| winrt(OPERATION, error))?;
    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .map_err(|error| winrt(OPERATION, error))?
        .await
        .map_err(|error| winrt(OPERATION, error))?;
    let mut adapters = Vec::new();
    for device in devices {
        let id = device
            .Id()
            .map_err(|error| winrt(OPERATION, error))?
            .to_string();
        let name = device.Name().ok().map(|name| name.to_string());
        let default = default_id.as_deref() == Some(id.as_str());
        adapters.push(WinRtAdapter { id, name, default });
    }
    Ok(adapters)
}

/// Rewrite the CCCD of exactly the characteristic instance btleplug
/// subscribed (finding 39): btleplug writes `Indicate` whenever a
/// characteristic can indicate, so the legacy notify preference and a hard
/// requirement are written here, through the vendored
/// `Peripheral::write_client_configuration` on the same GATT object the
/// subscription uses (UBM patch `winrt-cccd-mode`). Repeated UUIDs are
/// addressed by their handle, never refused as ambiguous.
pub(crate) async fn write_cccd(
    peripheral: &btleplug::platform::Peripheral,
    characteristic: &btleplug::api::Characteristic,
    mode: DeliveryMode,
) -> Result<(), DesktopError> {
    let value = match mode {
        DeliveryMode::Notification => GattClientCharacteristicConfigurationDescriptorValue::Notify,
        DeliveryMode::Indication => GattClientCharacteristicConfigurationDescriptorValue::Indicate,
    };
    peripheral
        .write_client_configuration(characteristic, value)
        .await
        .map_err(|error| {
            use crate::btleplug_backend::WithOs;
            winrt_text("gatt.subscribe.delivery", &error).with_os(&error)
        })
}

/// The legacy addon's `deployment` diagnostic (`addon.cpp`
/// `AdapterDeployment`): whether this process runs with package identity.
pub(crate) fn deployment() -> Result<HostDeployment, DesktopError> {
    let mut length = 0u32;
    // SAFETY: the documented size query — a zero length and no buffer; the
    // call writes only `length`, which outlives it.
    let status = unsafe { GetCurrentPackageFullName(&raw mut length, None) };
    deployment_from_status(status.0).map_err(|detail| winrt_text("host.deployment", detail))
}

/// Select the Windows Bluetooth adapter `wanted` names (legacy
/// `SelectAdapter`, `winrt-boundary.inc`): one Windows enumerates, opened
/// with `BluetoothAdapter::FromIdAsync`, with its own radio when access is
/// granted. The btleplug adapter built on that radio reports that
/// adapter's state; scanning and connections run through the Windows
/// Bluetooth LE stack, which takes no adapter, exactly as in the legacy
/// addon. No name selects the default adapter.
pub(crate) async fn select_adapter(
    wanted: Option<&str>,
) -> Result<(btleplug::platform::Adapter, String), DesktopError> {
    const OPERATION: &str = "adapter.select";
    let adapters = list_adapters().await?;
    let listed: Vec<ListedAdapter> = adapters
        .iter()
        .map(|adapter| ListedAdapter {
            id: adapter.id.clone(),
            default: adapter.default,
        })
        .collect();
    let index = select_listed(&listed, wanted)?;
    let id = listed
        .into_iter()
        .nth(index)
        .map(|adapter| adapter.id)
        .ok_or_else(|| {
            DesktopError::adapter_unavailable(OPERATION)
                .with_detail("the adapter listing changed during selection")
        })?;
    let native = BluetoothAdapter::FromIdAsync(&HSTRING::from(id.as_str()))
        .map_err(|error| winrt(OPERATION, error))?
        .await
        .map_err(|error| {
            DesktopError::adapter_unavailable(OPERATION).with_detail(format!(
                "the Windows Bluetooth adapter {id} could not be opened: {error}"
            ))
        })?;
    let adapter = match radio_access(adapter_authorization(&id)?) {
        RadioAccess::Read => {
            let radio = native
                .GetRadioAsync()
                .map_err(|error| winrt(OPERATION, error))?
                .await
                .map_err(|error| {
                    DesktopError::adapter_unavailable(OPERATION).with_detail(format!(
                        "the Windows Bluetooth adapter {id} has no associated radio: {error}"
                    ))
                })?;
            btleplug::platform::Adapter::from_radio(radio)
        }
        RadioAccess::WithheldUnauthorized => {
            btleplug::platform::Adapter::without_radio(btleplug::api::CentralState::Unauthorized)
        }
        RadioAccess::WithheldUndetermined => {
            btleplug::platform::Adapter::without_radio(btleplug::api::CentralState::Unknown)
        }
    }
    .map_err(|error| DesktopError::adapter_unavailable(OPERATION).with_detail(error.to_string()))?;
    Ok((adapter, id))
}

/// Adapter presence watches that failed or ended on their own since
/// process start. Counted and logged, never silent.
static ADAPTER_WATCH_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Adapter presence watch failures since process start.
#[must_use]
pub(crate) fn adapter_watch_failures() -> u64 {
    ADAPTER_WATCH_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

fn watch_failure(context: &str, detail: impl std::fmt::Display) {
    ADAPTER_WATCH_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!("ubm-desktop: Windows adapter presence watch: {context}: {detail}");
}

/// The selected adapter's presence watch (legacy WinRT backend: a ready
/// adapter that stops being ready tears down): a `DeviceWatcher` over
/// `BluetoothAdapter::GetDeviceSelector()`. The adapter device's removal
/// is [`RadioEvent::AdapterLost`] with [`AdapterLossCause::Removed`]; its
/// return rebinds the btleplug adapter to the new radio and is
/// [`RadioEvent::AdapterRestored`]. Stopped with the radio.
pub(crate) struct AdapterWatch {
    watcher: DeviceWatcher,
    tokens: [i64; 5],
    stopping: Arc<AtomicBool>,
}

impl AdapterWatch {
    fn start(
        adapter_id: &str,
        adapter: btleplug::platform::Adapter,
        events: tokio::sync::mpsc::Sender<RadioEvent>,
    ) -> Result<Self, DesktopError> {
        const OPERATION: &str = "adapter.watch";
        let selector =
            BluetoothAdapter::GetDeviceSelector().map_err(|error| winrt(OPERATION, error))?;
        let watcher = DeviceInformation::CreateWatcherAqsFilter(&selector)
            .map_err(|error| winrt(OPERATION, error))?;
        let presence = Arc::new(StdMutex::new(AdapterPresence::new(adapter_id)));
        let stopping = Arc::new(AtomicBool::new(false));
        let id = adapter_id.to_owned();
        // One report at a time, in the watcher's order: the lock is held
        // while the change is delivered so a loss and a return never swap.
        let report = move |report: PresenceReport| {
            let mut presence = presence.lock().unwrap_or_else(PoisonError::into_inner);
            let event = match presence.observe(report) {
                None => return,
                Some(PresenceChange::Lost) => RadioEvent::AdapterLost(AdapterLossCause::Removed),
                Some(PresenceChange::Restored) => {
                    if let Err(error) = rebind(&adapter, &id) {
                        watch_failure("rebinding the returned adapter's radio", error);
                    }
                    RadioEvent::AdapterRestored
                }
            };
            // A closed queue means the radio closed: nobody is left to tell.
            let _closed = events.blocking_send(event);
            drop(presence);
        };
        let report = Arc::new(report);
        let added = {
            let report = Arc::clone(&report);
            TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, device| {
                match device.ok().and_then(DeviceInformation::Id) {
                    Ok(id) => report(PresenceReport::Added(id.to_string())),
                    Err(error) => watch_failure("reading an added adapter's id", error),
                }
                Ok(())
            })
        };
        let removed = {
            let report = Arc::clone(&report);
            TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, update| {
                match update.ok().and_then(DeviceInformationUpdate::Id) {
                    Ok(id) => report(PresenceReport::Removed(id.to_string())),
                    Err(error) => watch_failure("reading a removed adapter's id", error),
                }
                Ok(())
            })
        };
        // Windows delivers Added/Removed after the first enumeration only
        // to a watcher that also handles Updated.
        let updated =
            TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(|_, _| Ok(()));
        let completed = {
            let report = Arc::clone(&report);
            TypedEventHandler::<DeviceWatcher, windows::core::IInspectable>::new(move |_, _| {
                report(PresenceReport::EnumerationCompleted);
                Ok(())
            })
        };
        let stopped = {
            let stopping = Arc::clone(&stopping);
            TypedEventHandler::<DeviceWatcher, windows::core::IInspectable>::new(
                move |watcher, _| {
                    if !stopping.load(Ordering::Acquire) {
                        let status = watcher
                            .ok()
                            .and_then(DeviceWatcher::Status)
                            .map(|status| format!("{status:?}"))
                            .unwrap_or_else(|error| format!("status unreadable: {error}"));
                        watch_failure("the watch ended on its own", status);
                    }
                    Ok(())
                },
            )
        };
        let mut tokens = [0i64; 5];
        let registrations = [
            watcher.Added(&added),
            watcher.Removed(&removed),
            watcher.Updated(&updated),
            watcher.EnumerationCompleted(&completed),
            watcher.Stopped(&stopped),
        ];
        let mut watch = Self {
            watcher,
            tokens,
            stopping,
        };
        for (slot, registration) in registrations.into_iter().enumerate() {
            match registration {
                Ok(token) => tokens[slot] = token,
                Err(error) => {
                    watch.tokens = tokens;
                    if let Err(cleanup) = watch.stop() {
                        watch_failure("removing a partial registration", cleanup);
                    }
                    return Err(winrt(OPERATION, error));
                }
            }
        }
        watch.tokens = tokens;
        watch
            .watcher
            .Start()
            .map_err(|error| winrt(OPERATION, error))?;
        Ok(watch)
    }

    /// Stop the watch and remove its handlers. Every failure is returned.
    fn stop(&mut self) -> Result<(), DesktopError> {
        const OPERATION: &str = "adapter.watch.stop";
        if self.stopping.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let mut failures = Vec::new();
        let [added, removed, updated, completed, stopped] = self.tokens;
        for (name, result) in [
            ("Added", self.watcher.RemoveAdded(added)),
            ("Removed", self.watcher.RemoveRemoved(removed)),
            ("Updated", self.watcher.RemoveUpdated(updated)),
            (
                "EnumerationCompleted",
                self.watcher.RemoveEnumerationCompleted(completed),
            ),
            ("Stopped", self.watcher.RemoveStopped(stopped)),
        ] {
            if let Err(error) = result {
                failures.push(format!("{name}: {error}"));
            }
        }
        match self.watcher.Status() {
            Ok(DeviceWatcherStatus::Started | DeviceWatcherStatus::EnumerationCompleted) => {
                if let Err(error) = self.watcher.Stop() {
                    failures.push(format!("Stop: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(format!("Status: {error}")),
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(winrt_text(OPERATION, failures.join("; ")))
        }
    }
}

impl Drop for AdapterWatch {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            watch_failure(
                "stopping at drop",
                error.detail().unwrap_or(error.code_str()),
            );
        }
    }
}

/// Read the returned adapter's radio again and rebind the btleplug adapter
/// to it; a withheld radio stays withheld.
fn rebind(adapter: &btleplug::platform::Adapter, id: &str) -> Result<(), String> {
    let authorization = adapter_authorization(id)
        .map_err(|error| error.detail().unwrap_or(error.code_str()).to_owned())?;
    if radio_access(authorization) != RadioAccess::Read {
        return Ok(());
    }
    let radio = BluetoothAdapter::FromIdAsync(&HSTRING::from(id))
        .and_then(|operation| operation.join())
        .and_then(|native| native.GetRadioAsync())
        .and_then(|operation| operation.join())
        .map_err(|error| error.to_string())?;
    adapter
        .rebind_radio(radio)
        .map_err(|error| error.to_string())
}
