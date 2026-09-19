//! Radio boundary: one trait, one backend per platform.
//!
//! [`PeripheralRadio`] is backend-neutral: the simulator logic in `sim.rs`
//! only sees service declarations built from [`CharProperty`] /
//! [`CharPermission`] and the [`RadioEvent`] stream. The [`CharProperty`]
//! matches are exhaustive on purpose — adding a variant fails to compile
//! every backend until its mapping is updated.
//!
//! * macOS / Windows: [`CrateRadio`], implemented with `ble-peripheral-rust`
//!   0.2.0 (CoreBluetooth via `objc2-core-bluetooth`, WinRT via `windows`).
//!   The crate hardcodes its BlueZ advertisement object (name and service
//!   UUIDs only, no control over Includes/Appearance/TxPower/duration) and
//!   registers the advertisement before the GATT application.
//! * Linux: [`BluerRadio`](crate::bluer_radio::BluerRadio) in
//!   `src/bluer_radio.rs`, implemented with `bluer` 0.17.4 directly so every
//!   `LEAdvertisement1` property stays under the sim's control and the GATT
//!   application is registered before the advertisement.
//!
//! Deliberately not used anywhere: `btleplug` (central role only), `bluest`
//! (no GATT server on macOS), raw `objc2-core-bluetooth` (would need a
//! hand-written delegate, run-loop pump and D-Bus GATT app for the same
//! surface).
//!
//! The advertised set (180D + FEEE) and the 31-byte payload budget live in
//! [`crate::advertisement`]; both backends advertise exactly that set.

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[cfg(not(target_os = "linux"))]
use ble_peripheral_rust::{
    gatt::{
        characteristic::Characteristic,
        peripheral_event::{
            PeripheralEvent, ReadRequestResponse, RequestResponse, WriteRequestResponse,
        },
        properties::{AttributePermission, CharacteristicProperty},
        service::Service,
    },
    Peripheral, PeripheralImpl,
};

#[cfg(target_os = "linux")]
#[path = "bluer_radio.rs"]
mod bluer_radio;
#[cfg(target_os = "linux")]
pub use self::bluer_radio::BluerRadio as PlatformRadio;
#[cfg(not(target_os = "linux"))]
pub use self::CrateRadio as PlatformRadio;

use crate::{advertisement, gatt_spec, sim::SimConfig};

/// Transport failure. Carries the backend message; never swallowed.
#[derive(Debug, Clone)]
pub struct RadioError(pub String);

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "radio error: {}", self.0)
    }
}

impl std::error::Error for RadioError {}

/// Answer to a read request: bytes from `offset`, or an ATT-style refusal.
#[derive(Debug)]
pub struct RadioReadAnswer {
    pub value: Vec<u8>,
    pub ok: bool,
}

/// Backend-agnostic peripheral events delivered to the simulator loop.
#[derive(Debug)]
pub enum RadioEvent {
    Powered(bool),
    Subscription {
        service: String,
        characteristic: String,
        subscribed: bool,
    },
    Read {
        service: String,
        characteristic: String,
        offset: u64,
        reply: oneshot::Sender<RadioReadAnswer>,
    },
    Write {
        service: String,
        characteristic: String,
        value: Vec<u8>,
        reply: oneshot::Sender<bool>,
    },
}

/// Characteristic properties the simulator needs. Every backend maps these
/// exhaustively onto its own GATT types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharProperty {
    Read,
    Write,
    Notify,
    Indicate,
}

/// GATT permissions the simulator needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharPermission {
    Readable,
    Writeable,
}

/// Characteristic declaration, mapped onto the backend's GATT types.
#[derive(Debug, Clone)]
pub struct CharSpec {
    pub uuid: Uuid,
    pub properties: Vec<CharProperty>,
    pub permissions: Vec<CharPermission>,
    pub initial_value: Option<Vec<u8>>,
}

/// Service declaration.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub uuid: Uuid,
    pub characteristics: Vec<CharSpec>,
}

/// Radio backend surface the simulator needs. The three defaulted methods let
/// Linux-only capabilities grow without touching the macOS/Windows backend:
/// backends without the OS API inherit a counted no-op.
#[async_trait]
pub trait PeripheralRadio: Send {
    async fn open(events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError>
    where
        Self: Sized;
    async fn is_powered(&mut self) -> Result<bool, RadioError>;
    async fn is_advertising(&mut self) -> Result<bool, RadioError>;
    async fn start_advertising(&mut self, name: &str, uuids: &[Uuid]) -> Result<(), RadioError>;
    async fn stop_advertising(&mut self) -> Result<(), RadioError>;
    async fn add_service(&mut self, service: &ServiceSpec) -> Result<(), RadioError>;
    async fn notify(&mut self, characteristic: Uuid, value: Vec<u8>) -> Result<(), RadioError>;
    /// Stages manufacturer data for the next advertisement. Default: no-op
    /// (Apple exposes no manufacturer-data peripheral API).
    async fn set_adv_manufacturer_data(
        &mut self,
        _company: u16,
        _data: Vec<u8>,
    ) -> Result<(), RadioError> {
        Ok(())
    }
    /// Whether staged manufacturer data actually goes over the air.
    fn supports_manufacturer_data(&self) -> bool {
        false
    }
    /// Disconnects connected centrals, returning how many links were dropped.
    /// Default: no-op (no disconnect API on that backend).
    async fn disconnect_centrals(&mut self) -> Result<usize, RadioError> {
        Ok(0)
    }
}

/// Short-UUID declarations for the whole H10 surface. Initial values mirror the
/// simulator config so backends that serve reads from the declaration agree
/// with the event-driven answers in the main loop.
pub fn h10_services(config: &SimConfig) -> Result<Vec<ServiceSpec>, RadioError> {
    let read = || vec![CharPermission::Readable];
    let write = || vec![CharPermission::Writeable];
    Ok(vec![
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::HEART_RATE_SERVICE),
            characteristics: vec![
                CharSpec {
                    uuid: advertisement::short_uuid(gatt_spec::uuid16::HEART_RATE_MEASUREMENT),
                    properties: vec![CharProperty::Notify],
                    permissions: Vec::new(),
                    initial_value: None,
                },
                CharSpec {
                    uuid: advertisement::short_uuid(gatt_spec::uuid16::BODY_SENSOR_LOCATION),
                    properties: vec![CharProperty::Read],
                    permissions: read(),
                    initial_value: Some(gatt_spec::encode_body_sensor_location()),
                },
            ],
        },
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::BATTERY_SERVICE),
            characteristics: vec![CharSpec {
                uuid: advertisement::short_uuid(gatt_spec::uuid16::BATTERY_LEVEL),
                properties: vec![CharProperty::Read, CharProperty::Notify],
                permissions: read(),
                initial_value: Some(gatt_spec::encode_battery_level(config.battery_percent)),
            }],
        },
        ServiceSpec {
            uuid: advertisement::short_uuid(gatt_spec::uuid16::DEVICE_INFORMATION_SERVICE),
            characteristics: vec![
                (gatt_spec::uuid16::MANUFACTURER_NAME, "Polar Electro Oy"),
                (gatt_spec::uuid16::MODEL_NUMBER, "H10"),
                (gatt_spec::uuid16::SERIAL_NUMBER, "SIM000001"),
                (gatt_spec::uuid16::FIRMWARE_REVISION, "3.2.1"),
                (gatt_spec::uuid16::HARDWARE_REVISION, "9"),
                (gatt_spec::uuid16::SOFTWARE_REVISION, "3.2.1"),
            ]
            .into_iter()
            .map(|(short, text): (u16, &str)| CharSpec {
                uuid: advertisement::short_uuid(short),
                properties: vec![CharProperty::Read],
                permissions: read(),
                initial_value: Some(match short {
                    x if x == gatt_spec::uuid16::MANUFACTURER_NAME => {
                        config.manufacturer.as_bytes().to_vec()
                    }
                    x if x == gatt_spec::uuid16::MODEL_NUMBER => config.model.as_bytes().to_vec(),
                    x if x == gatt_spec::uuid16::SERIAL_NUMBER => config.serial.as_bytes().to_vec(),
                    x if x == gatt_spec::uuid16::FIRMWARE_REVISION => {
                        config.firmware.as_bytes().to_vec()
                    }
                    x if x == gatt_spec::uuid16::HARDWARE_REVISION => {
                        config.hardware.as_bytes().to_vec()
                    }
                    x if x == gatt_spec::uuid16::SOFTWARE_REVISION => {
                        config.software.as_bytes().to_vec()
                    }
                    _ => text.as_bytes().to_vec(),
                }),
            })
            .chain(std::iter::once(CharSpec {
                uuid: advertisement::short_uuid(gatt_spec::uuid16::SYSTEM_ID),
                properties: vec![CharProperty::Read],
                permissions: read(),
                initial_value: Some(gatt_spec::encode_system_id(1, [0x6B, 0x00, 0x00])),
            }))
            .collect(),
        },
        ServiceSpec {
            uuid: parse_uuid(gatt_spec::pmd::SERVICE)?,
            characteristics: vec![
                CharSpec {
                    uuid: parse_uuid(gatt_spec::pmd::CONTROL_POINT)?,
                    properties: vec![
                        CharProperty::Read,
                        CharProperty::Write,
                        CharProperty::Indicate,
                    ],
                    permissions: {
                        let mut permissions = read();
                        permissions.extend(write());
                        permissions
                    },
                    initial_value: Some(gatt_spec::encode_pmd_features()),
                },
                CharSpec {
                    uuid: parse_uuid(gatt_spec::pmd::DATA)?,
                    properties: vec![CharProperty::Notify],
                    permissions: Vec::new(),
                    initial_value: None,
                },
            ],
        },
    ])
}

fn parse_uuid(text: &str) -> Result<Uuid, RadioError> {
    Uuid::parse_str(text).map_err(|error| RadioError(format!("invalid UUID {text}: {error}")))
}

/// Service UUIDs carried in the advertisement: Heart Rate + Polar (FEEE).
/// Pinned to [`advertisement::ADVERTISED_SERVICES`]; the 128-bit PMD service
/// UUID is served over GATT, never advertised.
pub fn adv_service_uuids() -> Vec<Uuid> {
    advertisement::ADVERTISED_SERVICES
        .iter()
        .map(|short| advertisement::short_uuid(*short))
        .collect()
}

/// 16-bit short UUID of `uuid`, for routing read/write events to `SimState`.
pub fn short_of(uuid: &Uuid) -> Option<u16> {
    let fields = uuid.as_fields();
    if fields.1 == 0 && fields.2 == 0x1000 && fields.3 == b"\x80\x00\x00\x80\x5F\x9B\x34\xFB" {
        Some((fields.0 & 0xFFFF) as u16)
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn map_property(property: &CharProperty) -> CharacteristicProperty {
    match property {
        CharProperty::Read => CharacteristicProperty::Read,
        CharProperty::Write => CharacteristicProperty::Write,
        CharProperty::Notify => CharacteristicProperty::Notify,
        CharProperty::Indicate => CharacteristicProperty::Indicate,
    }
}

#[cfg(not(target_os = "linux"))]
fn map_permission(permission: &CharPermission) -> AttributePermission {
    match permission {
        CharPermission::Readable => AttributePermission::Readable,
        CharPermission::Writeable => AttributePermission::Writeable,
    }
}

/// [`PeripheralRadio`] implemented with `ble-peripheral-rust` (macOS/Windows).
#[cfg(not(target_os = "linux"))]
pub struct CrateRadio {
    inner: Peripheral,
}

#[cfg(not(target_os = "linux"))]
#[async_trait]
impl PeripheralRadio for CrateRadio {
    async fn open(events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError> {
        let (backend_tx, mut backend_rx) = mpsc::channel::<PeripheralEvent>(256);
        let inner = Peripheral::new(backend_tx)
            .await
            .map_err(|error| RadioError(error.to_string()))?;
        tokio::spawn(async move {
            while let Some(event) = backend_rx.recv().await {
                if translate(event, &events).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self { inner })
    }

    async fn is_powered(&mut self) -> Result<bool, RadioError> {
        self.inner
            .is_powered()
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn is_advertising(&mut self) -> Result<bool, RadioError> {
        self.inner
            .is_advertising()
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn start_advertising(&mut self, name: &str, uuids: &[Uuid]) -> Result<(), RadioError> {
        self.inner
            .start_advertising(name, uuids)
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn stop_advertising(&mut self) -> Result<(), RadioError> {
        self.inner
            .stop_advertising()
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn add_service(&mut self, service: &ServiceSpec) -> Result<(), RadioError> {
        let backend = Service {
            uuid: service.uuid,
            primary: true,
            characteristics: service
                .characteristics
                .iter()
                .map(|characteristic| Characteristic {
                    uuid: characteristic.uuid,
                    properties: characteristic.properties.iter().map(map_property).collect(),
                    permissions: characteristic
                        .permissions
                        .iter()
                        .map(map_permission)
                        .collect(),
                    value: characteristic.initial_value.clone(),
                    descriptors: Vec::new(),
                })
                .collect(),
        };
        self.inner
            .add_service(&backend)
            .await
            .map_err(|error| RadioError(error.to_string()))
    }

    async fn notify(&mut self, characteristic: Uuid, value: Vec<u8>) -> Result<(), RadioError> {
        self.inner
            .update_characteristic(characteristic, value)
            .await
            .map_err(|error| RadioError(error.to_string()))
    }
}

#[cfg(not(target_os = "linux"))]
async fn translate(event: PeripheralEvent, events: &mpsc::Sender<RadioEvent>) -> Result<(), ()> {
    match event {
        PeripheralEvent::StateUpdate { is_powered } => {
            events
                .send(RadioEvent::Powered(is_powered))
                .await
                .map_err(|_| ())?;
        }
        PeripheralEvent::CharacteristicSubscriptionUpdate {
            request,
            subscribed,
        } => {
            events
                .send(RadioEvent::Subscription {
                    service: request.service.to_string(),
                    characteristic: request.characteristic.to_string(),
                    subscribed,
                })
                .await
                .map_err(|_| ())?;
        }
        PeripheralEvent::ReadRequest {
            request,
            offset,
            responder,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel::<RadioReadAnswer>();
            events
                .send(RadioEvent::Read {
                    service: request.service.to_string(),
                    characteristic: request.characteristic.to_string(),
                    offset,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| ())?;
            let answer = reply_rx.await.unwrap_or(RadioReadAnswer {
                value: Vec::new(),
                ok: false,
            });
            let _ = responder.send(ReadRequestResponse {
                value: answer.value,
                response: if answer.ok {
                    RequestResponse::Success
                } else {
                    RequestResponse::RequestNotSupported
                },
            });
        }
        PeripheralEvent::WriteRequest {
            request,
            value,
            offset: _,
            responder,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel::<bool>();
            events
                .send(RadioEvent::Write {
                    service: request.service.to_string(),
                    characteristic: request.characteristic.to_string(),
                    value,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| ())?;
            let accepted = reply_rx.await.unwrap_or(false);
            let _ = responder.send(WriteRequestResponse {
                response: if accepted {
                    RequestResponse::Success
                } else {
                    RequestResponse::RequestNotSupported
                },
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopRadio;

    #[async_trait]
    impl PeripheralRadio for NoopRadio {
        async fn open(_events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError> {
            Ok(Self)
        }
        async fn is_powered(&mut self) -> Result<bool, RadioError> {
            Ok(true)
        }
        async fn is_advertising(&mut self) -> Result<bool, RadioError> {
            Ok(false)
        }
        async fn start_advertising(
            &mut self,
            _name: &str,
            _uuids: &[Uuid],
        ) -> Result<(), RadioError> {
            Ok(())
        }
        async fn stop_advertising(&mut self) -> Result<(), RadioError> {
            Ok(())
        }
        async fn add_service(&mut self, _service: &ServiceSpec) -> Result<(), RadioError> {
            Ok(())
        }
        async fn notify(
            &mut self,
            _characteristic: Uuid,
            _value: Vec<u8>,
        ) -> Result<(), RadioError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn defaulted_backend_methods_are_counted_noops() {
        let mut radio = NoopRadio;
        assert!(radio
            .set_adv_manufacturer_data(0x006B, vec![1])
            .await
            .is_ok());
        assert!(
            !radio.supports_manufacturer_data(),
            "a backend without the OS API must say so"
        );
        assert_eq!(radio.disconnect_centrals().await.unwrap(), 0);
    }
}
