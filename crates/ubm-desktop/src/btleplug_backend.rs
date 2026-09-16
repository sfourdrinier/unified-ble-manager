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
//! `PARITY_GAPS.md`).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex as StdMutex;

use btleplug::api::{
    Central as _, CentralEvent, CharPropFlags, Characteristic, Descriptor, Manager as _,
    Peripheral as _, ScanFilter, Service, ValueNotification,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};

use crate::boundary::{
    CharacteristicSnapshot, DescriptorSnapshot, PeerSnapshot, PropertyFlags, RadioBoundary,
    RadioEvent, ScanFilterSpec, ServiceSnapshot,
};
use crate::errors::DesktopError;
use ubm_core::central::{GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE};

type EventStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = CentralEvent> + Send>>;
type NotificationStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = ValueNotification> + Send>>;

/// One live notification forwarder: the task fanning one characteristic
/// instance into the shared event channel, plus the instance address for
/// best-effort OS unsubscribe at teardown.
struct ForwarderEntry {
    task: tokio::task::JoinHandle<()>,
    peer_id: String,
    service_uuid: String,
    service_occurrence: u64,
    characteristic_uuid: String,
    characteristic_occurrence: u64,
}

/// Production radio backend over one btleplug adapter.
pub struct BtleplugRadio {
    adapter: Adapter,
    adapter_label: String,
    spawn: tokio::runtime::Handle,
    events: Mutex<Option<EventStream>>,
    notifications: mpsc::UnboundedSender<RadioEvent>,
    notification_rx: Mutex<mpsc::UnboundedReceiver<RadioEvent>>,
    forwarders: StdMutex<HashMap<String, ForwarderEntry>>,
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
        let manager = Manager::new().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.open").with_detail(error.to_string())
        })?;
        let adapters = manager.adapters().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.enumerate").with_detail(error.to_string())
        })?;
        let mut chosen: Option<(Adapter, String)> = None;
        for adapter in adapters {
            // Never synthesize an adapter identity: when the OS withholds
            // the info, the adapter is skipped, not labelled "unknown".
            let info = match adapter.adapter_info().await {
                Ok(info) => info,
                Err(_) => continue,
            };
            let wanted = adapter_id.as_deref().unwrap_or(info.as_str());
            if info == wanted {
                chosen = Some((adapter, info));
                break;
            }
        }
        let Some((adapter, adapter_label)) = chosen else {
            return Err(DesktopError::adapter_unavailable("adapter.select")
                .with_detail("no matching btleplug adapter"));
        };
        let events = adapter.events().await.map_err(|error| {
            DesktopError::adapter_unavailable("adapter.events").with_detail(error.to_string())
        })?;
        let (notifications, notification_rx) = mpsc::unbounded_channel();
        Ok(Self {
            adapter,
            adapter_label,
            spawn,
            events: Mutex::new(Some(events)),
            notifications,
            notification_rx: Mutex::new(notification_rx),
            forwarders: StdMutex::new(HashMap::new()),
        })
    }

    async fn peripheral_by_id(&self, peer_id: &str) -> Result<Peripheral, DesktopError> {
        for peripheral in self.adapter.peripherals().await.map_err(map_radio(
            "peer.lookup",
            ubm_core::contracts::BleErrorCode::PeerNotFound,
            ubm_core::contracts::BleErrorDomain::Connection,
        ))? {
            if peripheral.id().to_string() == peer_id {
                return Ok(peripheral);
            }
        }
        Err(DesktopError::new(
            ubm_core::contracts::BleErrorCode::PeerNotFound,
            ubm_core::contracts::BleErrorDomain::Connection,
            "peer.lookup",
        )
        .with_detail(peer_id.to_owned()))
    }

    async fn snapshot(&self, peripheral: &Peripheral) -> PeerSnapshot {
        let properties = peripheral.properties().await.ok().flatten();
        let (service_uuids, rssi) = properties
            .map(|facts| {
                (
                    facts
                        .services
                        .into_iter()
                        .map(|uuid| uuid.to_string())
                        .collect(),
                    facts.rssi,
                )
            })
            .unwrap_or_default();
        // btleplug exposes the address type opaquely per platform; report the
        // string, never guess public-vs-random (see PARITY_GAPS.md).
        let address = {
            let raw = peripheral.address().to_string();
            if raw.is_empty() || raw == "00:00:00:00:00:00" {
                None
            } else {
                Some(raw)
            }
        };
        PeerSnapshot {
            id: peripheral.id().to_string(),
            address,
            service_uuids,
            rssi,
        }
    }

    async fn advertisement_for(&self, id: &PeripheralId) -> Option<RadioEvent> {
        let wanted = id.to_string();
        for peripheral in self.adapter.peripherals().await.ok()? {
            if peripheral.id().to_string() == wanted {
                return Some(RadioEvent::Advertisement(self.snapshot(&peripheral).await));
            }
        }
        None
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
        queue.recv().await
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

/// Per-instance forwarder key: duplicate UUIDs never share a forwarder.
fn forwarder_key(
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
    move |error| DesktopError::new(code, domain, operation).with_detail(error.to_string())
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

fn property_bits(flags: PropertyFlags) -> u8 {
    let mut bits = 0u8;
    if flags.read {
        bits |= GATT_PROP_READ;
    }
    if flags.write {
        bits |= GATT_PROP_WRITE;
    }
    if flags.write_without_response {
        // Core models write-command readiness through the write-mode path;
        // the bit survives here so discovery snapshots stay lossless.
        bits |= GATT_PROP_WRITE;
    }
    if flags.notify {
        bits |= GATT_PROP_NOTIFY;
    }
    if flags.indicate {
        bits |= GATT_PROP_INDICATE;
    }
    bits
}

impl RadioBoundary for BtleplugRadio {
    async fn adapter_name(&self) -> Result<String, DesktopError> {
        Ok(self.adapter_label.clone())
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
            .start_scan(ScanFilter { services })
            .await
            .map_err(|error| DesktopError::scan_start_failed(error.to_string()))
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        self.adapter
            .stop_scan()
            .await
            .map_err(|error| DesktopError::scan_stop_failed(error.to_string()))
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        let mut out = Vec::new();
        let peripherals = self.adapter.peripherals().await.map_err(map_radio(
            "peer.list",
            ubm_core::contracts::BleErrorCode::AdapterUnavailable,
            ubm_core::contracts::BleErrorDomain::Adapter,
        ))?;
        for peripheral in &peripherals {
            out.push(self.snapshot(peripheral).await);
        }
        Ok(out)
    }

    async fn connect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        peripheral
            .connect()
            .await
            .map_err(|error| DesktopError::connection_failed(error.to_string()))
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        // btleplug maps an already-released peripheral to success-or-error
        // per platform; treat "already gone" as released (core treats
        // `is_connected() == false` as released too).
        if !peripheral.is_connected().await.unwrap_or(true) {
            return Ok(());
        }
        peripheral.disconnect().await.map_err(|error| {
            DesktopError::new(
                ubm_core::contracts::BleErrorCode::ConnectionLost,
                ubm_core::contracts::BleErrorDomain::Connection,
                "connection.disconnect",
            )
            .with_detail(error.to_string())
        })
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        peripheral.discover_services().await.map_err(map_radio(
            "discovery.complete",
            ubm_core::contracts::BleErrorCode::GattDiscoveryRequired,
            ubm_core::contracts::BleErrorDomain::Gatt,
        ))?;
        // The cached service set already iterates in canonical UUID-first
        // order; occurrences count per UUID in that order so instance
        // numbers agree with the occurrence-aware lookup path (H1).
        let services = peripheral.services();
        let mut out = Vec::with_capacity(services.len());
        let mut service_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
        for service in &services {
            let service_occurrence = service_counts.entry(service.uuid).or_insert(0);
            let service_occ = *service_occurrence;
            *service_occurrence += 1;
            let mut chars_out = Vec::with_capacity(service.characteristics.len());
            let mut char_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
            for characteristic in &service.characteristics {
                let char_occurrence = char_counts.entry(characteristic.uuid).or_insert(0);
                let char_occ = *char_occurrence;
                *char_occurrence += 1;
                let mut desc_counts: HashMap<uuid::Uuid, u64> = HashMap::new();
                let mut descs_out = Vec::with_capacity(characteristic.descriptors.len());
                for descriptor in &characteristic.descriptors {
                    let desc_occurrence = desc_counts.entry(descriptor.uuid).or_insert(0);
                    let desc_occ = *desc_occurrence;
                    *desc_occurrence += 1;
                    descs_out.push(DescriptorSnapshot {
                        uuid: descriptor.uuid.to_string(),
                        occurrence: desc_occ,
                    });
                }
                chars_out.push(CharacteristicSnapshot {
                    uuid: characteristic.uuid.to_string(),
                    occurrence: char_occ,
                    properties: property_flags(characteristic.properties),
                    descriptors: descs_out,
                });
            }
            out.push(ServiceSnapshot {
                uuid: service.uuid.to_string(),
                occurrence: service_occ,
                characteristics: chars_out,
            });
        }
        Ok(out)
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
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
        peripheral
            .read(&characteristic)
            .await
            .map_err(|error| DesktopError::read_failed(error.to_string()))
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
        let peripheral = self.peripheral_by_id(peer_id).await?;
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
            .map_err(|error| DesktopError::write_failed(error.to_string()))
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
        let peripheral = self.peripheral_by_id(peer_id).await?;
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
            .map_err(|error| DesktopError::read_failed(error.to_string()))
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
        let peripheral = self.peripheral_by_id(peer_id).await?;
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
            .map_err(|error| DesktopError::write_failed(error.to_string()))
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        // The peripheral lookup already failed closed upstream for unknown
        // peers; here an unknown peer is simply unmeasured, and the write
        // path fails closed as `capability.unavailable`.
        self.peripheral_by_id(peer_id).await.ok().map(|p| p.mtu())
    }

    async fn set_notifications(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
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
        let key = forwarder_key(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
        );
        if enable {
            peripheral
                .subscribe(&characteristic)
                .await
                .map_err(|error| DesktopError::subscribe_failed(error.to_string()))?;
            let stream: NotificationStream = peripheral
                .notifications()
                .await
                .map_err(|error| DesktopError::subscribe_failed(error.to_string()))?;
            let sender = self.notifications.clone();
            let peer = peer_id.to_owned();
            let service = service_uuid.to_owned();
            let instance_characteristic = characteristic_uuid.to_owned();
            // The btleplug stream is peripheral-wide: filter to this
            // instance's UUID so one subscription never routes another
            // UUID's values. Same-UUID duplicate instances stay
            // indistinguishable on this stream (no handles exposed) and
            // fan out to every same-UUID forwarder; see PARITY_GAPS.md.
            let own_uuid = characteristic.uuid;
            let forwarder = self.spawn.spawn(async move {
                let mut stream = stream;
                while let Some(note) = stream.next().await {
                    if note.uuid != own_uuid {
                        continue;
                    }
                    let event = RadioEvent::Notification {
                        peer_id: peer.clone(),
                        service_uuid: service.clone(),
                        service_occurrence,
                        characteristic_uuid: instance_characteristic.clone(),
                        characteristic_occurrence,
                        value: note.value,
                    };
                    // The CCCD stays enabled on send failure; the event loop
                    // being gone is a host-teardown fact, not a radio error.
                    if sender.send(event).is_err() {
                        break;
                    }
                }
            });
            // Defensive replace: a live entry under the same per-instance
            // key is aborted before overwrite so no forwarder ever leaks.
            let replaced = self.forwarders.lock().expect("forwarder table").insert(
                key,
                ForwarderEntry {
                    task: forwarder,
                    peer_id: peer_id.to_owned(),
                    service_uuid: service_uuid.to_owned(),
                    service_occurrence,
                    characteristic_uuid: characteristic_uuid.to_owned(),
                    characteristic_occurrence,
                },
            );
            if let Some(stale) = replaced {
                stale.task.abort();
            }
        } else {
            if let Some(entry) = self
                .forwarders
                .lock()
                .expect("forwarder table")
                .remove(&key)
            {
                entry.task.abort();
            }
            peripheral
                .unsubscribe(&characteristic)
                .await
                .map_err(|error| DesktopError::subscribe_failed(error.to_string()))?;
        }
        Ok(())
    }

    /// Teardown hook (M3): abort every live forwarder and best-effort
    /// release every OS-side CCCD. Failures are swallowed: teardown
    /// reports facts, never new failures.
    async fn close(&self) {
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
        for entry in &entries {
            let peripheral = match self.peripheral_by_id(&entry.peer_id).await {
                Ok(peripheral) => peripheral,
                Err(_) => continue,
            };
            let characteristic = match Self::find_characteristic(
                &peripheral,
                &entry.service_uuid,
                entry.service_occurrence,
                &entry.characteristic_uuid,
                entry.characteristic_occurrence,
            ) {
                Some(characteristic) => characteristic,
                None => continue,
            };
            let _ = peripheral.unsubscribe(&characteristic).await;
        }
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        enum Step {
            Notification(Option<RadioEvent>),
            Adapter(Option<CentralEvent>),
        }
        loop {
            // Both guards drop at the end of the block, before any
            // snapshot lookup awaits, so a slow OS lookup never wedges
            // the event source.
            let step = {
                let mut events = self.events.lock().await;
                let stream = events.as_mut()?;
                // Deliberately unbiased: a notification flood must never
                // starve adapter events (a delayed DeviceDisconnected is
                // a stale link, not a slow one).
                tokio::select! {
                    notified = self.recv_notification() => Step::Notification(notified),
                    event = stream.next() => Step::Adapter(event),
                }
            };
            match step {
                Step::Notification(notified) => return notified,
                Step::Adapter(None) => return None,
                Step::Adapter(Some(CentralEvent::DeviceDiscovered(id)))
                | Step::Adapter(Some(CentralEvent::DeviceUpdated(id))) => {
                    if let Some(event) = self.advertisement_for(&id).await {
                        return Some(event);
                    }
                }
                // A changed GATT database invalidates discovered paths:
                // surface it as its own event so the central invalidates
                // generations instead of re-reading stale handles. The
                // next discovery refreshes the snapshot.
                Step::Adapter(Some(CentralEvent::DeviceServicesModified(id))) => {
                    return Some(RadioEvent::ServicesChanged(id.to_string()));
                }
                Step::Adapter(Some(CentralEvent::DeviceConnected(id))) => {
                    return Some(RadioEvent::Connected(id.to_string()));
                }
                Step::Adapter(Some(CentralEvent::DeviceDisconnected(id))) => {
                    return Some(RadioEvent::Disconnected(id.to_string()));
                }
                Step::Adapter(Some(_)) => {}
            }
        }
    }
}

/// Translate one discovered service snapshot into core path registrations is
/// owned by [`crate::DesktopCentral`]; this helper exposes the property-bit
/// mapping for it.
#[must_use]
pub fn core_property_bits(flags: PropertyFlags) -> u8 {
    property_bits(flags)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use btleplug::api::{CharPropFlags, Characteristic, Descriptor, Service};

    use super::{
        core_property_bits, forwarder_key, select_characteristic, select_descriptor, select_service,
    };
    use crate::boundary::PropertyFlags;
    use ubm_core::central::{
        GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE,
    };

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).expect("fixture uuid")
    }

    fn characteristic(service: &str, char: &str, flags: CharPropFlags) -> Characteristic {
        Characteristic {
            uuid: uuid(char),
            service_uuid: uuid(service),
            properties: flags,
            descriptors: BTreeSet::new(),
        }
    }

    fn service_with(uuid_text: &str, chars: Vec<Characteristic>) -> Service {
        Service {
            uuid: uuid(uuid_text),
            primary: true,
            characteristics: chars.into_iter().collect(),
        }
    }

    #[test]
    fn occurrence_selects_among_duplicate_characteristics() {
        let service = service_with(
            HRM_SERVICE,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::READ),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::NOTIFY),
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
    fn occurrence_counts_per_uuid_not_flat_index() {
        // Flat indexing would number BATTERY_LEVEL as 2; per-UUID
        // counting (matching discover()) numbers it 0.
        let service = service_with(
            HRM_SERVICE,
            vec![
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::READ),
                characteristic(HRM_SERVICE, HRM_MEASUREMENT, CharPropFlags::NOTIFY),
                characteristic(HRM_SERVICE, BATTERY_LEVEL, CharPropFlags::READ),
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
                vec![characteristic(
                    HRM_SERVICE,
                    HRM_MEASUREMENT,
                    CharPropFlags::READ,
                )],
            ),
            service_with(
                HRM_SERVICE,
                vec![characteristic(
                    HRM_SERVICE,
                    HRM_MEASUREMENT,
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
                service_uuid: uuid(HRM_SERVICE),
                characteristic_uuid: uuid(HRM_MEASUREMENT),
            });
            Characteristic {
                uuid: uuid(HRM_MEASUREMENT),
                service_uuid: uuid(HRM_SERVICE),
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
}
