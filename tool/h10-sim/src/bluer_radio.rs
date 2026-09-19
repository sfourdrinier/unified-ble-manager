//! Linux radio backend: `bluer` 0.17.4 directly behind [`PeripheralRadio`].
//!
//! Registers the advertisement shape [`crate::advertisement`] describes
//! (Type `peripheral`, ServiceUUIDs 180D + FEEE, `LocalName`, empty
//! `Includes` — byte-identical to what `ble-peripheral-rust` sends), with
//! two deliberate differences from that crate's BlueZ backend:
//!
//! * every `LEAdvertisement1` property is constructed here, so host-side
//!   bisection (Discoverable, Includes, Duration, intervals) needs no
//!   dependency change — see the Linux section of the README;
//! * the GATT application is registered *before* the advertisement, so a
//!   failed advertisement never leaves services advertised without a server,
//!   and a failure names the stage that rejected it (`serve GATT
//!   application` vs `register advertisement`).
//!
//! Advertising takes one of two paths (`--linux-advertising`, see
//! [`crate::linux_advertising`]): an `LEAdvertisement1` object through
//! bluetoothd (default), or — explicit opt-in — an instance the sim adds on
//! the kernel's MGMT control channel ([`super::mgmt_socket`]) while the GATT
//! application stays on bluetoothd.
//!
//! Read/write/notify plumbing mirrors the `ble-peripheral-rust` BlueZ
//! backend against the same `bluer` version: callback funs bridge into the
//! simulator loop over channels, notification sessions are tracked per
//! characteristic UUID, and refusals answer `NotSupported` exactly as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bluer::{
    adv::{Advertisement, AdvertisementHandle, Type as AdvertisementType},
    gatt::{
        local::{
            characteristic_control, service_control, Application, ApplicationHandle,
            Characteristic, CharacteristicControl, CharacteristicControlEvent,
            CharacteristicControlHandle, CharacteristicNotify, CharacteristicNotifyMethod,
            CharacteristicRead, CharacteristicReadRequest, CharacteristicWrite,
            CharacteristicWriteMethod, CharacteristicWriteRequest, ReqError, Service,
        },
        CharacteristicWriter,
    },
    Adapter, AdapterEvent, AdapterProperty,
};
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::mgmt_socket::MgmtAdvertiser;
use crate::linux_advertising::{self, BluezRegistrationFailure};
use crate::radio::{
    short_of, CharPermission, CharProperty, CharSpec, PeripheralRadio, RadioError, RadioEvent,
    RadioReadAnswer, ServiceSpec,
};

fn backend_error(stage: &str, error: bluer::Error) -> RadioError {
    RadioError(format!("bluer {stage}: {error}"))
}

/// Subscription session handler: characteristic UUIDs plus its notify stream.
struct CharNotifyHandler {
    service_uuid: Uuid,
    characteristic_uuid: Uuid,
    control: CharacteristicControl,
}

/// [`PeripheralRadio`] implemented with `bluer` on Linux.
pub struct BluerRadio {
    adapter: Adapter,
    services: Vec<ServiceSpec>,
    adv_handle: Option<AdvertisementHandle>,
    app_handle: Option<ApplicationHandle>,
    events: mpsc::Sender<RadioEvent>,
    writers: Arc<Mutex<HashMap<Uuid, Arc<CharacteristicWriter>>>>,
    /// Staged manufacturer data (None = not configured or empty payload).
    mfr: Option<(u16, Vec<u8>)>,
    /// `Some` in `--linux-advertising mgmt-legacy`: the advertisement is the
    /// sim's own MGMT instance instead of an `LEAdvertisement1` object.
    mgmt: Option<MgmtAdvertiser>,
    /// Set when bluetoothd failed with the known BlueZ/kernel mismatch.
    advertising_unavailable: bool,
    _drop_tx: oneshot::Sender<()>,
}

#[async_trait]
impl PeripheralRadio for BluerRadio {
    async fn open(events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError> {
        let session = bluer::Session::new()
            .await
            .map_err(|error| backend_error("open session", error))?;
        let adapter = session
            .default_adapter()
            .await
            .map_err(|error| backend_error("open adapter", error))?;
        adapter
            .set_powered(true)
            .await
            .map_err(|error| backend_error("power on", error))?;

        let (drop_tx, drop_rx) = oneshot::channel::<()>();
        // Power changes are observed for the radio's lifetime. Other adapter
        // events (devices appearing, other properties) are skipped, never a
        // reason to stop watching.
        let mut adapter_stream = adapter
            .events()
            .await
            .map_err(|error| backend_error("watch adapter events", error))?;
        let sender = events.clone();
        tokio::spawn(async move {
            let stream_future = async {
                while let Some(event) = adapter_stream.next().await {
                    if let AdapterEvent::PropertyChanged(AdapterProperty::Powered(powered)) = event
                    {
                        if sender.send(RadioEvent::Powered(powered)).await.is_err() {
                            break;
                        }
                    }
                }
            };
            tokio::select! {
                _ = stream_future => {},
                _ = drop_rx => {}
            }
        });

        Ok(Self {
            adapter,
            services: Vec::new(),
            adv_handle: None,
            app_handle: None,
            events,
            writers: Arc::new(Mutex::new(HashMap::new())),
            mfr: None,
            mgmt: None,
            advertising_unavailable: false,
            _drop_tx: drop_tx,
        })
    }

    async fn is_powered(&mut self) -> Result<bool, RadioError> {
        self.adapter
            .is_powered()
            .await
            .map_err(|error| backend_error("is powered", error))
    }

    async fn is_advertising(&mut self) -> Result<bool, RadioError> {
        if let Some(mgmt) = self.mgmt.as_mut() {
            let active = tokio::task::block_in_place(|| mgmt.is_active()).map_err(RadioError)?;
            return Ok(active && self.app_handle.is_some());
        }
        let instances = self
            .adapter
            .active_advertising_instances()
            .await
            .map_err(|error| backend_error("is advertising", error))?;
        Ok(instances > 0 && self.adv_handle.is_some())
    }

    async fn start_advertising(&mut self, name: &str, uuids: &[Uuid]) -> Result<(), RadioError> {
        // GATT application first — but only once: re-registering an identical
        // application makes BlueZ emit Service Changed to connected centrals,
        // and a real H10 never re-registers on re-advertise. The declarations
        // are fixed at startup (`add_service` refuses late additions), so a
        // live application is always current.
        let had_app = self.app_handle.is_some();
        if !had_app {
            // The name already fits the scan-response budget — the caller
            // enforces it via `crate::advertisement::fit_name`.
            let (handlers, services) = build_services(self.services.clone(), self.events.clone())?;
            let app_handle = self
                .adapter
                .serve_gatt_application(Application {
                    services,
                    ..Default::default()
                })
                .await
                .map_err(|error| backend_error("serve GATT application", error))?;
            self.setup_char_handlers(handlers);
            self.app_handle = Some(app_handle);
        }

        if let Some(mgmt) = self.mgmt.as_mut() {
            let uuids16 = uuids
                .iter()
                .map(|uuid| {
                    short_of(uuid).ok_or_else(|| {
                        RadioError(format!("advertised UUID {uuid} is not a 16-bit UUID"))
                    })
                })
                .collect::<Result<Vec<u16>, RadioError>>()?;
            let mfr = self
                .mfr
                .as_ref()
                .map(|(company, payload)| (*company, payload.as_slice()));
            // A failed add after a fresh registration drops the application
            // again: never a half-registered peripheral.
            if let Err(error) = tokio::task::block_in_place(|| mgmt.start(name, &uuids16, mfr)) {
                if !had_app {
                    self.app_handle = None;
                }
                return Err(RadioError(format!(
                    "mgmt-legacy add advertisement: {error}"
                )));
            }
            return Ok(());
        }

        let instances_before = self.advertising_instances().await;
        match self
            .adapter
            .advertise(h10_advertisement(name, uuids, self.mfr.as_ref()))
            .await
        {
            Ok(adv_handle) => {
                self.adv_handle = Some(adv_handle);
                Ok(())
            }
            Err(error) => {
                // Never leave a half-registered peripheral behind after a
                // fresh registration: dropping the handle unregisters the
                // GATT application again.
                if !had_app {
                    self.app_handle = None;
                }
                let diagnosis =
                    linux_advertising::diagnose_bluez_registration(&BluezRegistrationFailure {
                        failed_kind: matches!(error.kind, bluer::ErrorKind::Failed),
                        message: &error.message,
                        instances_before: &instances_before,
                        kernel_release: &kernel_release(),
                    });
                self.advertising_unavailable = diagnosis.known_mismatch;
                Err(RadioError(format!(
                    "bluer register advertisement: {}",
                    diagnosis.text
                )))
            }
        }
    }

    async fn stop_advertising(&mut self) -> Result<(), RadioError> {
        // Only the advertisement goes away: the GATT application stays
        // registered, so connected centrals see no Service Changed and the
        // next advertise needs no re-registration. Process exit unregisters
        // the application with the D-Bus connection.
        let removed = match self.mgmt.as_mut() {
            Some(mgmt) => tokio::task::block_in_place(|| mgmt.stop()).map(|_| ()),
            None => Ok(()),
        };
        self.adv_handle = None;
        removed.map_err(|error| RadioError(format!("mgmt-legacy remove advertisement: {error}")))
    }

    async fn add_service(&mut self, service: &ServiceSpec) -> Result<(), RadioError> {
        // Declarations are fixed at startup: a live application would never
        // see a late addition, so it is refused loudly instead of queued
        // into a config nobody reads.
        if self.app_handle.is_some() {
            return Err(RadioError(format!(
                "service {} added after the GATT application was registered",
                service.uuid
            )));
        }
        self.services.push(service.clone());
        Ok(())
    }

    async fn set_adv_manufacturer_data(
        &mut self,
        company: u16,
        data: Vec<u8>,
    ) -> Result<(), RadioError> {
        // An empty payload stages nothing: a company id with no payload bytes
        // claims more than the captures pin down, so it stays off the air.
        self.mfr = if data.is_empty() {
            None
        } else {
            Some((company, data))
        };
        Ok(())
    }

    fn supports_manufacturer_data(&self) -> bool {
        true
    }

    async fn disconnect_centrals(&mut self) -> Result<usize, RadioError> {
        // Genuine disconnect: Device1.Disconnect on every connected device,
        // using the caller's own D-Bus session (no escalation — the same
        // access that registered the advertisement). Per-device failures are
        // reported, never silent.
        let addresses = self
            .adapter
            .device_addresses()
            .await
            .map_err(|error| backend_error("list devices", error))?;
        let mut dropped = 0;
        for address in addresses {
            let device = match self.adapter.device(address) {
                Ok(device) => device,
                Err(error) => {
                    eprintln!("h10-sim: disconnect {address}: cannot open device: {error:?}");
                    continue;
                }
            };
            let connected = device
                .is_connected()
                .await
                .map_err(|error| backend_error("is connected", error))?;
            if !connected {
                continue;
            }
            match device.disconnect().await {
                Ok(()) => dropped += 1,
                Err(error) => {
                    eprintln!("h10-sim: disconnect {address} failed: {error:?}");
                }
            }
        }
        Ok(dropped)
    }

    async fn notify(&mut self, characteristic: Uuid, value: Vec<u8>) -> Result<bool, RadioError> {
        let writer = match self.writers.lock() {
            Ok(writers) => writers.get(&characteristic).cloned(),
            Err(error) => return Err(RadioError(format!("bluer notify lock: {error}"))),
        };
        let Some(writer) = writer else {
            // Nobody subscribed: the normal case for stream ticks, never an
            // error — and never reported as a delivery either.
            return Ok(false);
        };
        // Awaited, not spawned: `send` only waits for socket buffer space,
        // never for the ATT confirmation, so a dead session surfaces here
        // instead of timing out the central.
        match writer.send(&value).await {
            Ok(()) => Ok(true),
            Err(error) => {
                // The session died under us: drop the stale writer so the
                // next send is a clean miss, and report loudly at the caller.
                if let Ok(mut writers) = self.writers.lock() {
                    writers.remove(&characteristic);
                }
                Err(RadioError(format!(
                    "bluer notify {characteristic}: {error}"
                )))
            }
        }
    }

    fn advertising_detail(&self) -> Option<serde_json::Value> {
        Some(match &self.mgmt {
            Some(mgmt) => mgmt.detail(),
            None => serde_json::json!({"backend": "bluez"}),
        })
    }

    fn advertising_unavailable(&self) -> bool {
        self.advertising_unavailable
    }
}

/// What a readable notify-session fd turned out to be. BlueZ makes the
/// `AcquireNotify` fd readable both when the central confirms an indication
/// and when the session really ends — only the second drops the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFate {
    /// An indication confirmation arrived: the session stays up.
    Confirmed,
    /// The session ended: the writer must go.
    Ended,
    /// Readiness fired with nothing to read: keep waiting.
    Spurious,
}

/// Classifies one non-blocking drain of a readable notify-session fd:
/// confirmation bytes keep the session, an empty read or a socket error ends
/// it, `WouldBlock` is a spurious wakeup. Pure so it is unit-testable without
/// a radio.
fn classify_session_drain(result: std::io::Result<usize>) -> SessionFate {
    match result {
        Ok(n) if n > 0 => SessionFate::Confirmed,
        Ok(_) => SessionFate::Ended,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => SessionFate::Spurious,
        Err(_) => SessionFate::Ended,
    }
}

/// Drains one message from a readable notify-session fd without blocking.
/// `bluer` never reads this fd itself, so a raw `recv` here races with
/// nothing; `MSG_DONTWAIT` keeps a spurious wakeup from ever blocking the
/// session task.
fn drain_session_fd(writer: &CharacteristicWriter) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; 64];
    let ret = unsafe {
        libc::recv(
            writer.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

/// `uname -r`, or why it could not be read — for failure diagnoses.
fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_string())
        .unwrap_or_else(|error| format!("unknown (/proc/sys/kernel/osrelease: {error})"))
}

impl BluerRadio {
    /// Switches advertising to the sim's own MGMT instance
    /// (`--linux-advertising mgmt-legacy`) on this adapter's controller.
    /// Fails, naming the `setcap` command, without `CAP_NET_ADMIN`; removes
    /// an instance a previous run left behind. Returns what it found.
    pub fn use_mgmt_legacy(&mut self) -> Result<serde_json::Value, RadioError> {
        let index = linux_advertising::hci_index(self.adapter.name()).map_err(RadioError)?;
        let (advertiser, found) =
            tokio::task::block_in_place(|| MgmtAdvertiser::open(index)).map_err(RadioError)?;
        self.mgmt = Some(advertiser);
        Ok(found)
    }

    /// `LEAdvertisingManager1` instance counters, read so a registration
    /// failure carries the adapter state it happened in. A counter that
    /// cannot be read is reported as such, never guessed.
    async fn advertising_instances(&self) -> String {
        let active = self.adapter.active_advertising_instances().await;
        let supported = self.adapter.supported_advertising_instances().await;
        match (active, supported) {
            (Ok(active), Ok(supported)) => {
                format!("ActiveInstances={active} SupportedInstances={supported}")
            }
            (active, supported) => {
                format!("instance counters unreadable: active {active:?}, supported {supported:?}")
            }
        }
    }

    fn setup_char_handlers(&mut self, handlers: Vec<CharNotifyHandler>) {
        for mut handler in handlers {
            let sender = self.events.clone();
            let writers = self.writers.clone();
            tokio::spawn(async move {
                while let Some(CharacteristicControlEvent::Notify(writer)) =
                    handler.control.next().await
                {
                    let writer = Arc::new(writer);
                    let service = handler.service_uuid.to_string();
                    let characteristic = handler.characteristic_uuid.to_string();
                    if sender
                        .send(RadioEvent::Subscription {
                            service: service.clone(),
                            characteristic: characteristic.clone(),
                            subscribed: true,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if let Ok(mut writers) = writers.lock() {
                        writers.insert(handler.characteristic_uuid, writer.clone());
                    }
                    // BlueZ makes the `AcquireNotify` fd readable both when
                    // the central confirms an indication and when the session
                    // really ends: drain the confirmation and keep the writer,
                    // and only drop it on a closed fd. Treating a confirmation
                    // as an unsubscribe silently drops every later indication.
                    let mut ended = false;
                    while !ended {
                        if writer.closed().await.is_err() {
                            break;
                        }
                        match classify_session_drain(drain_session_fd(&writer)) {
                            SessionFate::Confirmed => {
                                if sender
                                    .send(RadioEvent::IndicationConfirmed {
                                        service: service.clone(),
                                        characteristic: characteristic.clone(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    ended = true;
                                }
                            }
                            SessionFate::Spurious => {}
                            SessionFate::Ended => ended = true,
                        }
                    }
                    if let Ok(mut writers) = writers.lock() {
                        writers.remove(&handler.characteristic_uuid);
                    }
                    if sender
                        .send(RadioEvent::Subscription {
                            service,
                            characteristic,
                            subscribed: false,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    }
}

/// The `LEAdvertisement1` object the sim registers: a connectable
/// (`peripheral`) advertisement carrying the H10 service set, the name for the
/// scan response, General Discoverable flags, and manufacturer data only when
/// a payload is staged. Every other property stays unset so BlueZ applies its
/// defaults — no `Includes`, `Appearance`, `TxPower`, `Duration`, `Timeout`,
/// `DiscoverableTimeout`, `SecondaryChannel` or intervals. Pure construction,
/// pinned by `advertisement_properties_are_pinned`.
fn h10_advertisement(name: &str, uuids: &[Uuid], mfr: Option<&(u16, Vec<u8>)>) -> Advertisement {
    Advertisement {
        advertisement_type: AdvertisementType::Peripheral,
        service_uuids: uuids.iter().copied().collect(),
        manufacturer_data: mfr.cloned().into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(name.to_string()),
        ..Default::default()
    }
}

/// How the GATT server answers one ATT operation on a characteristic.
///
/// Mirrors the CoreBluetooth split the declarations are written for: the
/// property advertises the operation, the permission grants it. `bluer` has
/// one flag for both, so an advertised-but-not-granted operation keeps its
/// flag and is refused in the callback with `NotPermitted` — the answer a
/// CoreBluetooth peripheral gives — without consulting the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Absent,
    Granted,
    Refused,
}

fn access(
    spec: &CharSpec,
    property: CharProperty,
    permission: CharPermission,
) -> Result<Access, RadioError> {
    let advertised = spec.properties.contains(&property);
    let granted = spec.permissions.contains(&permission);
    match (advertised, granted) {
        (true, true) => Ok(Access::Granted),
        (true, false) => Ok(Access::Refused),
        (false, false) => Ok(Access::Absent),
        (false, true) => Err(RadioError(format!(
            "characteristic {} grants {permission:?} without the {property:?} property",
            spec.uuid
        ))),
    }
}

/// Write access across both write properties: a `Writeable` permission is
/// granted by `Write`, by `WriteWithoutResponse`, or by both (the FEEE
/// characteristics carry both, the vendor `6217ff4d` only
/// `WriteWithoutResponse`).
fn write_access(spec: &CharSpec) -> Result<(Access, bool, bool), RadioError> {
    let with_response = spec.properties.contains(&CharProperty::Write);
    let without_response = spec
        .properties
        .contains(&CharProperty::WriteWithoutResponse);
    let granted = spec.permissions.contains(&CharPermission::Writeable);
    let access = match (with_response || without_response, granted) {
        (true, true) => Access::Granted,
        (true, false) => Access::Refused,
        (false, false) => Access::Absent,
        (false, true) => {
            return Err(RadioError(format!(
                "characteristic {} grants Writeable without a write property \
                 (Write or WriteWithoutResponse)",
                spec.uuid
            )));
        }
    };
    Ok((access, with_response, without_response))
}

/// Checks a declared initial value against the Linux read path. BlueZ has no
/// cached-value slot: every read reaches the simulator, whose answer at start
/// is the declared initial value (pinned by
/// `live_reads_start_at_declared_initial_values`) and then tracks live state
/// (battery drain). An initial value on a characteristic nobody can read
/// could never be served, so the declaration is refused rather than dropped.
fn check_initial_value(spec: &CharSpec, read: Access) -> Result<(), RadioError> {
    match (&spec.initial_value, read) {
        (Some(_), Access::Absent | Access::Refused) => Err(RadioError(format!(
            "characteristic {} declares an initial value but is not readable \
             (needs the Read property and the Readable permission)",
            spec.uuid
        ))),
        _ => Ok(()),
    }
}

/// Answers one ATT read: refused reads never reach the simulator; granted
/// reads are forwarded to the simulator loop, whose answer is authoritative.
async fn answer_read(
    access: Access,
    sender: mpsc::Sender<RadioEvent>,
    service_uuid: Uuid,
    char_uuid: Uuid,
    offset: u16,
) -> Result<Vec<u8>, ReqError> {
    if access != Access::Granted {
        eprintln!("h10-sim: read of {char_uuid} refused: not granted Readable");
        return Err(ReqError::NotPermitted);
    }
    let (reply_tx, reply_rx) = oneshot::channel::<RadioReadAnswer>();
    let event = RadioEvent::Read {
        service: service_uuid.to_string(),
        characteristic: char_uuid.to_string(),
        offset: u64::from(offset),
        reply: reply_tx,
    };
    if sender.send(event).await.is_err() {
        eprintln!("h10-sim: simulator loop gone during read");
        return Err(ReqError::Failed);
    }
    match reply_rx.await {
        Ok(answer) if answer.ok => Ok(answer.value),
        Ok(_) => Err(ReqError::NotSupported),
        Err(_) => Err(ReqError::Failed),
    }
}

/// Builds the `bluer` GATT application from the queued service declarations.
/// Pure construction: no D-Bus traffic, so it is unit-testable without a radio.
/// An inconsistent declaration (a permission without its property, an
/// unreadable initial value) fails the whole application, loudly.
fn build_services(
    specs: Vec<ServiceSpec>,
    sender: mpsc::Sender<RadioEvent>,
) -> Result<(Vec<CharNotifyHandler>, Vec<Service>), RadioError> {
    let mut services = Vec::with_capacity(specs.len());
    let mut handlers = Vec::new();
    for spec in specs {
        let (_, service_handle) = service_control();
        let mut characteristics = Vec::with_capacity(spec.characteristics.len());
        for char_spec in spec.characteristics {
            let service_uuid = spec.uuid;
            let char_uuid = char_spec.uuid;
            let read_access = access(&char_spec, CharProperty::Read, CharPermission::Readable)?;
            let (write_access, with_response, without_response) = write_access(&char_spec)?;
            check_initial_value(&char_spec, read_access)?;
            let has_notify = char_spec.properties.contains(&CharProperty::Notify);
            let has_indicate = char_spec.properties.contains(&CharProperty::Indicate);

            let read = (read_access != Access::Absent).then(|| {
                let sender = sender.clone();
                CharacteristicRead {
                    read: true,
                    encrypt_read: false,
                    encrypt_authenticated_read: false,
                    secure_read: false,
                    fun: Box::new(move |request: CharacteristicReadRequest| {
                        answer_read(
                            read_access,
                            sender.clone(),
                            service_uuid,
                            char_uuid,
                            request.offset,
                        )
                        .boxed()
                    }),
                    ..Default::default()
                }
            });

            let write = (write_access != Access::Absent).then(|| {
                let sender = sender.clone();
                CharacteristicWrite {
                    write: with_response,
                    write_without_response: without_response,
                    reliable_write: false,
                    authenticated_signed_writes: false,
                    encrypt_write: false,
                    encrypt_authenticated_write: false,
                    secure_write: false,
                    method: CharacteristicWriteMethod::Fun(Box::new(
                        move |value: Vec<u8>, _request: CharacteristicWriteRequest| {
                            let sender = sender.clone();
                            async move {
                                if write_access == Access::Refused {
                                    eprintln!(
                                        "h10-sim: write to {char_uuid} refused: \
                                         not granted Writeable"
                                    );
                                    return Err(ReqError::NotPermitted);
                                }
                                let (reply_tx, reply_rx) = oneshot::channel::<bool>();
                                let event = RadioEvent::Write {
                                    service: service_uuid.to_string(),
                                    characteristic: char_uuid.to_string(),
                                    value,
                                    reply: reply_tx,
                                };
                                if sender.send(event).await.is_err() {
                                    eprintln!("h10-sim: simulator loop gone during write");
                                    return Err(ReqError::Failed);
                                }
                                match reply_rx.await {
                                    Ok(true) => Ok(()),
                                    Ok(false) => Err(ReqError::NotSupported),
                                    Err(_) => Err(ReqError::Failed),
                                }
                            }
                            .boxed()
                        },
                    )),
                    ..Default::default()
                }
            });

            let (notify, control_handle) = if has_notify || has_indicate {
                let (control, handle) = characteristic_control();
                handlers.push(CharNotifyHandler {
                    service_uuid,
                    characteristic_uuid: char_uuid,
                    control,
                });
                (
                    Some(CharacteristicNotify {
                        notify: has_notify,
                        indicate: has_indicate,
                        method: CharacteristicNotifyMethod::Io,
                        ..Default::default()
                    }),
                    handle,
                )
            } else {
                (None, CharacteristicControlHandle::default())
            };
            characteristics.push(Characteristic {
                uuid: char_uuid,
                read,
                write,
                notify,
                control_handle,
                ..Default::default()
            });
        }
        services.push(Service {
            uuid: spec.uuid,
            primary: true,
            characteristics,
            control_handle: service_handle,
            ..Default::default()
        });
    }
    Ok((handlers, services))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radio::h10_services;
    use crate::sim::SimConfig;
    use std::collections::BTreeMap;

    fn build_app() -> (Vec<CharNotifyHandler>, Vec<Service>) {
        let specs = h10_services(&SimConfig::default()).unwrap();
        let (sender, _receiver) = mpsc::channel::<RadioEvent>(8);
        build_services(specs, sender).expect("the H10 surface is consistent")
    }

    #[test]
    fn application_declares_full_h10_surface() {
        let (handlers, services) = build_app();
        assert_eq!(services.len(), 6, "180D, 180A, 180F, 6217ff4b, PMD, FEEE");
        let counts: Vec<usize> = services
            .iter()
            .map(|service| service.characteristics.len())
            .collect();
        assert_eq!(counts, vec![2, 7, 1, 2, 2, 3]);
        assert!(services.iter().all(|service| service.primary));
        // Every notify/indicate characteristic runs a session: HR
        // measurement, battery level, vendor write/indicate, PMD control
        // point, PMD data, FEEE 0x51 and 0x52 — one handler each.
        assert_eq!(handlers.len(), 7);
    }

    #[test]
    fn control_point_is_readable_writable_and_indicating() {
        use crate::gatt_spec;
        let (_, services) = build_app();
        let pmd_service = gatt_spec::pmd::SERVICE.parse::<Uuid>().unwrap();
        let pmd = services
            .iter()
            .find(|service| service.uuid == pmd_service)
            .expect("PMD service must be registered");
        assert_eq!(pmd.characteristics.len(), 2);
        let control_uuid = gatt_spec::pmd::CONTROL_POINT.parse::<Uuid>().unwrap();
        let control = pmd
            .characteristics
            .iter()
            .find(|characteristic| characteristic.uuid == control_uuid)
            .expect("PMD control point must be registered");
        let read = control.read.as_ref().expect("control point is readable");
        assert!(read.read);
        let write = control.write.as_ref().expect("control point is writable");
        assert!(
            write.write,
            "write request, as before — no behaviour change"
        );
        assert!(
            !write.write_without_response,
            "write command stays off, as before"
        );
        let notify = control.notify.as_ref().expect("control point indicates");
        assert!(!notify.notify);
        assert!(notify.indicate);
        let data_uuid = gatt_spec::pmd::DATA.parse::<Uuid>().unwrap();
        let data = pmd
            .characteristics
            .iter()
            .find(|characteristic| characteristic.uuid == data_uuid)
            .expect("PMD data must be registered");
        assert!(data.read.is_none() && data.write.is_none());
        let data_notify = data.notify.as_ref().expect("PMD data notifies");
        assert!(data_notify.notify && !data_notify.indicate);
    }

    fn one_characteristic(spec: CharSpec) -> ServiceSpec {
        ServiceSpec {
            uuid: crate::advertisement::short_uuid(0x180F),
            characteristics: vec![spec],
        }
    }

    #[test]
    fn advertisement_properties_are_pinned() {
        let uuids = [
            crate::advertisement::short_uuid(0x180D),
            crate::advertisement::short_uuid(0xFEEE),
        ];
        let adv = h10_advertisement("Polar H10 SIM0001", &uuids, None);
        assert_eq!(adv.advertisement_type, AdvertisementType::Peripheral);
        assert_eq!(adv.service_uuids, uuids.into_iter().collect());
        assert!(adv.manufacturer_data.is_empty(), "no payload staged");
        assert!(adv.solicit_uuids.is_empty());
        assert!(adv.service_data.is_empty());
        assert!(adv.advertising_data.is_empty());
        assert_eq!(adv.discoverable, Some(true));
        assert_eq!(adv.discoverable_timeout, None);
        assert!(
            adv.system_includes.is_empty(),
            "LocalName is set, so Includes must not repeat local-name"
        );
        assert_eq!(adv.local_name.as_deref(), Some("Polar H10 SIM0001"));
        assert_eq!(adv.appearance, None);
        assert_eq!(adv.duration, None);
        assert_eq!(adv.timeout, None);
        assert_eq!(adv.secondary_channel, None);
        assert_eq!(adv.min_interval, None);
        assert_eq!(adv.max_interval, None);
        assert_eq!(adv.tx_power, None);
    }

    #[test]
    fn staged_manufacturer_data_is_advertised() {
        let staged = (0x006B, vec![0x33, 0x1C]);
        let adv = h10_advertisement("Polar H10 SIM0001", &[], Some(&staged));
        assert_eq!(
            adv.manufacturer_data,
            BTreeMap::from([(0x006B, vec![0x33, 0x1C])])
        );
    }

    #[test]
    fn h10_surface_requires_no_encryption() {
        let (_, services) = build_app();
        for characteristic in services.iter().flat_map(|service| &service.characteristics) {
            if let Some(read) = &characteristic.read {
                assert!(read.read);
                assert!(
                    !read.encrypt_read && !read.encrypt_authenticated_read && !read.secure_read
                );
            }
            if let Some(write) = &characteristic.write {
                assert!(
                    write.write || write.write_without_response,
                    "a declared write flag must survive"
                );
                assert!(
                    !write.encrypt_write
                        && !write.encrypt_authenticated_write
                        && !write.secure_write
                        && !write.authenticated_signed_writes
                );
            }
        }
    }

    #[test]
    fn readable_characteristics_follow_their_declarations() {
        let specs = h10_services(&SimConfig::default()).unwrap();
        let (_, services) = build_app();
        for (spec, service) in specs.iter().zip(&services) {
            for (char_spec, characteristic) in
                spec.characteristics.iter().zip(&service.characteristics)
            {
                assert_eq!(char_spec.uuid, characteristic.uuid);
                assert_eq!(
                    characteristic.read.is_some(),
                    char_spec.properties.contains(&CharProperty::Read),
                    "read flag of {}",
                    char_spec.uuid
                );
                assert_eq!(
                    characteristic.write.is_some(),
                    char_spec.properties.contains(&CharProperty::Write)
                        || char_spec
                            .properties
                            .contains(&CharProperty::WriteWithoutResponse),
                    "write flag of {}",
                    char_spec.uuid
                );
                if let Some(write) = &characteristic.write {
                    assert_eq!(
                        write.write,
                        char_spec.properties.contains(&CharProperty::Write),
                        "with-response flag of {}",
                        char_spec.uuid
                    );
                    assert_eq!(
                        write.write_without_response,
                        char_spec
                            .properties
                            .contains(&CharProperty::WriteWithoutResponse),
                        "without-response flag of {}",
                        char_spec.uuid
                    );
                }
            }
        }
    }

    #[test]
    fn live_reads_start_at_declared_initial_values() {
        let config = SimConfig::default();
        let sim = crate::sim::SimState::new(config.clone());
        let mut declared = 0;
        for spec in h10_services(&config).unwrap() {
            for characteristic in spec.characteristics {
                let Some(initial) = characteristic.initial_value else {
                    continue;
                };
                declared += 1;
                let answer = crate::read_answer(&sim, &characteristic.uuid, 0);
                assert!(answer.ok, "{} must be readable", characteristic.uuid);
                assert_eq!(
                    answer.value, initial,
                    "first read of {}",
                    characteristic.uuid
                );
            }
        }
        assert_eq!(
            declared, 11,
            "BSL, battery, 7 DIS, PMD control point, vendor read"
        );
    }

    #[tokio::test]
    async fn read_without_readable_permission_is_refused_before_the_sim() {
        let spec = one_characteristic(CharSpec {
            uuid: crate::advertisement::short_uuid(0x2A19),
            properties: vec![CharProperty::Read],
            permissions: Vec::new(),
            initial_value: None,
        });
        let (sender, mut receiver) = mpsc::channel::<RadioEvent>(8);
        let service_uuid = spec.uuid;
        let char_uuid = spec.characteristics[0].uuid;
        let (_, services) = build_services(vec![spec], sender.clone()).unwrap();
        assert!(
            services[0].characteristics[0].read.is_some(),
            "the Read property keeps the flag"
        );
        let answer = answer_read(Access::Refused, sender, service_uuid, char_uuid, 0).await;
        assert!(matches!(answer, Err(ReqError::NotPermitted)));
        assert!(receiver.try_recv().is_err(), "the sim is never consulted");
    }

    #[test]
    fn permission_without_property_is_refused() {
        let spec = one_characteristic(CharSpec {
            uuid: crate::advertisement::short_uuid(0x2A19),
            properties: vec![CharProperty::Notify],
            permissions: vec![CharPermission::Writeable],
            initial_value: None,
        });
        let (sender, _receiver) = mpsc::channel::<RadioEvent>(8);
        let error = build_services(vec![spec], sender).err().expect("refused");
        assert!(
            error.0.contains("Writeable without a write property"),
            "{}",
            error.0
        );
    }

    #[test]
    fn write_without_response_only_advertises_no_response_write() {
        use crate::gatt_spec;
        let (_, services) = build_app();
        let find = |uuid: &str| {
            let target = uuid.parse::<Uuid>().unwrap();
            services
                .iter()
                .flat_map(|service| &service.characteristics)
                .find(|characteristic| characteristic.uuid == target)
                .unwrap_or_else(|| panic!("{uuid} must be registered"))
        };
        // The vendor characteristic is indicate + write-command only: no
        // write-with-response flag, so discovery matches the strap.
        let vendor_write = find(gatt_spec::vendor::WRITE_INDICATE)
            .write
            .as_ref()
            .expect("6217ff4d is writable");
        assert!(!vendor_write.write);
        assert!(vendor_write.write_without_response);
        // The PMD control point keeps write-with-response and no command flag.
        let control = find(gatt_spec::pmd::CONTROL_POINT)
            .write
            .as_ref()
            .expect("control point is writable");
        assert!(control.write);
        assert!(!control.write_without_response);
        // FEEE 0x51 carries both write flags plus notify.
        let char51 = find(gatt_spec::feee::CHAR_51);
        let write51 = char51.write.as_ref().expect("0x51 is writable");
        assert!(write51.write && write51.write_without_response);
        assert!(char51.notify.as_ref().expect("0x51 notifies").notify);
    }

    #[test]
    fn session_drain_keeps_confirmed_indications_and_drops_closed() {
        assert_eq!(
            classify_session_drain(Ok(1)),
            SessionFate::Confirmed,
            "BlueZ's per-indication confirmation byte keeps the session"
        );
        assert_eq!(
            classify_session_drain(Ok(0)),
            SessionFate::Ended,
            "an empty read means the peer closed the session"
        );
        assert_eq!(
            classify_session_drain(Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))),
            SessionFate::Spurious,
            "a spurious wakeup keeps waiting"
        );
        assert_eq!(
            classify_session_drain(Err(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset
            ))),
            SessionFate::Ended,
            "a dead socket ends the session"
        );
    }

    #[test]
    fn unreadable_initial_value_is_refused() {
        let spec = one_characteristic(CharSpec {
            uuid: crate::advertisement::short_uuid(0x2A19),
            properties: vec![CharProperty::Read],
            permissions: Vec::new(),
            initial_value: Some(vec![85]),
        });
        let (sender, _receiver) = mpsc::channel::<RadioEvent>(8);
        let error = build_services(vec![spec], sender).err().expect("refused");
        assert!(
            error
                .0
                .contains("declares an initial value but is not readable"),
            "{}",
            error.0
        );
    }
}
