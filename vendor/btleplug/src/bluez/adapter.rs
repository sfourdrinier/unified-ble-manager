use super::peripheral::{Peripheral, PeripheralId};
use crate::api::{Central, CentralEvent, CentralState, ScanFilter};
use crate::{Error, Result};
use async_trait::async_trait;
use bluez_async::{
    AdapterEvent, AdapterId, BluetoothError, BluetoothEvent, BluetoothSession, DeviceEvent,
    DiscoveryFilter, Transport,
};
use futures::stream::{self, Stream, StreamExt};
use std::pin::Pin;

/// Implementation of [api::Central](crate::api::Central).
#[derive(Clone, Debug)]
pub struct Adapter {
    session: BluetoothSession,
    adapter: AdapterId,
}

impl Adapter {
    pub(crate) fn new(session: BluetoothSession, adapter: AdapterId) -> Self {
        Self { session, adapter }
    }
}

fn get_central_state(powered: bool) -> CentralState {
    match powered {
        true => CentralState::PoweredOn,
        false => CentralState::PoweredOff,
    }
}

#[async_trait]
impl Central for Adapter {
    type Peripheral = Peripheral;

    async fn events(&self) -> Result<Pin<Box<dyn Stream<Item = CentralEvent> + Send>>> {
        // There's a race between getting this event stream and getting the current set of devices.
        // Get the stream first, on the basis that it's better to have a duplicate DeviceDiscovered
        // event than to miss one. It's unlikely to happen in any case.
        let events = self.session.adapter_event_stream(&self.adapter).await?;

        // Synthesise `DeviceDiscovered' and `DeviceConnected` events for existing peripherals.
        let devices = self.session.get_devices().await?;
        let adapter_id = self.adapter.clone();
        let initial_events = stream::iter(
            devices
                .into_iter()
                .filter(move |device| device.id.adapter() == adapter_id)
                .flat_map(|device| {
                    let peripheral_id: PeripheralId = device.id.into();
                    let mut events = vec![CentralEvent::DeviceDiscovered(peripheral_id.clone())];
                    if !device.services.is_empty() {
                        events.push(CentralEvent::ServicesAdvertisement {
                            id: peripheral_id.clone(),
                            services: device.services,
                        });
                    }
                    if device.connected {
                        events.push(CentralEvent::DeviceConnected(peripheral_id));
                    }
                    events.into_iter()
                }),
        );

        let session = self.session.clone();
        let adapter_id = self.adapter.clone();
        let events = events
            .filter_map(move |event| central_events(event, session.clone(), adapter_id.clone()))
            .flat_map(stream::iter);

        Ok(Box::pin(initial_events.chain(events)))
    }

    async fn start_scan(&self, filter: ScanFilter) -> Result<()> {
        let filter = discovery_filter(filter);
        self.session
            .start_discovery_on_adapter_with_filter(&self.adapter, &filter)
            .await?;
        Ok(())
    }

    async fn stop_scan(&self) -> Result<()> {
        self.session
            .stop_discovery_on_adapter(&self.adapter)
            .await?;
        Ok(())
    }

    async fn peripherals(&self) -> Result<Vec<Peripheral>> {
        let devices = self.session.get_devices_on_adapter(&self.adapter).await?;
        Ok(devices
            .into_iter()
            .map(|device| Peripheral::new(self.session.clone(), device))
            .collect())
    }

    async fn peripheral(&self, id: &PeripheralId) -> Result<Peripheral> {
        let device = self.session.get_device_info(&id.0).await.map_err(|e| {
            if let BluetoothError::DbusError(_) = e {
                Error::DeviceNotFound
            } else {
                e.into()
            }
        })?;
        Ok(Peripheral::new(self.session.clone(), device))
    }

    async fn add_peripheral(&self, _address: &PeripheralId) -> Result<Peripheral> {
        Err(Error::NotSupported(
            "Can't add a Peripheral from a PeripheralId".to_string(),
        ))
    }

    async fn clear_peripherals(&self) -> Result<()> {
        // BlueZ queries the daemon live; peripherals aren't cached locally.
        Ok(())
    }

    async fn adapter_info(&self) -> Result<String> {
        let adapter_info = self.session.get_adapter_info(&self.adapter).await?;
        Ok(format!("{} ({})", adapter_info.id, adapter_info.modalias))
    }

    async fn adapter_state(&self) -> Result<CentralState> {
        let mut powered = false;
        if let Ok(info) = self.session.get_adapter_info(&self.adapter).await {
            powered = info.powered;
        }
        Ok(get_central_state(powered))
    }
}

impl From<BluetoothError> for Error {
    /// UBM patch (UBM_PATCHES.md #15): a D-Bus failure keeps its error name
    /// as the platform's answer; other failures stay local errors.
    fn from(error: BluetoothError) -> Self {
        match error {
            BluetoothError::DbusError(dbus) => Error::Platform(crate::PlatformError::bluez_dbus(
                dbus.name(),
                dbus.message(),
            )),
            other => Error::Other(Box::new(other)),
        }
    }
}

/// UBM patch (UBM_PATCHES.md #17): BlueZ reports no single advertisement,
/// only the `Device1` properties it merged, so a sighting is that state,
/// labelled as such. The legacy BlueZ backend read the same properties.
fn device_state_report(device: &bluez_async::DeviceInfo) -> crate::api::AdvertisementReport {
    DeviceState {
        name: device.name.clone(),
        rssi: device.rssi,
        tx_power: device.tx_power,
        manufacturer_data: device.manufacturer_data.clone(),
        service_data: device.service_data.clone(),
        services: device.services.clone(),
    }
    .report()
}

/// The `Device1` facts a sighting carries.
struct DeviceState {
    name: Option<String>,
    rssi: Option<i16>,
    tx_power: Option<i16>,
    manufacturer_data: std::collections::HashMap<u16, Vec<u8>>,
    service_data: std::collections::HashMap<uuid::Uuid, Vec<u8>>,
    services: Vec<uuid::Uuid>,
}

impl DeviceState {
    fn report(self) -> crate::api::AdvertisementReport {
        crate::api::AdvertisementReport {
            source: crate::api::ReportSource::DeviceState,
            local_name: self.name,
            rssi: self.rssi,
            tx_power_level: self.tx_power,
            manufacturer_data: self.manufacturer_data,
            service_data: self.service_data,
            services: self.services,
            solicited_services: None,
            overflow_services: None,
            connectable: None,
        }
    }
}

/// UBM patch (UBM_PATCHES.md #17): the sighting a device event is, as the
/// legacy BlueZ backend reported one on discovery and on every
/// advertisement-bearing property change; an unreadable device is reported,
/// never dropped.
async fn sighting(session: &BluetoothSession, id: &bluez_async::DeviceId) -> CentralEvent {
    match session.get_device_info(id).await {
        Ok(device) => CentralEvent::Advertisement {
            id: device.id.clone().into(),
            report: device_state_report(&device),
        },
        Err(error) => CentralEvent::AdvertisementUnread {
            id: id.clone().into(),
            detail: error.to_string(),
        },
    }
}

async fn central_events(
    event: BluetoothEvent,
    session: BluetoothSession,
    adapter_id: AdapterId,
) -> Option<Vec<CentralEvent>> {
    // UBM patch (UBM_PATCHES.md #17): discovery and every
    // advertisement-bearing property change is also a sighting.
    if let BluetoothEvent::Device { id, event } = &event {
        if id.adapter() == adapter_id && reports_sighting(event) {
            let report = sighting(&session, id).await;
            let mut events = upstream_central_events(event.clone(), &session, id, adapter_id)
                .await
                .unwrap_or_default();
            events.push(report);
            return Some(events);
        }
    }
    upstream_events(event, session, adapter_id).await
}

/// UBM patches #17/#18: the BlueZ events that are a sighting: discovery,
/// and each `Device1` `PropertiesChanged` signal (vendored bluez-async's
/// one summary event per signal, so a signal that changes several
/// properties is one sighting, and a name-only change is one too), as the
/// legacy BlueZ backend reported the device on every property change.
fn reports_sighting(event: &DeviceEvent) -> bool {
    matches!(
        event,
        DeviceEvent::Discovered | DeviceEvent::PropertiesChanged { .. }
    )
}

async fn upstream_central_events(
    event: DeviceEvent,
    session: &BluetoothSession,
    id: &bluez_async::DeviceId,
    adapter_id: AdapterId,
) -> Option<Vec<CentralEvent>> {
    upstream_events(
        BluetoothEvent::Device {
            id: id.clone(),
            event,
        },
        session.clone(),
        adapter_id,
    )
    .await
}

async fn upstream_events(
    event: BluetoothEvent,
    session: BluetoothSession,
    adapter_id: AdapterId,
) -> Option<Vec<CentralEvent>> {
    match event {
        BluetoothEvent::Device {
            id,
            event: device_event,
        } if id.adapter() == adapter_id => match device_event {
            DeviceEvent::Discovered => {
                let device = session.get_device_info(&id).await.ok()?;
                let peripheral_id: PeripheralId = device.id.into();
                let mut events = vec![CentralEvent::DeviceDiscovered(peripheral_id.clone())];
                // BlueZ may already know the device's services (from cache or the
                // advertisement).  Emit a ServicesAdvertisement so listeners don't
                // have to wait for a separate PropertiesChanged signal that may
                // never arrive for cached devices.
                if !device.services.is_empty() {
                    events.push(CentralEvent::ServicesAdvertisement {
                        id: peripheral_id,
                        services: device.services,
                    });
                }
                Some(events)
            }
            DeviceEvent::Connected { connected } => {
                if connected {
                    Some(vec![CentralEvent::DeviceConnected(id.into())])
                } else {
                    Some(vec![CentralEvent::DeviceDisconnected(id.into())])
                }
            }
            DeviceEvent::Rssi { rssi } => {
                let device = session.get_device_info(&id).await.ok()?;
                Some(vec![CentralEvent::RssiUpdate {
                    id: device.id.into(),
                    rssi,
                }])
            }
            DeviceEvent::ManufacturerData { manufacturer_data } => {
                let device = session.get_device_info(&id).await.ok()?;
                Some(vec![CentralEvent::ManufacturerDataAdvertisement {
                    id: device.id.into(),
                    manufacturer_data,
                }])
            }
            DeviceEvent::ServiceData { service_data } => {
                let device = session.get_device_info(&id).await.ok()?;
                Some(vec![CentralEvent::ServiceDataAdvertisement {
                    id: device.id.into(),
                    service_data,
                }])
            }
            DeviceEvent::Services { services } => {
                let device = session.get_device_info(&id).await.ok()?;
                Some(vec![CentralEvent::ServicesAdvertisement {
                    id: device.id.into(),
                    services,
                }])
            }
            _ => None,
        },
        BluetoothEvent::Adapter {
            id,
            event: adapter_event,
        } if id == adapter_id => match adapter_event {
            AdapterEvent::Powered { powered } => {
                let state = get_central_state(powered);
                Some(vec![CentralEvent::StateUpdate(state)])
            }
            _ => None,
        },
        _ => None,
    }
}

/// UBM patches (UBM_PATCHES.md #8, #12): LE-only discovery (upstream asked
/// for `Transport: auto`, which adds BR/EDR inquiry to a BLE scan),
/// `DuplicateData` from the caller's duplicate policy (upstream always
/// `true`; `None` keeps that default) and the caller's name prefix as
/// `Pattern`, as the legacy BlueZ backend sent (`bluez-runtime-models.ts`
/// `scanFilterVariant`).
fn discovery_filter(filter: ScanFilter) -> DiscoveryFilter {
    DiscoveryFilter {
        service_uuids: filter.services,
        duplicate_data: Some(filter.allow_duplicates.unwrap_or(true)),
        transport: Some(Transport::Le),
        pattern: filter.name_prefix,
        ..Default::default()
    }
}

#[cfg(test)]
mod ubm_discovery_filter_tests {
    use super::*;

    #[test]
    fn the_name_prefix_becomes_the_discovery_pattern() {
        let filter = discovery_filter(ScanFilter {
            services: Vec::new(),
            allow_duplicates: Some(false),
            name_prefix: Some("Polar".to_owned()),
        });
        assert_eq!(filter.pattern.as_deref(), Some("Polar"));
        assert_eq!(filter.duplicate_data, Some(false));
        assert_eq!(filter.transport, Some(Transport::Le));
    }

    #[test]
    fn no_name_prefix_sets_no_pattern() {
        let filter = discovery_filter(ScanFilter::default());
        assert_eq!(filter.pattern, None);
        assert_eq!(filter.duplicate_data, Some(true));
    }
}

#[cfg(test)]
mod ubm_sighting_tests {
    use super::{DeviceState, reports_sighting};
    use bluez_async::DeviceEvent;

    /// UBM patch #18: discovery and every property-change signal (a
    /// name-only one included) is one sighting; the per-property events of
    /// the same signal are not counted again.
    #[test]
    fn every_device_signal_is_one_sighting() {
        assert!(reports_sighting(&DeviceEvent::Discovered));
        assert!(reports_sighting(&DeviceEvent::PropertiesChanged {
            properties: vec!["Name".to_owned()],
        }));
        for per_property in [
            DeviceEvent::Rssi { rssi: -60 },
            DeviceEvent::Services {
                services: Vec::new(),
            },
            DeviceEvent::Connected { connected: true },
            DeviceEvent::ServicesResolved,
        ] {
            assert!(!reports_sighting(&per_property), "{per_property:?}");
        }
    }

    use crate::api::ReportSource;

    /// UBM patch #17: a BlueZ sighting is the device state, labelled so.
    #[test]
    fn a_bluez_sighting_is_the_labelled_device_state() {
        let services = vec![uuid::Uuid::from_u128(
            0x0000180d_0000_1000_8000_00805f9b34fb,
        )];
        let report = DeviceState {
            name: Some("Polar H10".to_owned()),
            rssi: Some(-60),
            tx_power: Some(4),
            manufacturer_data: [(0x006b, vec![1, 2])].into_iter().collect(),
            service_data: Default::default(),
            services: services.clone(),
        }
        .report();
        assert_eq!(report.source, ReportSource::DeviceState);
        assert_eq!(report.local_name.as_deref(), Some("Polar H10"));
        assert_eq!(report.rssi, Some(-60));
        assert_eq!(report.tx_power_level, Some(4));
        assert_eq!(report.manufacturer_data[&0x006b], vec![1, 2]);
        assert_eq!(report.services, services);
    }
}
