use async_trait::async_trait;
use bluez_async::{
    BluetoothEvent, BluetoothSession, CharacteristicEvent, CharacteristicFlags, CharacteristicId,
    CharacteristicInfo, DescriptorInfo, DeviceId, DeviceInfo, MacAddress, ServiceInfo,
    WriteOptions,
};
use futures::future::{join_all, ready};
use futures::stream::{Stream, StreamExt};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "serde")]
use serde_cr as serde;
use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::api::{
    self, AddressType, BDAddr, CharPropFlags, Characteristic, Descriptor, PeripheralProperties,
    Service, ValueNotification, WriteType,
};
use crate::{Error, Result};
pub use bluez_async::{
    LE_GATT_OBSERVATION_TIMEOUT, LeGattBearer, LeGattErrorStage, LeGattReadyToken, LeGattSnapshot,
    LeGattStatus,
};

// UBM patch (UBM_PATCHES.md #6): every GATT attribute is kept as its own
// instance, keyed by the ATT handle BlueZ encodes in its object path
// (`service%04x`, `char%04x`, `desc%04x`, BlueZ `src/gatt-client.c`).
// Upstream kept one attribute per UUID in `HashMap<Uuid, _>` maps, so
// same-UUID services, characteristics and descriptors collapsed into one.

#[derive(Clone, Debug)]
struct DescriptorInternal {
    handle: u64,
    info: DescriptorInfo,
}

#[derive(Clone, Debug)]
struct CharacteristicInternal {
    handle: u64,
    info: CharacteristicInfo,
    descriptors: Vec<DescriptorInternal>,
}

#[derive(Clone, Debug)]
struct ServiceInternal {
    handle: u64,
    info: ServiceInfo,
    characteristics: Vec<CharacteristicInternal>,
}

#[derive(Clone, Debug, Default)]
struct PublishedGatt {
    services: Vec<ServiceInternal>,
    accepted_token: Option<LeGattReadyToken>,
}

fn publish_gatt(publication: &Mutex<PublishedGatt>, candidate: PublishedGatt) -> Result<()> {
    let mut current = publication.lock().map_err(Into::<Error>::into)?;
    if let Some(accepted) = &current.accepted_token {
        let Some(proposed) = &candidate.accepted_token else {
            return Err(bluez_async::BluetoothError::LeGattProtocolError(
                "an unattested graph cannot replace an owner-pinned publication".into(),
            )
            .into());
        };
        // A clone may have completed its bracket before another clone's
        // newer publication. Compare under the same commit lock, not before
        // waiting for it. Unique owner replacement is never inferred here.
        if proposed.daemon_owner != accepted.daemon_owner
            || (proposed.attachment, proposed.revision) < (accepted.attachment, accepted.revision)
        {
            return Err(bluez_async::BluetoothError::LeGattTokenChanged {
                before: proposed.clone(),
                after: accepted.clone(),
            }
            .into());
        }
    }
    *current = candidate;
    Ok(())
}

/// The ATT handle BlueZ encodes in the last segment of a GATT object path
/// (`.../service0010` → `0x10`). `None` when the segment does not follow
/// BlueZ's naming.
pub(crate) fn handle_from_object_path(path: &str) -> Option<u64> {
    let segment = path.rsplit('/').next()?;
    let digits = segment.trim_start_matches(|c: char| c.is_ascii_lowercase());
    if digits.is_empty() || digits.len() == segment.len() {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn object_handle(path: &str) -> Result<u64> {
    handle_from_object_path(path).ok_or_else(|| {
        Error::Other(format!("BlueZ GATT object path {path} carries no ATT handle").into())
    })
}

#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_cr")
)]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeripheralId(pub(crate) DeviceId);

impl Display for PeripheralId {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Implementation of [api::Peripheral](crate::api::Peripheral).
#[derive(Clone, Debug)]
pub struct Peripheral {
    session: BluetoothSession,
    device: DeviceId,
    mac_address: BDAddr,
    services: Arc<Mutex<PublishedGatt>>,
}

fn get_characteristic<'a>(
    services: &'a [ServiceInternal],
    service_uuid: &Uuid,
    service_instance: u64,
    characteristic_uuid: &Uuid,
    characteristic_instance: u64,
) -> Result<&'a CharacteristicInternal> {
    services
        .iter()
        .find(|service| service.info.uuid == *service_uuid && service.handle == service_instance)
        .ok_or_else(|| {
            Error::Other(
                format!("Service with UUID {service_uuid} (handle {service_instance:#06x}) not found.")
                    .into(),
            )
        })?
        .characteristics
        .iter()
        .find(|characteristic| {
            characteristic.info.uuid == *characteristic_uuid
                && characteristic.handle == characteristic_instance
        })
        .ok_or_else(|| {
            Error::Other(
                format!(
                    "Characteristic with UUID {characteristic_uuid} (handle {characteristic_instance:#06x}) not found."
                )
                .into(),
            )
        })
}

impl Peripheral {
    /// Nonrecycled private identities shared across this actual sender lifetime.
    pub fn allocate_le_reservation_id(&self) -> Result<u64> {
        Ok(self.session.allocate_le_reservation_id()?)
    }

    /// Metadata housekeeping only, after consuming the exact terminal receipt.
    pub async fn acknowledge_le_lease(&self, owner: &str, token: u64) -> Result<()> {
        Ok(self
            .session
            .acknowledge_le_lease(&self.device, owner, token)
            .await?)
    }

    /// Reserve sender ownership before any accepted LE connection effect.
    pub async fn reserve_le_lease(&self, owner: &str, reservation_id: u64) -> Result<u64> {
        Ok(self
            .session
            .reserve_le_lease(&self.device, owner, reservation_id)
            .await?)
    }

    /// Reconcile an original identity without acquiring any new connection.
    pub async fn recover_le_lease(&self, owner: &str, reservation_id: u64) -> Result<Option<u64>> {
        Ok(self
            .session
            .recover_le_lease(&self.device, owner, reservation_id)
            .await?)
    }

    /// Connect the exact retained reservation on its original daemon owner.
    pub async fn connect_le_lease(&self, owner: &str, token: u64) -> Result<u64> {
        Ok(self
            .session
            .connect_le_lease(&self.device, owner, token)
            .await?)
    }

    /// Preserve authoritative scope; lease retirement is not ACL disconnection.
    pub async fn release_le_lease(
        &self,
        owner: &str,
        token: u64,
        expected_generation: Option<u64>,
    ) -> Result<bluez_async::LeLeaseReleaseReceipt> {
        Ok(self
            .session
            .release_le_lease(&self.device, owner, token, expected_generation)
            .await?)
    }

    /// Pin this peripheral's GATT calls to an explicitly attested BlueZ
    /// daemon epoch. No connection or subscription is created here.
    pub async fn with_le_owner(mut self, owner: &str) -> Result<Self> {
        self.session = self.session.with_le_owner(owner).await?;
        Ok(self)
    }

    /// Strict LE-only acquisition; never invokes Device1.Connect.
    pub async fn connect_le(&self, owner: &str) -> Result<()> {
        self.session.connect_le(&self.device, owner).await?;
        Ok(())
    }

    /// Retryable LE-only release of accepted work on the same daemon epoch.
    pub async fn disconnect_le(&self, owner: &str) -> Result<()> {
        self.session.disconnect_le(&self.device, owner).await?;
        Ok(())
    }

    /// Read the current daemon answer on this peripheral's strict owner.
    pub async fn le_gatt_snapshot(&self) -> Result<LeGattSnapshot> {
        let owner = self.session.attested_le_owner().ok_or_else(|| {
            Error::NotSupported(
                "LE GATT snapshot requires an explicitly attested daemon owner".into(),
            )
        })?;
        Ok(self.session.le_gatt_snapshot(&self.device, owner).await?)
    }

    /// The token accepted with the last successfully published complete graph.
    /// Shared by clones. No async work or new radio ownership is performed.
    pub fn accepted_le_gatt_ready_token(&self) -> Result<Option<LeGattReadyToken>> {
        Ok(self
            .services
            .lock()
            .map_err(Into::<Error>::into)?
            .accepted_token
            .clone())
    }

    /// Capture the exact current characteristic path for retained notification
    /// cleanup. The returned cleanup-only peripheral has an independent graph;
    /// later publication on the original peripheral cannot retarget its STOP.
    /// No discovery, connection or notification is created here.
    pub fn notification_cleanup_peripheral(&self, characteristic: &Characteristic) -> Result<Self> {
        let publication = self.services.lock().map_err(Into::<Error>::into)?;
        let selected = get_characteristic(
            &publication.services,
            &characteristic.service_uuid,
            characteristic.service_instance,
            &characteristic.uuid,
            characteristic.instance,
        )?;
        let service = publication
            .services
            .iter()
            .find(|service| service.info.id == selected.info.id.service())
            .ok_or_else(|| Error::Other("Selected characteristic service is absent".into()))?;
        let mut retained_service = service.clone();
        retained_service.characteristics = vec![selected.clone()];
        let mut retained = self.clone();
        retained.services = Arc::new(Mutex::new(PublishedGatt {
            services: vec![retained_service],
            accepted_token: publication.accepted_token.clone(),
        }));
        Ok(retained)
    }

    pub(crate) fn new(session: BluetoothSession, device: DeviceInfo) -> Self {
        Peripheral {
            session,
            device: device.id,
            mac_address: device.mac_address.into(),
            services: Arc::new(Mutex::new(PublishedGatt::default())),
        }
    }

    fn characteristic_info(&self, characteristic: &Characteristic) -> Result<CharacteristicInfo> {
        let services = self.services.lock().map_err(Into::<Error>::into)?;
        get_characteristic(
            &services.services,
            &characteristic.service_uuid,
            characteristic.service_instance,
            &characteristic.uuid,
            characteristic.instance,
        )
        .map(|c| &c.info)
        .cloned()
    }

    fn descriptor_info(&self, descriptor: &Descriptor) -> Result<DescriptorInfo> {
        let services = self.services.lock().map_err(Into::<Error>::into)?;
        let characteristic = get_characteristic(
            &services.services,
            &descriptor.service_uuid,
            descriptor.service_instance,
            &descriptor.characteristic_uuid,
            descriptor.characteristic_instance,
        )?;
        characteristic
            .descriptors
            .iter()
            .find(|entry| entry.info.uuid == descriptor.uuid && entry.handle == descriptor.instance)
            .map(|entry| entry.info.clone())
            .ok_or_else(|| {
                Error::Other(
                    format!(
                        "Descriptor with UUID {} (handle {:#06x}) not found.",
                        descriptor.uuid, descriptor.instance
                    )
                    .into(),
                )
            })
    }

    async fn device_info(&self) -> Result<DeviceInfo> {
        Ok(self.session.get_device_info(&self.device).await?)
    }
}

#[async_trait]
impl api::Peripheral for Peripheral {
    fn id(&self) -> PeripheralId {
        PeripheralId(self.device.to_owned())
    }

    fn address(&self) -> BDAddr {
        self.mac_address
    }

    fn mtu(&self) -> u16 {
        // UBM patch (UBM_PATCHES.md #6): upstream unwrapped the first
        // characteristic's `MTU`, which BlueZ may withhold (a panic). The
        // first characteristic that reports one answers.
        let services = self.services.lock().unwrap();
        services
            .services
            .iter()
            .flat_map(|service| service.characteristics.iter())
            .find_map(|characteristic| characteristic.info.mtu)
            .unwrap_or(api::DEFAULT_MTU_SIZE)
    }

    async fn properties(&self) -> Result<Option<PeripheralProperties>> {
        let device_info = self.device_info().await?;
        Ok(Some(PeripheralProperties {
            address: device_info.mac_address.into(),
            address_type: Some(device_info.address_type.into()),
            local_name: device_info.alias.or(device_info.name.clone()),
            advertisement_name: device_info.name,
            tx_power_level: device_info.tx_power,
            rssi: device_info.rssi,
            manufacturer_data: device_info.manufacturer_data,
            service_data: device_info.service_data,
            services: device_info.services,
            class: device_info.class,
        }))
    }

    fn services(&self) -> BTreeSet<Service> {
        self.services
            .lock()
            .unwrap()
            .services
            .iter()
            .map(|service| service.into())
            .collect()
    }

    async fn is_connected(&self) -> Result<bool> {
        let device_info = self.device_info().await?;
        Ok(device_info.connected)
    }

    async fn connect(&self) -> Result<()> {
        self.session.connect(&self.device).await?;
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.session.disconnect(&self.device).await?;
        Ok(())
    }

    async fn discover_services(&self) -> Result<()> {
        // UBM patch (UBM_PATCHES.md #6): every characteristic and descriptor
        // instance is kept (upstream kept the first of each UUID), and a
        // descriptor listing that fails fails the discovery (upstream
        // replaced it with an empty list).
        let before = if let Some(owner) = self.session.attested_le_owner() {
            Some(
                self.session
                    .await_le_gatt_ready(&self.device, owner)
                    .await?,
            )
        } else {
            None
        };
        let mut services_internal = Vec::new();
        let services = self.session.get_services(&self.device).await?;
        for service in services {
            let service_handle = object_handle(&service.id.to_string())?;
            let characteristics = self.session.get_characteristics(&service.id).await?;
            let characteristics = join_all(characteristics.into_iter().map(|info| async move {
                let handle = object_handle(&info.id.to_string())?;
                let descriptors = self
                    .session
                    .get_descriptors(&info.id)
                    .await?
                    .into_iter()
                    .map(|descriptor| {
                        Ok(DescriptorInternal {
                            handle: object_handle(&descriptor.id.to_string())?,
                            info: descriptor,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok::<_, Error>(CharacteristicInternal {
                    handle,
                    info,
                    descriptors,
                })
            }))
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
            services_internal.push(ServiceInternal {
                handle: service_handle,
                info: service,
                characteristics,
            });
        }
        if let Some(before) = &before {
            // Never wait/retry the publication bracket: the candidate was read
            // under THIS token, not a later successful rediscovery.
            let after = self.le_gatt_snapshot().await?.ready_token()?;
            if &after != before {
                return Err(bluez_async::BluetoothError::LeGattTokenChanged {
                    before: before.clone(),
                    after,
                }
                .into());
            }
        }
        // Graph and token are committed together; failed parsing/RPC/brackets
        // leave the prior publication untouched. No await under this lock.
        publish_gatt(
            &self.services,
            PublishedGatt {
                services: services_internal,
                accepted_token: before,
            },
        )
    }

    async fn write(
        &self,
        characteristic: &Characteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        let characteristic_info = self.characteristic_info(characteristic)?;
        let options = WriteOptions {
            write_type: Some(write_type.into()),
            ..Default::default()
        };
        Ok(self
            .session
            .write_characteristic_value_with_options(&characteristic_info.id, data, options)
            .await?)
    }

    async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        let characteristic_info = self.characteristic_info(characteristic)?;
        Ok(self
            .session
            .read_characteristic_value(&characteristic_info.id)
            .await?)
    }

    async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let characteristic_info = self.characteristic_info(characteristic)?;
        Ok(self.session.start_notify(&characteristic_info.id).await?)
    }

    async fn unsubscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let characteristic_info = self.characteristic_info(characteristic)?;
        Ok(self.session.stop_notify(&characteristic_info.id).await?)
    }

    async fn notifications(&self) -> Result<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>> {
        let device_id = self.device.clone();
        let events = self.session.device_event_stream(&device_id).await?;
        let services = self.services.clone();
        Ok(Box::pin(events.filter_map(move |event| {
            ready(value_notification(event, &device_id, services.clone()))
        })))
    }

    async fn read_rssi(&self) -> Result<i16> {
        let device_info = self.device_info().await?;
        device_info.rssi.ok_or(Error::NotConnected)
    }

    async fn write_descriptor(&self, descriptor: &Descriptor, data: &[u8]) -> Result<()> {
        let descriptor_info = self.descriptor_info(descriptor)?;
        Ok(self
            .session
            .write_descriptor_value(&descriptor_info.id, data)
            .await?)
    }

    async fn read_descriptor(&self, descriptor: &Descriptor) -> Result<Vec<u8>> {
        let descriptor_info = self.descriptor_info(descriptor)?;
        Ok(self
            .session
            .read_descriptor_value(&descriptor_info.id)
            .await?)
    }
}

fn value_notification(
    event: BluetoothEvent,
    device_id: &DeviceId,
    services: Arc<Mutex<PublishedGatt>>,
) -> Option<ValueNotification> {
    match event {
        BluetoothEvent::Characteristic {
            id,
            event: CharacteristicEvent::Value { value },
        } if id.service().device() == *device_id => {
            let services = services.lock().unwrap();
            let (charac, service) = find_characteristic_by_id(&services.services, id.clone())?;
            Some(ValueNotification {
                uuid: charac.info.uuid,
                instance: charac.handle,
                service_uuid: service.info.uuid,
                service_instance: service.handle,
                value,
                lost_before: 0,
            })
        }
        _ => None,
    }
}

fn find_characteristic_by_id(
    services: &[ServiceInternal],
    characteristic_id: CharacteristicId,
) -> Option<(&CharacteristicInternal, &ServiceInternal)> {
    for service in services {
        for characteristic in &service.characteristics {
            if characteristic.info.id == characteristic_id {
                return Some((characteristic, service));
            }
        }
    }
    None
}

impl From<WriteType> for bluez_async::WriteType {
    fn from(write_type: WriteType) -> Self {
        match write_type {
            WriteType::WithoutResponse => bluez_async::WriteType::WithoutResponse,
            WriteType::WithResponse => bluez_async::WriteType::WithResponse,
        }
    }
}

impl From<MacAddress> for BDAddr {
    fn from(mac_address: MacAddress) -> Self {
        <[u8; 6]>::into(mac_address.into())
    }
}

impl From<DeviceId> for PeripheralId {
    fn from(device_id: DeviceId) -> Self {
        PeripheralId(device_id)
    }
}

impl From<bluez_async::AddressType> for AddressType {
    fn from(address_type: bluez_async::AddressType) -> Self {
        match address_type {
            bluez_async::AddressType::Public => AddressType::Public,
            bluez_async::AddressType::Random => AddressType::Random,
        }
    }
}

fn make_descriptor(
    descriptor: &DescriptorInternal,
    characteristic: &CharacteristicInternal,
    service: &ServiceInternal,
) -> Descriptor {
    Descriptor {
        uuid: descriptor.info.uuid,
        instance: descriptor.handle,
        characteristic_uuid: characteristic.info.uuid,
        characteristic_instance: characteristic.handle,
        service_uuid: service.info.uuid,
        service_instance: service.handle,
    }
}

fn make_characteristic(
    characteristic: &CharacteristicInternal,
    service: &ServiceInternal,
) -> Characteristic {
    Characteristic {
        uuid: characteristic.info.uuid,
        instance: characteristic.handle,
        properties: characteristic.info.flags.into(),
        descriptors: characteristic
            .descriptors
            .iter()
            .map(|descriptor| make_descriptor(descriptor, characteristic, service))
            .collect(),
        service_uuid: service.info.uuid,
        service_instance: service.handle,
    }
}

impl From<&ServiceInternal> for Service {
    fn from(service: &ServiceInternal) -> Self {
        Service {
            uuid: service.info.uuid,
            instance: service.handle,
            primary: service.info.primary,
            characteristics: service
                .characteristics
                .iter()
                .map(|characteristic| make_characteristic(characteristic, service))
                .collect(),
        }
    }
}

impl From<CharacteristicFlags> for CharPropFlags {
    fn from(flags: CharacteristicFlags) -> Self {
        let mut result = CharPropFlags::default();
        if flags.contains(CharacteristicFlags::BROADCAST) {
            result.insert(CharPropFlags::BROADCAST);
        }
        if flags.contains(CharacteristicFlags::READ) {
            result.insert(CharPropFlags::READ);
        }
        if flags.contains(CharacteristicFlags::WRITE_WITHOUT_RESPONSE) {
            result.insert(CharPropFlags::WRITE_WITHOUT_RESPONSE);
        }
        if flags.contains(CharacteristicFlags::WRITE) {
            result.insert(CharPropFlags::WRITE);
        }
        if flags.contains(CharacteristicFlags::NOTIFY) {
            result.insert(CharPropFlags::NOTIFY);
        }
        if flags.contains(CharacteristicFlags::INDICATE) {
            result.insert(CharPropFlags::INDICATE);
        }
        if flags.contains(CharacteristicFlags::SIGNED_WRITE) {
            result.insert(CharPropFlags::AUTHENTICATED_SIGNED_WRITES);
        }
        if flags.contains(CharacteristicFlags::EXTENDED_PROPERTIES) {
            result.insert(CharPropFlags::EXTENDED_PROPERTIES);
        }
        result
    }
}

#[cfg(test)]
mod ubm_instance_tests {
    use super::handle_from_object_path;

    #[test]
    fn bluez_object_paths_carry_att_handles() {
        assert_eq!(
            handle_from_object_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010"),
            Some(0x10)
        );
        assert_eq!(
            handle_from_object_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char002a"),
            Some(0x2a)
        );
        assert_eq!(
            handle_from_object_path(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/char002a/desc002c"
            ),
            Some(0x2c)
        );
        assert_eq!(handle_from_object_path("/org/bluez/nohandle"), None);
    }
}

#[cfg(test)]
#[path = "le_gatt_tests.rs"]
mod le_gatt_tests;
