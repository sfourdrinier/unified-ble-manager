use super::characteristic::BLECharacteristic;
use super::descriptor::AttributeKey;
use crate::api::Service;
use std::collections::HashMap;
use uuid::Uuid;

/// One discovered primary service. UBM patch (`winrt-attribute-instances`):
/// `instance` is the service's `AttributeHandle`, and characteristics are
/// keyed by (UUID, handle) so repeated UUIDs stay distinct instances.
#[derive(Debug)]
pub struct BLEService {
    pub uuid: Uuid,
    pub instance: u64,
    pub characteristics: HashMap<AttributeKey, BLECharacteristic>,
}

impl BLEService {
    pub fn key(&self) -> AttributeKey {
        (self.uuid, self.instance)
    }

    pub fn to_service(&self) -> Service {
        let characteristics = self
            .characteristics
            .values()
            .map(|ble_characteristic| {
                ble_characteristic.to_characteristic(self.uuid, self.instance)
            })
            .collect();
        Service {
            uuid: self.uuid,
            instance: self.instance,
            primary: true,
            characteristics,
        }
    }
}
