//! Windows WinRT adapter: what btleplug 0.12 does not expose (PR210
//! decision 7, PARITY-INVENTORY §2), mirroring the legacy addon
//! (`native/electron/winrt/src/addon.cpp`, `winrt-boundary.inc`).
//!
//! - Link security through `DeviceInformationPairing`: `IsPaired`,
//!   `CanPair`, `PairAsync` (cancelled through its `IAsyncOperation`),
//!   `UnpairAsync`.
//! - `GattSession.MaintainConnection(true)` held for each connection and
//!   released with it (legacy connect sequence, `winrt-boundary.inc`).
//! - CCCD mode selection: the first write to the exact subscribed instance
//!   is the selected mode (vendored `winrt-cccd-mode`). Notify is not
//!   enabled by rewriting the CCCD after an Indicate write.
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
//!
//! Cleanup retains failed WinRT stages and retries only unconfirmed stages.
//! Failed-open watches have no returned radio owner, so a process-owned vault
//! retries them on the next open or explicit radio close of that same native
//! adapter identity. Other adapters neither retry nor report that debt. It has no background
//! retry loop: absent either trigger, ownership lasts until process exit.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use btleplug::api::{Central as _, Peripheral as _};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use windows::Devices::Bluetooth::GenericAttributeProfile::GattSession;
use windows::Devices::Bluetooth::{
    BluetoothAdapter, BluetoothAddressType, BluetoothConnectionStatus, BluetoothLEDevice,
};
use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationUpdate, DevicePairingResult, DeviceWatcher,
    DeviceWatcherStatus,
};
use windows::Foundation::TypedEventHandler;
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::core::HSTRING;
use windows_future::IAsyncOperation;

use super::winrt_cleanup::{
    CallbackGate, CleanupStages, PeerAdmission, RetryVault, release_session_stages, retain_failed,
};
use super::winrt_model::{
    AdapterPresence, DirectoryQuery, ListedAdapter, PairingStatus, PresenceChange, PresenceReport,
    RadioAccess, UnpairingStatus, address_of_peer, deployment_from_status, pairing_status,
    radio_access, select_listed, unpairing_status,
};
use crate::boundary::{
    AdapterLossCause, BondState, HostDeployment, PairOutcome, RadioEvent, SecurityState,
    UnpairOutcome,
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

async fn device(
    peer_id: &str,
    operation: &str,
    kind: Option<crate::boundary::AddressType>,
) -> Result<BluetoothLEDevice, DesktopError> {
    let address = address(peer_id, operation)?;
    match kind {
        Some(kind) => BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(
            address,
            native_address_type(kind),
        ),
        None => BluetoothLEDevice::FromBluetoothAddressAsync(address),
    }
    .map_err(|error| winrt(operation, error))?
    .await
    .map_err(|error| winrt(operation, error))
}

fn native_address_type(kind: crate::boundary::AddressType) -> BluetoothAddressType {
    match kind {
        crate::boundary::AddressType::Public => BluetoothAddressType::Public,
        crate::boundary::AddressType::Random => BluetoothAddressType::Random,
    }
}

/// One maintained connection: the GATT session held with
/// `MaintainConnection(true)` and the device whose `GattServicesChanged`
/// handler reports database changes (legacy `winrt-boundary.inc`).
struct Maintained {
    session: GattSession,
    device: BluetoothLEDevice,
    services_changed: i64,
    cleanup: CleanupStages<3>,
    callbacks: Arc<CallbackGate>,
    availability: Option<(i64, tokio::sync::watch::Sender<Result<bool, DesktopError>>)>,
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
    adapter: btleplug::platform::Adapter,
    adapter_id: String,
    pairings: StdMutex<HashMap<String, IAsyncOperation<DevicePairingResult>>>,
    sessions: StdMutex<HashMap<String, Vec<Maintained>>>,
    session_admission: PeerAdmission,
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
        cleanup_result(retry_failed_watches(adapter_id))?;
        cleanup_result(retry_directory_devices(adapter_id))?;
        Ok(Self {
            adapter: adapter.clone(),
            adapter_id: adapter_id.to_owned(),
            pairings: StdMutex::new(HashMap::new()),
            sessions: StdMutex::new(HashMap::new()),
            session_admission: PeerAdmission::new(),
            adapter_watch: StdMutex::new(Some(AdapterWatch::start(adapter_id, adapter, events)?)),
        })
    }

    async fn device(
        &self,
        peer_id: &str,
        operation: &str,
    ) -> Result<BluetoothLEDevice, DesktopError> {
        let id = peer_id
            .parse::<btleplug::platform::PeripheralId>()
            .map_err(|error| winrt_text(operation, error))?;
        let peripheral = self
            .adapter
            .add_peripheral(&id)
            .await
            .map_err(|error| winrt_text(operation, error))?;
        let kind = peripheral
            .properties()
            .await
            .map_err(|error| winrt_text(operation, error))?
            .and_then(|properties| properties.address_type)
            .map(|kind| match kind {
                btleplug::api::AddressType::Public => crate::boundary::AddressType::Public,
                btleplug::api::AddressType::Random => crate::boundary::AddressType::Random,
            });
        device(peer_id, operation, kind).await
    }

    pub(crate) async fn resolve_address(
        &self,
        peer_id: &str,
        kind: crate::boundary::AddressType,
    ) -> Result<String, DesktopError> {
        let operation = "peer.address-targeting";
        let requested = address(peer_id, operation)?;
        let found = device(peer_id, operation, Some(kind)).await?;
        super::winrt_cleanup::inspect_transient(
            || {
                let observed = found
                    .BluetoothAddress()
                    .map_err(|error| winrt(operation, error))?;
                let observed_type = match found
                    .BluetoothAddressType()
                    .map_err(|error| winrt(operation, error))?
                {
                    BluetoothAddressType::Public => crate::boundary::AddressType::Public,
                    BluetoothAddressType::Random => crate::boundary::AddressType::Random,
                    _ => {
                        return Err(winrt_text(
                            operation,
                            "native lookup returned an unknown LE address type",
                        ));
                    }
                };
                if !super::winrt_model::address_target_matches(
                    requested,
                    kind,
                    observed,
                    observed_type,
                ) {
                    return Err(winrt_text(
                        operation,
                        "native lookup returned a different address or address type",
                    ));
                }
                Ok(())
            },
            || found.Close().map_err(|error| winrt(operation, error)),
        )?;
        self.register_target(peer_id, kind, operation).await?;
        let address = peer_id
            .parse::<btleplug::api::BDAddr>()
            .map_err(|error| winrt_text(operation, error))?;
        Ok(btleplug::platform::PeripheralId::with_address_type(
            address,
            Some(match kind {
                crate::boundary::AddressType::Public => btleplug::api::AddressType::Public,
                crate::boundary::AddressType::Random => btleplug::api::AddressType::Random,
            }),
        )
        .to_string())
    }

    async fn register_target(
        &self,
        peer_id: &str,
        kind: crate::boundary::AddressType,
        operation: &str,
    ) -> Result<(), DesktopError> {
        let id = peer_id
            .parse::<btleplug::api::BDAddr>()
            .map_err(|error| winrt_text(operation, error))?
            .into();
        self.adapter
            .add_peripheral_with_address_type(
                &id,
                match kind {
                    crate::boundary::AddressType::Public => btleplug::api::AddressType::Public,
                    crate::boundary::AddressType::Random => btleplug::api::AddressType::Random,
                },
            )
            .await
            .map_err(|error| winrt_text(operation, error))?;
        Ok(())
    }

    pub(crate) async fn directory_capability_limitations(
        &self,
    ) -> Result<(Option<&'static str>, Option<&'static str>), DesktopError> {
        const OP: &str = "peers.directory.capability";
        let adapter = BluetoothAdapter::GetDefaultAsync()
            .map_err(|error| winrt(OP, error))?
            .await
            .map_err(|error| winrt(OP, error))?;
        if adapter.DeviceId().map_err(|error| winrt(OP, error))? != self.adapter_id.as_str() {
            return Ok((
                Some("winrt-directory-requires-default-adapter"),
                Some("winrt-directory-requires-default-adapter"),
            ));
        }
        let type_name = HSTRING::from("Windows.Devices.Bluetooth.BluetoothLEDevice");
        let known = windows::Foundation::Metadata::ApiInformation::IsMethodPresent(
            &type_name,
            &HSTRING::from("GetDeviceSelector"),
        )
        .map_err(|error| winrt(OP, error))?;
        let connected = windows::Foundation::Metadata::ApiInformation::IsMethodPresent(
            &type_name,
            &HSTRING::from("GetDeviceSelectorFromConnectionStatus"),
        )
        .map_err(|error| winrt(OP, error))?;
        Ok((
            (!known || !connected).then_some("winrt-known-directory-api-unavailable"),
            (!connected).then_some("winrt-connected-directory-api-unavailable"),
        ))
    }

    async fn directory_peers(
        &self,
        kind: DirectoryQuery,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        let operation = kind.operation();
        cleanup_result(self.retry_directory_cleanup())?;
        let (known_limit, connected_limit) = self.directory_capability_limitations().await?;
        let limitation = if matches!(kind, DirectoryQuery::Connected) {
            connected_limit
        } else {
            known_limit
        };
        if let Some(limitation) = limitation {
            return Err(DesktopError::new(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                operation,
            )
            .with_detail(limitation));
        }
        // The generic LE selector omits some currently connected, unpaired
        // peers. Include the native connected inventory without starting the
        // unpaired discovery selector (which can initiate a radio scan).
        let mut devices = Vec::new();
        for scope in kind.scopes() {
            let selector = match scope {
                DirectoryQuery::Bonded => {
                    BluetoothLEDevice::GetDeviceSelectorFromPairingState(true)
                }
                DirectoryQuery::Known => BluetoothLEDevice::GetDeviceSelector(),
                DirectoryQuery::Connected => {
                    BluetoothLEDevice::GetDeviceSelectorFromConnectionStatus(
                        BluetoothConnectionStatus::Connected,
                    )
                }
            }
            .map_err(|error| winrt(operation, error))?;
            let records = DeviceInformation::FindAllAsyncAqsFilter(&selector)
                .map_err(|error| winrt(operation, error))?
                .await
                .map_err(|error| winrt(operation, error))?;
            devices.extend(records);
            if devices.len() > 4096 {
                return Err(DesktopError::new(
                    BleErrorCode::CapabilityLimited,
                    BleErrorDomain::Capability,
                    operation,
                )
                .with_detail("the native peer directory exceeds its 4096-record bound"));
            }
        }
        let mut peers = Vec::new();
        for information in devices {
            // Read the current OS fact as well as the enumeration selector;
            // a concurrently unpaired device no longer belongs in this set.
            if matches!(kind, DirectoryQuery::Bonded)
                && !information
                    .Pairing()
                    .map_err(|error| winrt(operation, error))?
                    .IsPaired()
                    .map_err(|error| winrt(operation, error))?
            {
                continue;
            }
            let device = BluetoothLEDevice::FromIdAsync(
                &information.Id().map_err(|error| winrt(operation, error))?,
            )
            .map_err(|error| winrt(operation, error))?
            .await
            .map_err(|error| winrt(operation, error))?;
            let (address_type, mut peer) = super::winrt_cleanup::inspect_transient(
                || {
                    let native_address = device
                        .BluetoothAddress()
                        .map_err(|error| winrt(operation, error))?;
                    let peer_id = btleplug::api::BDAddr::try_from(native_address)
                        .map_err(|error| winrt_text(operation, error))?
                        .to_string();
                    let kind = match device
                        .BluetoothAddressType()
                        .map_err(|error| winrt(operation, error))?
                    {
                        BluetoothAddressType::Public => crate::boundary::AddressType::Public,
                        BluetoothAddressType::Random => crate::boundary::AddressType::Random,
                        _ => {
                            return Err(winrt_text(
                                operation,
                                "bonded peer has an unknown LE address type",
                            ));
                        }
                    };
                    Ok((
                        kind,
                        crate::boundary::DirectoryPeer {
                            peer_id,
                            name: Some(
                                information
                                    .Name()
                                    .map_err(|error| winrt(operation, error))?
                                    .to_string(),
                            ),
                            connection: match device
                                .ConnectionStatus()
                                .map_err(|error| winrt(operation, error))?
                            {
                                BluetoothConnectionStatus::Connected => "connected",
                                BluetoothConnectionStatus::Disconnected => "disconnected",
                                _ => "unknown",
                            },
                        },
                    ))
                },
                || match device.Close() {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        DIRECTORY_DEVICES.push(&self.adapter_id, device.clone());
                        Err(winrt(operation, error))
                    }
                },
            )?;
            if matches!(kind, DirectoryQuery::Connected) && peer.connection != "connected" {
                continue;
            }
            self.register_target(&peer.peer_id, address_type, operation)
                .await?;
            let address = peer
                .peer_id
                .parse::<btleplug::api::BDAddr>()
                .map_err(|error| winrt_text(operation, error))?;
            peer.peer_id = btleplug::platform::PeripheralId::with_address_type(
                address,
                Some(match address_type {
                    crate::boundary::AddressType::Public => btleplug::api::AddressType::Public,
                    crate::boundary::AddressType::Random => btleplug::api::AddressType::Random,
                }),
            )
            .to_string();
            peers.push(peer);
        }
        peers.sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
        peers.dedup_by(|left, right| left.peer_id == right.peer_id);
        Ok(peers)
    }

    pub(crate) async fn bonded_peers(
        &self,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_peers(DirectoryQuery::Bonded).await
    }
    pub(crate) async fn known_peers(
        &self,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_peers(DirectoryQuery::Known).await
    }
    pub(crate) async fn connected_peers(
        &self,
    ) -> Result<Vec<crate::boundary::DirectoryPeer>, DesktopError> {
        self.directory_peers(DirectoryQuery::Connected).await
    }

    fn retry_directory_cleanup(&self) -> Vec<DesktopError> {
        retry_directory_devices(&self.adapter_id)
    }

    /// Stop the adapter presence watch (radio close). Stopping twice is
    /// nothing to do.
    pub(crate) fn stop_adapter_watch(&self) -> Vec<DesktopError> {
        let mut watch = self
            .adapter_watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut failures = watch.as_mut().map_or_else(Vec::new, AdapterWatch::stop);
        if failures.is_empty() {
            *watch = None;
        }
        drop(watch);
        failures.extend(retry_failed_watches(&self.adapter_id));
        failures.extend(self.retry_directory_cleanup());
        failures
    }

    fn pairings(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, IAsyncOperation<DevicePairingResult>>> {
        self.pairings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Maintained>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Legacy `ReadWinRtSecurityState`: bond from `IsPaired`, pairing
    /// possibility from `CanPair`.
    pub(crate) async fn security_state(
        &self,
        peer_id: &str,
    ) -> Result<SecurityState, DesktopError> {
        let device = self.device(peer_id, "security.state").await?;
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
        let device = self.device(peer_id, "security.pair").await?;
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
        let device = self.device(peer_id, "security.unpair").await?;
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
        let mut healthy = self
            .session_admission
            .acquire(peer_id)
            .await
            .map_err(|()| {
                winrt_text(
                    "connection.maintain",
                    "radio cleanup has closed session admission",
                )
            })?;
        if *healthy {
            return Ok(());
        }
        // A healthy owner is shared by additional leases. Only failed
        // cleanup is retried before creating a replacement shared session.
        cleanup_result(self.release_owned(peer_id))?;
        let device = self.device(peer_id, "connection.maintain").await?;
        let id = device
            .BluetoothDeviceId()
            .map_err(|error| winrt("connection.maintain", error))?;
        let session = GattSession::FromDeviceIdAsync(&id)
            .map_err(|error| winrt("connection.maintain", error))?
            .await
            .map_err(|error| winrt("connection.maintain", error))?;
        if let Err(error) = session.SetMaintainConnection(true) {
            let mut owner = Maintained {
                session,
                device,
                services_changed: 0,
                cleanup: CleanupStages::new([false, true, true]),
                callbacks: Arc::new(CallbackGate::new()),
                availability: None,
            };
            let mut failures = vec![winrt("connection.maintain", error)];
            let cleanup = release_maintained(&mut owner);
            if !cleanup.is_empty() {
                self.sessions()
                    .entry(peer_id.to_owned())
                    .or_default()
                    .push(owner);
            }
            failures.extend(cleanup);
            return cleanup_result(failures);
        }
        let peer = peer_id.to_owned();
        let callbacks = Arc::new(CallbackGate::new());
        let handler_callbacks = Arc::clone(&callbacks);
        let handler = TypedEventHandler::<BluetoothLEDevice, windows::core::IInspectable>::new(
            move |_, _| {
                handler_callbacks.run(|| {
                    if events
                        .try_send(RadioEvent::ServicesChanged(peer.clone()))
                        .is_err()
                    {
                        SERVICES_CHANGED_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
                Ok(())
            },
        );
        let services_changed = match device.GattServicesChanged(&handler) {
            Ok(token) => token,
            Err(error) => {
                let mut owner = Maintained {
                    session,
                    device,
                    services_changed: 0,
                    cleanup: CleanupStages::new([false, true, true]),
                    callbacks,
                    availability: None,
                };
                let failures = release_maintained(&mut owner);
                let mut reported = vec![winrt("connection.maintain", error)];
                if !failures.is_empty() {
                    self.sessions()
                        .entry(peer_id.to_owned())
                        .or_default()
                        .push(owner);
                }
                reported.extend(failures);
                return cleanup_result(reported);
            }
        };
        let maintained = Maintained {
            session,
            device,
            services_changed,
            cleanup: CleanupStages::new([true; 3]),
            callbacks,
            availability: None,
        };
        self.sessions()
            .entry(peer_id.to_owned())
            .or_default()
            .push(maintained);
        *healthy = true;
        Ok(())
    }

    /// Ask Windows to maintain the link before GATT discovery and await its
    /// actual connection-status callback. The maintained owner owns the
    /// callback too, so dropped acquisition is compensated by normal release.
    pub(crate) async fn wait_available(
        &self,
        peer_id: &str,
        events: tokio::sync::mpsc::Sender<RadioEvent>,
    ) -> Result<(), DesktopError> {
        self.maintain(peer_id, events).await?;
        let mut receiver = {
            let mut owners = self.sessions();
            let owner = owners
                .get_mut(peer_id)
                .and_then(|owners| owners.last_mut())
                .ok_or_else(|| {
                    winrt_text(
                        "connection.connect.when-available",
                        "maintained session owner is missing",
                    )
                })?;
            if owner.availability.is_none() {
                let (sender, _) = tokio::sync::watch::channel(Ok(false));
                let status_sender = sender.clone();
                let device = owner.device.clone();
                let callbacks = Arc::clone(&owner.callbacks);
                let handler =
                    TypedEventHandler::<BluetoothLEDevice, windows::core::IInspectable>::new(
                        move |_, _| {
                            callbacks.run(|| {
                                publish_availability(
                                    &status_sender,
                                    device
                                        .ConnectionStatus()
                                        .map(|status| {
                                            status == BluetoothConnectionStatus::Connected
                                        })
                                        .map_err(|error| {
                                            winrt("connection.connect.when-available", error)
                                        }),
                                );
                            });
                            Ok(())
                        },
                    );
                let token = owner
                    .device
                    .ConnectionStatusChanged(&handler)
                    .map_err(|error| winrt("connection.connect.when-available", error))?;
                owner.availability = Some((token, sender));
            }
            let (_, sender) = owner.availability.as_ref().expect("availability installed");
            publish_availability(
                sender,
                owner
                    .device
                    .ConnectionStatus()
                    .map(|status| status == BluetoothConnectionStatus::Connected)
                    .map_err(|error| winrt("connection.connect.when-available", error)),
            );
            sender.subscribe()
        };
        loop {
            if receiver.borrow_and_update().clone()? {
                return Ok(());
            }
            receiver
                .changed()
                .await
                .map_err(|error| winrt_text("connection.connect.when-available", error))?;
        }
    }

    /// Release the maintained session of `peer_id`, if any.
    pub(crate) fn release(&self, peer_id: &str) -> Result<(), DesktopError> {
        let mut healthy = self.session_admission.release(peer_id).map_err(|()| {
            winrt_text(
                "connection.maintain.release",
                "session acquisition or cleanup is in flight; retry is required",
            )
        })?;
        *healthy = false;
        cleanup_result(self.release_owned(peer_id))
    }

    fn release_owned(&self, peer_id: &str) -> Vec<DesktopError> {
        let mut sessions = self.sessions();
        let failures = sessions
            .get_mut(peer_id)
            .map_or_else(Vec::new, |owners| retain_failed(owners, release_maintained));
        sessions.retain(|_, owners| !owners.is_empty());
        failures
    }

    /// Release every maintained session (radio close). Each failure is
    /// returned with its peer, never dropped.
    pub(crate) fn release_all(&self) -> Vec<(String, DesktopError)> {
        let mut failures = Vec::new();
        for peer in self.session_admission.close() {
            match self.session_admission.release(&peer) {
                Ok(mut healthy) => {
                    *healthy = false;
                    failures.extend(
                        self.release_owned(&peer)
                            .into_iter()
                            .map(|error| (peer.clone(), error)),
                    );
                }
                Err(()) => failures.push((
                    peer,
                    winrt_text(
                        "connection.maintain.release",
                        "session acquisition or cleanup is in flight; retry is required",
                    ),
                )),
            }
        }
        failures
    }
}

fn publish_availability(
    sender: &tokio::sync::watch::Sender<Result<bool, DesktopError>>,
    observation: Result<bool, DesktopError>,
) {
    sender.send_if_modified(|current| {
        if current.is_err() {
            return false;
        }
        *current = observation;
        true
    });
}

fn release_maintained(maintained: &mut Maintained) -> Vec<DesktopError> {
    maintained.callbacks.close();
    let mut failures = Vec::new();
    if let Some((token, _)) = maintained.availability.as_ref() {
        match maintained.device.RemoveConnectionStatusChanged(*token) {
            Ok(()) => maintained.availability = None,
            Err(error) => failures.push(winrt("connection.maintain.release.availability", error)),
        }
    }
    failures.extend(release_session_stages(&mut maintained.cleanup, |stage| {
        let (operation, result) = match stage {
            0 => (
                "connection.maintain.release.handler",
                maintained
                    .device
                    .RemoveGattServicesChanged(maintained.services_changed),
            ),
            1 => (
                "connection.maintain.release.disable",
                maintained.session.SetMaintainConnection(false),
            ),
            _ => (
                "connection.maintain.release.close",
                maintained.session.Close(),
            ),
        };
        result.map_err(|error| winrt(operation, error))
    }));
    failures
}

fn cleanup_result(failures: Vec<DesktopError>) -> Result<(), DesktopError> {
    super::winrt_cleanup::cleanup_result(failures)
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
#[derive(Clone)]
pub(crate) struct AdapterWatch {
    adapter_id: String,
    watcher: DeviceWatcher,
    tokens: [i64; 5],
    stopping: Arc<AtomicBool>,
    cleanup: CleanupStages<6>,
    callbacks: Arc<CallbackGate>,
}

// Failed open has no radio owner to return. Retain its exact native watch
// until the next open or explicit radio close retries it; no background loop.
// Directory reads acquire no connection lease. A refused Close keeps the
// exact object owned across radio drop, and only the same adapter retries it.
static DIRECTORY_DEVICES: RetryVault<BluetoothLEDevice> = RetryVault::new();
fn retry_directory_devices(adapter_id: &str) -> Vec<DesktopError> {
    DIRECTORY_DEVICES
        .retry(adapter_id, |device| match device.Close() {
            Ok(()) => Vec::new(),
            Err(error) => vec![winrt("peers.directory.cleanup", error)],
        })
        .unwrap_or_else(|()| {
            vec![winrt_text(
                "peers.directory.cleanup",
                "native directory cleanup is still in flight; retry is required",
            )]
        })
}

static FAILED_WATCHES: RetryVault<AdapterWatch> = RetryVault::new();

fn retry_failed_watches(adapter_id: &str) -> Vec<DesktopError> {
    FAILED_WATCHES
        .retry(adapter_id, AdapterWatch::stop)
        .unwrap_or_else(|()| {
            vec![winrt_text(
                "adapter.watch.cleanup",
                "watch cleanup is still in flight or newly pending; retry is required",
            )]
        })
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
        let callbacks = Arc::new(CallbackGate::new());
        let report_callbacks = Arc::clone(&callbacks);
        let id = adapter_id.to_owned();
        let report_stopping = Arc::clone(&stopping);
        // One report at a time, in the watcher's order: the lock is held
        // while the change is delivered so a loss and a return never swap.
        let report = move |report: PresenceReport| {
            report_callbacks.run(|| {
                if report_stopping.load(Ordering::Acquire) {
                    return;
                }
                let mut presence = presence.lock().unwrap_or_else(PoisonError::into_inner);
                let event = match presence.observe(report) {
                    None => return,
                    Some(PresenceChange::Lost) => {
                        RadioEvent::AdapterLost(AdapterLossCause::Removed)
                    }
                    Some(PresenceChange::Restored) => {
                        if let Err(error) = rebind(&adapter, &id) {
                            watch_failure("rebinding the returned adapter's radio", error);
                        }
                        RadioEvent::AdapterRestored
                    }
                };
                if let Err(error) = events.try_send(event) {
                    watch_failure("delivering adapter presence", error);
                }
                drop(presence);
            });
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
            adapter_id: adapter_id.to_owned(),
            watcher,
            tokens,
            stopping,
            cleanup: CleanupStages::new([false, false, false, false, false, true]),
            callbacks,
        };
        let mut failures = Vec::new();
        for (slot, registration) in registrations.into_iter().enumerate() {
            match registration {
                Ok(token) => {
                    tokens[slot] = token;
                    watch.cleanup.activate(slot);
                }
                Err(error) => {
                    failures.push(winrt(OPERATION, error));
                }
            }
        }
        watch.tokens = tokens;
        if failures.is_empty()
            && let Err(error) = watch.watcher.Start()
        {
            failures.push(winrt(OPERATION, error));
        }
        if !failures.is_empty() {
            let cleanup = watch.stop();
            if !cleanup.is_empty() {
                FAILED_WATCHES.push(adapter_id, watch);
            }
            failures.extend(cleanup);
            return match cleanup_result(failures) {
                Err(error) => Err(error),
                Ok(()) => unreachable!("failed acquisition has a diagnostic"),
            };
        }
        Ok(watch)
    }

    /// Stop the watch and remove its handlers. Every failure is returned.
    fn stop(&mut self) -> Vec<DesktopError> {
        self.stopping.store(true, Ordering::Release);
        self.callbacks.close();
        let [added, removed, updated, completed, stopped] = self.tokens;
        self.cleanup.run(|stage| {
            let (operation, result) = match stage {
                0 => (
                    "adapter.watch.remove.added",
                    self.watcher.RemoveAdded(added),
                ),
                1 => (
                    "adapter.watch.remove.removed",
                    self.watcher.RemoveRemoved(removed),
                ),
                2 => (
                    "adapter.watch.remove.updated",
                    self.watcher.RemoveUpdated(updated),
                ),
                3 => (
                    "adapter.watch.remove.completed",
                    self.watcher.RemoveEnumerationCompleted(completed),
                ),
                4 => (
                    "adapter.watch.remove.stopped",
                    self.watcher.RemoveStopped(stopped),
                ),
                _ => (
                    "adapter.watch.stop",
                    self.watcher.Status().and_then(|status| match status {
                        DeviceWatcherStatus::Started
                        | DeviceWatcherStatus::EnumerationCompleted => self.watcher.Stop(),
                        _ => Ok(()),
                    }),
                ),
            };
            result.map_err(|error| winrt(operation, error))
        })
    }
}

impl Drop for AdapterWatch {
    fn drop(&mut self) {
        let failures = self.stop();
        if !failures.is_empty() {
            FAILED_WATCHES.push(&self.adapter_id, self.clone());
        }
        for error in failures {
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
