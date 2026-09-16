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

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use btleplug::api::{
    Central as _, CentralEvent, CharPropFlags, Manager as _, Peripheral as _, ScanFilter,
    ValueNotification,
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

/// Production radio backend over one btleplug adapter.
pub struct BtleplugRadio {
    adapter: Adapter,
    adapter_label: String,
    spawn: tokio::runtime::Handle,
    events: Mutex<Option<EventStream>>,
    notifications: mpsc::UnboundedSender<RadioEvent>,
    notification_rx: Mutex<mpsc::UnboundedReceiver<RadioEvent>>,
    forwarders: StdMutex<HashMap<String, tokio::task::JoinHandle<()>>>,
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
            let info = adapter
                .adapter_info()
                .await
                .unwrap_or_else(|_| "unknown".to_owned());
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

    fn find_characteristic(
        peripheral: &Peripheral,
        characteristic_uuid: &str,
    ) -> Option<btleplug::api::Characteristic> {
        peripheral
            .characteristics()
            .into_iter()
            .find(|known| known.uuid.to_string() == characteristic_uuid)
    }

    async fn recv_notification(&self) -> Option<RadioEvent> {
        let mut queue = self.notification_rx.lock().await;
        queue.recv().await
    }

    fn find_descriptor(
        peripheral: &Peripheral,
        characteristic_uuid: &str,
        descriptor_uuid: &str,
    ) -> Option<btleplug::api::Descriptor> {
        Self::find_characteristic(peripheral, characteristic_uuid).and_then(|characteristic| {
            characteristic
                .descriptors
                .into_iter()
                .find(|known| known.uuid.to_string() == descriptor_uuid)
        })
    }
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
        let mut services: Vec<_> = peripheral.services().into_iter().collect();
        services.sort_by_key(|service| service.uuid);
        let mut out = Vec::with_capacity(services.len());
        for (service_occurrence, service) in services.iter().enumerate() {
            let mut characteristics: Vec<_> = service.characteristics.iter().collect();
            characteristics.sort_by_key(|characteristic| characteristic.uuid);
            let mut chars_out = Vec::with_capacity(characteristics.len());
            for (occurrence, characteristic) in characteristics.iter().enumerate() {
                let mut descriptors: Vec<_> = characteristic.descriptors.iter().collect();
                descriptors.sort_by_key(|descriptor| descriptor.uuid);
                chars_out.push(CharacteristicSnapshot {
                    uuid: characteristic.uuid.to_string(),
                    occurrence: occurrence as u64,
                    properties: property_flags(characteristic.properties),
                    descriptors: descriptors
                        .iter()
                        .map(|descriptor| DescriptorSnapshot {
                            uuid: descriptor.uuid.to_string(),
                        })
                        .collect(),
                });
            }
            out.push(ServiceSnapshot {
                uuid: service.uuid.to_string(),
                occurrence: service_occurrence as u64,
                characteristics: chars_out,
            });
        }
        Ok(out)
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        characteristic_uuid: &str,
    ) -> Result<Vec<u8>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let characteristic = Self::find_characteristic(&peripheral, characteristic_uuid)
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

    async fn write_characteristic(
        &self,
        peer_id: &str,
        characteristic_uuid: &str,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let characteristic = Self::find_characteristic(&peripheral, characteristic_uuid)
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

    async fn read_descriptor(
        &self,
        peer_id: &str,
        characteristic_uuid: &str,
        descriptor_uuid: &str,
    ) -> Result<Vec<u8>, DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let descriptor = Self::find_descriptor(&peripheral, characteristic_uuid, descriptor_uuid)
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

    async fn write_descriptor(
        &self,
        peer_id: &str,
        characteristic_uuid: &str,
        descriptor_uuid: &str,
        value: Vec<u8>,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let descriptor = Self::find_descriptor(&peripheral, characteristic_uuid, descriptor_uuid)
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
        characteristic_uuid: &str,
        enable: bool,
    ) -> Result<(), DesktopError> {
        let peripheral = self.peripheral_by_id(peer_id).await?;
        let characteristic = Self::find_characteristic(&peripheral, characteristic_uuid)
            .ok_or_else(|| {
                DesktopError::subscribe_failed(format!(
                    "unknown characteristic {characteristic_uuid}"
                ))
            })?;
        let key = format!("{peer_id}#{characteristic_uuid}");
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
            let forwarder = self.spawn.spawn(async move {
                let mut stream = stream;
                while let Some(note) = stream.next().await {
                    let event = RadioEvent::Notification {
                        peer_id: peer.clone(),
                        characteristic_uuid: note.uuid.to_string(),
                        value: note.value,
                    };
                    // The CCCD stays enabled on send failure; the event loop
                    // being gone is a host-teardown fact, not a radio error.
                    if sender.send(event).is_err() {
                        break;
                    }
                }
            });
            self.forwarders
                .lock()
                .expect("forwarder table")
                .insert(key, forwarder);
        } else {
            if let Some(worker) = self
                .forwarders
                .lock()
                .expect("forwarder table")
                .remove(&key)
            {
                worker.abort();
            }
            peripheral
                .unsubscribe(&characteristic)
                .await
                .map_err(|error| DesktopError::subscribe_failed(error.to_string()))?;
        }
        Ok(())
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
                tokio::select! {
                    biased;
                    notified = self.recv_notification() => Step::Notification(notified),
                    event = stream.next() => Step::Adapter(event),
                }
            };
            match step {
                Step::Notification(notified) => return notified,
                Step::Adapter(None) => return None,
                Step::Adapter(Some(CentralEvent::DeviceDiscovered(id)))
                | Step::Adapter(Some(CentralEvent::DeviceUpdated(id)))
                | Step::Adapter(Some(CentralEvent::DeviceServicesModified(id))) => {
                    if let Some(event) = self.advertisement_for(&id).await {
                        return Some(event);
                    }
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
    use super::core_property_bits;
    use crate::boundary::PropertyFlags;
    use ubm_core::central::{
        GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE,
    };

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
