//! UniFFI facade of the process-owned mobile owner (`ubm-mobile`): the
//! `Mobile*` types of `ubm_echo.udl`. Swift implements
//! `MobilePlatformRadio` over `OwnedCoreBluetoothProtocolRadio`; every
//! request is answered through `MobileCoreHost.complete`.
//!
//! No path from here reaches the staged/fake radio (guarded by
//! `mobile_surface_has_no_staged_path`).

use std::sync::{Arc, Mutex, OnceLock};

use ubm_desktop::{
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, DesktopError, ObservedDelivery,
    PropertyFlags, ReadProvenance, ServiceSnapshot,
};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, Advertisement,
    AuthenticationState, BondState, BondedPeer, CloseFailure, CompletionStatus, EncryptionState,
    FailureKind, HostOptions, IngressClass, Instance, ManufacturerData, MobileHost, MobilePlatform,
    MobileSession, PhyObservation, PlatformFailure, PlatformRadio, RadioCompletion, RadioIngress,
    RadioRequest, RestoredPeer, SecureConnectionsState, SecurityState, ServiceData, WakeSink,
    WriteLimits,
};

use crate::build_identity::ubm_build_identity_json;

/// Mirrors UDL `[Error] interface MobileCoreError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileCoreError {
    Failed {
        code: String,
        domain: String,
        operation: String,
        detail: Option<String>,
    },
}

impl std::fmt::Display for MobileCoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed {
                code,
                domain,
                operation,
                detail,
            } => write!(
                f,
                "{code}|{domain}|{operation}|{}",
                detail.as_deref().unwrap_or("")
            ),
        }
    }
}

impl std::error::Error for MobileCoreError {}

impl From<DesktopError> for MobileCoreError {
    fn from(error: DesktopError) -> Self {
        Self::Failed {
            code: error.code_str().to_owned(),
            domain: error.domain().as_str().to_owned(),
            operation: error.operation().to_owned(),
            detail: error.detail().map(str::to_owned),
        }
    }
}

fn failed(code: &str, operation: &str, detail: &str) -> MobileCoreError {
    MobileCoreError::Failed {
        code: code.to_owned(),
        domain: "core".to_owned(),
        operation: operation.to_owned(),
        detail: Some(detail.to_owned()),
    }
}

/// Mirrors UDL `callback interface MobilePlatformRadio`.
pub trait MobilePlatformRadio: Send + Sync {
    fn submit(&self, request: MobileRadioRequest);
    fn cancel(&self, request_id: u64);
}

/// Mirrors UDL `callback interface MobileWakeSink`.
pub trait MobileWakeSink: Send + Sync {
    fn wake(&self, session_id: u64);
}

/// Mirrors UDL `callback interface MobileInvokeCompletion`.
pub trait MobileInvokeCompletion: Send + Sync {
    fn complete(&self, envelope: String);
}

/// Mirrors UDL `dictionary MobileInstance`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileInstance {
    pub peer_id: String,
    pub service_uuid: String,
    pub service_occurrence: u64,
    pub characteristic_uuid: String,
    pub characteristic_occurrence: u64,
}

impl From<&Instance> for MobileInstance {
    fn from(instance: &Instance) -> Self {
        Self {
            peer_id: instance.peer_id.clone(),
            service_uuid: instance.service_uuid.clone(),
            service_occurrence: instance.service_occurrence,
            characteristic_uuid: instance.characteristic_uuid.clone(),
            characteristic_occurrence: instance.characteristic_occurrence,
        }
    }
}

impl From<MobileInstance> for Instance {
    fn from(instance: MobileInstance) -> Self {
        Self {
            peer_id: instance.peer_id,
            service_uuid: instance.service_uuid,
            service_occurrence: instance.service_occurrence,
            characteristic_uuid: instance.characteristic_uuid,
            characteristic_occurrence: instance.characteristic_occurrence,
        }
    }
}

/// Mirrors UDL `[Enum] interface MobileRadioRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileRadioRequest {
    AdapterState {
        id: u64,
    },
    StartScan {
        id: u64,
        service_uuids: Vec<String>,
        device_addresses: Vec<String>,
        scan_mode: Option<String>,
        callback_type: Option<String>,
        legacy: Option<bool>,
    },
    StopScan {
        id: u64,
    },
    Connect {
        id: u64,
        peer_id: String,
        auto_connect: bool,
        /// `le-1m`/`le-2m`/`le-coded`: the PHYs to establish the link on
        /// (empty = no preference). The Apple adapter never receives one
        /// (Rust refuses it before any effect) and fails `unsupported` if
        /// it ever does.
        preferred_phy: Vec<String>,
    },
    Disconnect {
        id: u64,
        peer_id: String,
    },
    Discover {
        id: u64,
        peer_id: String,
    },
    Read {
        id: u64,
        instance: MobileInstance,
    },
    Write {
        id: u64,
        instance: MobileInstance,
        value: Vec<u8>,
        with_response: bool,
    },
    ReadDescriptor {
        id: u64,
        instance: MobileInstance,
        descriptor_uuid: String,
        descriptor_occurrence: u64,
    },
    WriteDescriptor {
        id: u64,
        instance: MobileInstance,
        descriptor_uuid: String,
        descriptor_occurrence: u64,
        value: Vec<u8>,
    },
    EnableNotifications {
        id: u64,
        instance: MobileInstance,
        epoch: u64,
        requested: Option<String>,
        preferred: Option<String>,
    },
    DisableNotifications {
        id: u64,
        instance: MobileInstance,
    },
    ReadMtu {
        id: u64,
        peer_id: String,
    },
    ReadWriteLimits {
        id: u64,
        peer_id: String,
    },
    RequestMtu {
        id: u64,
        peer_id: String,
        mtu: u16,
    },
    ReadRssi {
        id: u64,
        peer_id: String,
    },
    RequestConnectionPriority {
        id: u64,
        peer_id: String,
        priority: String,
    },
    ReadPhy {
        id: u64,
        peer_id: String,
    },
    RequestPhy {
        id: u64,
        peer_id: String,
        tx: Option<String>,
        rx: Option<String>,
    },
    SecurityState {
        id: u64,
        peer_id: String,
    },
    CreateBond {
        id: u64,
        peer_id: String,
        transport: String,
    },
    CancelBond {
        id: u64,
        peer_id: String,
    },
    BondedPeers {
        id: u64,
    },
    AcquireBackground {
        id: u64,
        kind: String,
        reason: String,
    },
    ReleaseBackground {
        id: u64,
        lease_id: String,
    },
    UpdateBackgroundNotification {
        id: u64,
        lease_id: String,
        title: String,
        body: Option<String>,
    },
    AssociateCompanion {
        id: u64,
        name: Option<String>,
        service_uuid: Option<String>,
    },
    Close {
        id: u64,
    },
}

fn mode(mode: Option<DeliveryMode>) -> Option<String> {
    mode.map(|mode| mode.as_str().to_owned())
}

impl From<&RadioRequest> for MobileRadioRequest {
    fn from(request: &RadioRequest) -> Self {
        match request {
            RadioRequest::AdapterState { id } => Self::AdapterState { id: *id },
            RadioRequest::StartScan { id, scan } => {
                let android = scan.android.unwrap_or_default();
                Self::StartScan {
                    id: *id,
                    service_uuids: scan.service_uuids.clone(),
                    device_addresses: scan.device_addresses.clone(),
                    scan_mode: android.mode.map(|mode| mode.as_str().to_owned()),
                    callback_type: android.callback_type.map(|kind| kind.as_str().to_owned()),
                    legacy: android.legacy,
                }
            }
            RadioRequest::StopScan { id } => Self::StopScan { id: *id },
            RadioRequest::Connect {
                id,
                peer_id,
                auto_connect,
                preferred_phy,
            } => Self::Connect {
                id: *id,
                peer_id: peer_id.clone(),
                auto_connect: *auto_connect,
                preferred_phy: preferred_phy
                    .iter()
                    .map(|phy| phy.as_str().to_owned())
                    .collect(),
            },
            RadioRequest::Disconnect { id, peer_id } => Self::Disconnect {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::Discover { id, peer_id } => Self::Discover {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::Read { id, instance } => Self::Read {
                id: *id,
                instance: instance.into(),
            },
            RadioRequest::Write {
                id,
                instance,
                value,
                with_response,
            } => Self::Write {
                id: *id,
                instance: instance.into(),
                value: value.clone(),
                with_response: *with_response,
            },
            RadioRequest::ReadDescriptor { id, descriptor } => Self::ReadDescriptor {
                id: *id,
                instance: (&descriptor.instance).into(),
                descriptor_uuid: descriptor.descriptor_uuid.clone(),
                descriptor_occurrence: descriptor.descriptor_occurrence,
            },
            RadioRequest::WriteDescriptor {
                id,
                descriptor,
                value,
            } => Self::WriteDescriptor {
                id: *id,
                instance: (&descriptor.instance).into(),
                descriptor_uuid: descriptor.descriptor_uuid.clone(),
                descriptor_occurrence: descriptor.descriptor_occurrence,
                value: value.clone(),
            },
            RadioRequest::EnableNotifications {
                id,
                instance,
                epoch,
                requested,
                preferred,
            } => Self::EnableNotifications {
                id: *id,
                instance: instance.into(),
                epoch: *epoch,
                requested: mode(*requested),
                preferred: mode(*preferred),
            },
            RadioRequest::DisableNotifications { id, instance } => Self::DisableNotifications {
                id: *id,
                instance: instance.into(),
            },
            RadioRequest::ReadMtu { id, peer_id } => Self::ReadMtu {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::ReadWriteLimits { id, peer_id } => Self::ReadWriteLimits {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::RequestMtu { id, peer_id, mtu } => Self::RequestMtu {
                id: *id,
                peer_id: peer_id.clone(),
                mtu: *mtu,
            },
            RadioRequest::ReadRssi { id, peer_id } => Self::ReadRssi {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::RequestConnectionPriority {
                id,
                peer_id,
                priority,
            } => Self::RequestConnectionPriority {
                id: *id,
                peer_id: peer_id.clone(),
                priority: priority.as_str().to_owned(),
            },
            RadioRequest::ReadPhy { id, peer_id } => Self::ReadPhy {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::RequestPhy {
                id,
                peer_id,
                tx,
                rx,
            } => Self::RequestPhy {
                id: *id,
                peer_id: peer_id.clone(),
                tx: tx.map(|phy| phy.as_str().to_owned()),
                rx: rx.map(|phy| phy.as_str().to_owned()),
            },
            RadioRequest::SecurityState { id, peer_id } => Self::SecurityState {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::CreateBond {
                id,
                peer_id,
                transport,
            } => Self::CreateBond {
                id: *id,
                peer_id: peer_id.clone(),
                transport: transport.as_str().to_owned(),
            },
            RadioRequest::CancelBond { id, peer_id } => Self::CancelBond {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::BondedPeers { id } => Self::BondedPeers { id: *id },
            RadioRequest::AcquireBackground { id, kind, reason } => Self::AcquireBackground {
                id: *id,
                kind: kind.as_str().to_owned(),
                reason: reason.clone(),
            },
            RadioRequest::ReleaseBackground { id, lease_id } => Self::ReleaseBackground {
                id: *id,
                lease_id: lease_id.clone(),
            },
            RadioRequest::UpdateBackgroundNotification {
                id,
                lease_id,
                title,
                body,
            } => Self::UpdateBackgroundNotification {
                id: *id,
                lease_id: lease_id.clone(),
                title: title.clone(),
                body: body.clone(),
            },
            RadioRequest::AssociateCompanion {
                id,
                name,
                service_uuid,
            } => Self::AssociateCompanion {
                id: *id,
                name: name.clone(),
                service_uuid: service_uuid.clone(),
            },
            RadioRequest::Close { id } => Self::Close { id: *id },
        }
    }
}

/// Mirrors UDL `dictionary MobileAdapterSnapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileAdapterSnapshot {
    pub availability: String,
    pub authorization: String,
    pub power: String,
    pub safe_reason: Option<String>,
}

/// Mirrors UDL `dictionary MobileGattProperties`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobileGattProperties {
    pub read: bool,
    pub write: bool,
    pub write_without_response: bool,
    pub notify: bool,
    pub indicate: bool,
}

/// Mirrors UDL `dictionary MobileGattDescriptor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileGattDescriptor {
    pub uuid: String,
    pub occurrence: u64,
}

/// Mirrors UDL `dictionary MobileGattCharacteristic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileGattCharacteristic {
    pub uuid: String,
    pub occurrence: u64,
    pub properties: MobileGattProperties,
    pub descriptors: Vec<MobileGattDescriptor>,
}

/// Mirrors UDL `dictionary MobileGattService`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileGattService {
    pub uuid: String,
    pub occurrence: u64,
    pub characteristics: Vec<MobileGattCharacteristic>,
}

/// Mirrors UDL `dictionary MobileSecurityState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileSecurityState {
    pub bond: String,
    pub encryption: String,
    pub authentication: String,
    pub secure_connections: String,
    pub pairing_possible: Option<bool>,
}

/// Mirrors UDL `dictionary MobilePeerName`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobilePeerName {
    pub peer_id: String,
    pub name: Option<String>,
}

/// Mirrors UDL `dictionary MobileCloseFailure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileCloseFailure {
    pub instance: MobileInstance,
    pub detail: String,
}

/// Mirrors UDL `[Enum] interface MobileRadioCompletion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileRadioCompletion {
    Unit,
    Bytes {
        value: Vec<u8>,
    },
    Read {
        value: Vec<u8>,
        provenance: String,
    },
    Adapter {
        snapshot: MobileAdapterSnapshot,
    },
    Discovered {
        services: Vec<MobileGattService>,
    },
    NotifyEnabled {
        delivery: String,
    },
    Mtu {
        mtu: Option<u16>,
    },
    WriteLimits {
        with_response: u16,
        without_response: u16,
    },
    Rssi {
        rssi: i16,
    },
    Accepted {
        accepted: bool,
    },
    Phy {
        tx: String,
        rx: String,
    },
    PhyRequest {
        accepted: bool,
        tx: Option<String>,
        rx: Option<String>,
    },
    Security {
        state: MobileSecurityState,
    },
    BondedPeers {
        peers: Vec<MobilePeerName>,
    },
    Lease {
        lease_id: String,
    },
    Companion {
        association_id: i64,
        peer_id: Option<String>,
        display_name: Option<String>,
    },
    Closed {
        failures: Vec<MobileCloseFailure>,
    },
    Failed {
        kind: String,
        gatt_status: Option<i32>,
        /// The `NSError` domain and code CoreBluetooth failed with (113).
        native_domain: Option<String>,
        native_code: Option<i64>,
        detail: String,
        /// `false`: the platform refused before sending anything.
        dispatched: bool,
    },
}

fn adapter(snapshot: MobileAdapterSnapshot) -> Result<AdapterSnapshot, String> {
    Ok(AdapterSnapshot {
        availability: AdapterAvailability::parse(&snapshot.availability)
            .ok_or("unknown availability")?,
        authorization: AdapterAuthorization::parse(&snapshot.authorization)
            .ok_or("unknown authorization")?,
        power: AdapterPower::parse(&snapshot.power).ok_or("unknown power")?,
        safe_reason: snapshot.safe_reason,
    })
}

fn security(state: MobileSecurityState) -> Result<SecurityState, String> {
    Ok(SecurityState {
        bond: BondState::parse(&state.bond).ok_or("unknown bond")?,
        encryption: EncryptionState::parse(&state.encryption).ok_or("unknown encryption")?,
        authentication: AuthenticationState::parse(&state.authentication)
            .ok_or("unknown authentication")?,
        secure_connections: SecureConnectionsState::parse(&state.secure_connections)
            .ok_or("unknown secure connections")?,
        pairing_possible: state.pairing_possible,
    })
}

fn phy(text: &str) -> Result<ubm_mobile::Phy, String> {
    match text {
        "le-1m" => Ok(ubm_mobile::Phy::Le1m),
        "le-2m" => Ok(ubm_mobile::Phy::Le2m),
        "le-coded" => Ok(ubm_mobile::Phy::LeCoded),
        _ => Err(format!("unknown phy {text}")),
    }
}

fn properties(flags: MobileGattProperties) -> PropertyFlags {
    PropertyFlags {
        read: flags.read,
        write: flags.write,
        write_without_response: flags.write_without_response,
        notify: flags.notify,
        indicate: flags.indicate,
    }
}

/// Typed completion from the Swift value; an unreadable value is an error
/// the host turns into a platform failure for that request.
pub fn completion(value: MobileRadioCompletion) -> Result<RadioCompletion, String> {
    Ok(match value {
        MobileRadioCompletion::Unit => RadioCompletion::Unit,
        MobileRadioCompletion::Bytes { value } => RadioCompletion::Bytes(value),
        MobileRadioCompletion::Read { value, provenance } => RadioCompletion::Read {
            value,
            provenance: ReadProvenance::from_wire(&provenance)
                .ok_or_else(|| format!("unknown read provenance {provenance}"))?,
        },
        MobileRadioCompletion::Adapter { snapshot } => RadioCompletion::Adapter(adapter(snapshot)?),
        MobileRadioCompletion::Discovered { services } => RadioCompletion::Discovered(
            services
                .into_iter()
                .map(|service| ServiceSnapshot {
                    uuid: service.uuid,
                    occurrence: service.occurrence,
                    characteristics: service
                        .characteristics
                        .into_iter()
                        .map(|characteristic| CharacteristicSnapshot {
                            uuid: characteristic.uuid,
                            occurrence: characteristic.occurrence,
                            properties: properties(characteristic.properties),
                            descriptors: characteristic
                                .descriptors
                                .into_iter()
                                .map(|descriptor| DescriptorSnapshot {
                                    uuid: descriptor.uuid,
                                    occurrence: descriptor.occurrence,
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        ),
        MobileRadioCompletion::NotifyEnabled { delivery } => {
            RadioCompletion::NotifyEnabled(match delivery.as_str() {
                "notification" => ObservedDelivery::Notification,
                "indication" => ObservedDelivery::Indication,
                "unknown" => ObservedDelivery::Unknown,
                _ => return Err(format!("unknown delivery {delivery}")),
            })
        }
        MobileRadioCompletion::Mtu { mtu } => RadioCompletion::Mtu(mtu),
        MobileRadioCompletion::WriteLimits {
            with_response,
            without_response,
        } => {
            if with_response == 0 || without_response == 0 {
                return Err("write limits must be positive byte counts".to_owned());
            }
            RadioCompletion::WriteLimits(WriteLimits {
                with_response,
                without_response,
            })
        }
        MobileRadioCompletion::Rssi { rssi } => RadioCompletion::Rssi(rssi),
        MobileRadioCompletion::Accepted { accepted } => RadioCompletion::Accepted(accepted),
        MobileRadioCompletion::Phy { tx, rx } => RadioCompletion::Phy(PhyObservation {
            tx: phy(&tx)?,
            rx: phy(&rx)?,
        }),
        MobileRadioCompletion::PhyRequest { accepted, tx, rx } => RadioCompletion::PhyRequest {
            accepted,
            observation: match (tx, rx) {
                (Some(tx), Some(rx)) => Some(PhyObservation {
                    tx: phy(&tx)?,
                    rx: phy(&rx)?,
                }),
                (None, None) => None,
                _ => return Err("tx and rx are both set or both absent".to_owned()),
            },
        },
        MobileRadioCompletion::Security { state } => RadioCompletion::Security(security(state)?),
        MobileRadioCompletion::BondedPeers { peers } => RadioCompletion::BondedPeers(
            peers
                .into_iter()
                .map(|peer| BondedPeer {
                    peer_id: peer.peer_id,
                    name: peer.name,
                })
                .collect(),
        ),
        MobileRadioCompletion::Lease { lease_id } => RadioCompletion::Lease(lease_id),
        MobileRadioCompletion::Companion {
            association_id,
            peer_id,
            display_name,
        } => RadioCompletion::Companion {
            association_id,
            peer_id,
            display_name,
        },
        MobileRadioCompletion::Closed { failures } => RadioCompletion::Closed(
            failures
                .into_iter()
                .map(|failure| CloseFailure {
                    instance: failure.instance.into(),
                    detail: failure.detail,
                })
                .collect(),
        ),
        MobileRadioCompletion::Failed {
            kind,
            gatt_status,
            native_domain,
            native_code,
            detail,
            dispatched,
        } => RadioCompletion::Failed(PlatformFailure {
            kind: FailureKind::parse(&kind)
                .ok_or_else(|| format!("unknown failure kind {kind}"))?,
            gatt_status,
            native_domain: native_domain.filter(|domain| !domain.is_empty()),
            native_code,
            native_name: None,
            detail,
            dispatched,
        }),
    })
}

/// Mirrors UDL `dictionary MobileManufacturerData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileManufacturerData {
    pub company_id: u16,
    pub payload: Vec<u8>,
}

/// Mirrors UDL `dictionary MobileServiceData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileServiceData {
    pub uuid: String,
    pub payload: Vec<u8>,
}

/// Mirrors UDL `dictionary MobileAdvertisement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileAdvertisement {
    pub peer_id: String,
    pub address: Option<String>,
    pub local_name: Option<String>,
    pub rssi: Option<i16>,
    pub tx_power_level: Option<i16>,
    pub service_uuids: Vec<String>,
    pub manufacturer_data: Vec<MobileManufacturerData>,
    pub service_data: Vec<MobileServiceData>,
    pub connectable: Option<bool>,
    pub solicited_service_uuids: Option<Vec<String>>,
    pub overflow_service_uuids: Option<Vec<String>>,
}

/// Mirrors UDL `dictionary MobileRestoredPeer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileRestoredPeer {
    pub peer_id: String,
    pub name: Option<String>,
    pub connected: bool,
}

/// Mirrors UDL `[Enum] interface MobileRadioIngress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileRadioIngress {
    Advertisement {
        advertisement: MobileAdvertisement,
    },
    Connection {
        peer_id: String,
        connected: bool,
        status: Option<i32>,
    },
    ServicesChanged {
        peer_id: String,
    },
    Notification {
        instance: MobileInstance,
        epoch: u64,
        value: Vec<u8>,
    },
    AdapterState {
        snapshot: MobileAdapterSnapshot,
    },
    ScanFailed {
        detail: String,
    },
    SecurityChanged {
        peer_id: String,
        state: MobileSecurityState,
    },
    Restored {
        peers: Vec<MobileRestoredPeer>,
    },
    Dropped {
        ingress_class: String,
        detail: String,
    },
}

fn class_of(ingress: &MobileRadioIngress) -> IngressClass {
    match ingress {
        MobileRadioIngress::Advertisement { .. } => IngressClass::Advertisement,
        MobileRadioIngress::Notification { .. } => IngressClass::Notification,
        _ => IngressClass::Control,
    }
}

/// Typed ingress from the Swift value.
pub fn ingress(value: MobileRadioIngress) -> Result<RadioIngress, String> {
    Ok(match value {
        MobileRadioIngress::Advertisement { advertisement } => {
            RadioIngress::Advertisement(Advertisement {
                peer_id: advertisement.peer_id,
                address: advertisement.address,
                local_name: advertisement.local_name,
                rssi: advertisement.rssi,
                tx_power_level: advertisement.tx_power_level,
                service_uuids: advertisement.service_uuids,
                manufacturer_data: advertisement
                    .manufacturer_data
                    .into_iter()
                    .map(|entry| ManufacturerData {
                        company_id: entry.company_id,
                        payload: entry.payload,
                    })
                    .collect(),
                service_data: advertisement
                    .service_data
                    .into_iter()
                    .map(|entry| ServiceData {
                        uuid: entry.uuid,
                        payload: entry.payload,
                    })
                    .collect(),
                connectable: advertisement.connectable,
                solicited_service_uuids: advertisement.solicited_service_uuids,
                overflow_service_uuids: advertisement.overflow_service_uuids,
                // CoreBluetooth reports neither GAP Appearance nor the raw
                // advertising bytes (legacy iOS never did either).
                appearance: None,
                raw_record: None,
            })
        }
        MobileRadioIngress::Connection {
            peer_id,
            connected,
            status,
        } => RadioIngress::Connection {
            peer_id,
            connected,
            status,
        },
        MobileRadioIngress::ServicesChanged { peer_id } => {
            RadioIngress::ServicesChanged { peer_id }
        }
        MobileRadioIngress::Notification {
            instance,
            epoch,
            value,
        } => RadioIngress::Notification {
            instance: instance.into(),
            epoch,
            value,
        },
        MobileRadioIngress::AdapterState { snapshot } => {
            RadioIngress::AdapterState(adapter(snapshot)?)
        }
        MobileRadioIngress::ScanFailed { detail } => RadioIngress::ScanFailed { detail },
        MobileRadioIngress::SecurityChanged { peer_id, state } => RadioIngress::SecurityChanged {
            peer_id,
            state: security(state)?,
        },
        MobileRadioIngress::Restored { peers } => RadioIngress::Restored {
            peers: peers
                .into_iter()
                .map(|peer| RestoredPeer {
                    peer_id: peer.peer_id,
                    name: peer.name,
                    connected: peer.connected,
                })
                .collect(),
        },
        MobileRadioIngress::Dropped {
            ingress_class,
            detail,
        } => RadioIngress::Dropped {
            class: match ingress_class.as_str() {
                "advertisement" => IngressClass::Advertisement,
                "notification" => IngressClass::Notification,
                "control" => IngressClass::Control,
                _ => return Err(format!("unknown ingress class {ingress_class}")),
            },
            detail,
        },
    })
}

struct ForeignPlatformRadio {
    radio: Box<dyn MobilePlatformRadio>,
}

impl PlatformRadio for ForeignPlatformRadio {
    fn submit(&self, request: RadioRequest) {
        self.radio.submit(MobileRadioRequest::from(&request));
    }

    fn cancel(&self, request_id: u64) {
        self.radio.cancel(request_id);
    }
}

struct ForeignWake {
    wake: Box<dyn MobileWakeSink>,
}

impl WakeSink for ForeignWake {
    fn wake(&self, session_id: u64) {
        self.wake.wake(session_id);
    }
}

fn host_slot() -> &'static Mutex<Option<Arc<MobileCoreHost>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<MobileCoreHost>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn contract_revision() -> &'static str {
    ubm_core::contracts::CONTRACT_REVISION
}

/// UDL `mobile_build_identity_json`.
pub fn mobile_build_identity_json() -> String {
    ubm_build_identity_json(contract_revision())
}

/// UDL `mobile_contract_revision`.
pub fn mobile_contract_revision() -> String {
    contract_revision().to_owned()
}

/// UDL `mobile_wire_revision`.
pub fn mobile_wire_revision() -> String {
    ubm_mobile::WIRE_REVISION.to_owned()
}

/// UDL `mobile_host_install`: the one process host.
pub fn mobile_host_install(
    radio: Box<dyn MobilePlatformRadio>,
    wake: Box<dyn MobileWakeSink>,
    platform: String,
    owner: String,
    adapter_label: String,
) -> Result<Arc<MobileCoreHost>, MobileCoreError> {
    const OP: &str = "mobile.host.install";
    let platform = match platform.as_str() {
        "android" => MobilePlatform::Android,
        "apple" => MobilePlatform::Apple,
        _ => {
            return Err(failed(
                "argument.invalid",
                OP,
                "platform must be android or apple",
            ))
        }
    };
    let mut slot = host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        return Err(failed(
            "lifecycle.invalid-state",
            OP,
            "a mobile host is already installed in this process",
        ));
    }
    let host = MobileHost::open_blocking(
        Arc::new(ForeignPlatformRadio { radio }),
        Arc::new(ForeignWake { wake }),
        HostOptions {
            platform,
            owner,
            adapter_label,
        },
        ubm_desktop::executor::desktop_runtime(),
    )?;
    let handle = Arc::new(MobileCoreHost { host });
    *slot = Some(Arc::clone(&handle));
    Ok(handle)
}

/// UDL `mobile_host_current`.
pub fn mobile_host_current() -> Option<Arc<MobileCoreHost>> {
    host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Mirrors UDL `interface MobileCoreHost`.
pub struct MobileCoreHost {
    host: MobileHost,
}

impl MobileCoreHost {
    pub fn complete(&self, request_id: u64, completion_value: MobileRadioCompletion) -> String {
        let typed = completion(completion_value).unwrap_or_else(|detail| {
            RadioCompletion::Failed(PlatformFailure::new(
                FailureKind::Platform,
                format!("malformed completion from MobilePlatformRadio: {detail}"),
            ))
        });
        match self.host.complete(request_id, typed) {
            CompletionStatus::Delivered => "delivered",
            CompletionStatus::Late => "late",
            CompletionStatus::Mismatched => "mismatched",
        }
        .to_owned()
    }

    pub fn ingest(&self, value: MobileRadioIngress) -> String {
        let class = class_of(&value);
        let typed = ingress(value).unwrap_or_else(|detail| RadioIngress::Dropped {
            class,
            detail: format!("unreadable ingress: {detail}"),
        });
        self.host.ingest(typed).as_str().to_owned()
    }

    pub fn open_session(
        &self,
        owner: String,
        expected_wire_revision: String,
    ) -> Result<Arc<MobileCoreSession>, MobileCoreError> {
        let (session, admission) = self.host.admit_session(
            &owner,
            &expected_wire_revision,
            &mobile_build_identity_json(),
        )?;
        Ok(Arc::new(MobileCoreSession { session, admission }))
    }

    pub fn shutdown(&self) -> String {
        let record = self.host.shutdown_blocking();
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.as_ref(), self))
        {
            *slot = None;
        }
        record
    }
}

/// Mirrors UDL `interface MobileCoreSession`.
pub struct MobileCoreSession {
    session: MobileSession,
    admission: String,
}

impl MobileCoreSession {
    pub fn session_id(&self) -> u64 {
        self.session.id()
    }

    pub fn admission_json(&self) -> String {
        self.admission.clone()
    }

    pub fn invoke(
        &self,
        op: String,
        args_json: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        self.session.invoke(
            &op,
            &args_json,
            Box::new(move |envelope| completion.complete(envelope)),
        );
    }

    pub fn drain(&self, max_items: u32, max_bytes: u32) -> String {
        self.session.drain(max_items, max_bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::Arc;

    use super::*;

    struct Recorder {
        tx: Mutex<mpsc::Sender<MobileRadioRequest>>,
    }

    impl MobilePlatformRadio for Recorder {
        fn submit(&self, request: MobileRadioRequest) {
            if let Ok(tx) = self.tx.lock() {
                let _ = tx.send(request);
            }
        }
        fn cancel(&self, _request_id: u64) {}
    }

    struct NoWake;

    impl MobileWakeSink for NoWake {
        fn wake(&self, _session_id: u64) {}
    }

    struct Capture {
        tx: Mutex<mpsc::Sender<String>>,
    }

    impl MobileInvokeCompletion for Capture {
        fn complete(&self, envelope: String) {
            let _ = self.tx.lock().map(|tx| tx.send(envelope));
        }
    }

    #[test]
    fn udl_mobile_host_answers_through_the_foreign_radio() {
        let (request_tx, request_rx) = mpsc::channel();
        let host = mobile_host_install(
            Box::new(Recorder {
                tx: Mutex::new(request_tx),
            }),
            Box::new(NoWake),
            "apple".to_owned(),
            "uniffi-test".to_owned(),
            "corebluetooth".to_owned(),
        );
        let Ok(host) = host else {
            unreachable!("host installs: {:?}", host.err());
        };
        assert!(mobile_host_install(
            Box::new(Recorder {
                tx: Mutex::new(mpsc::channel().0)
            }),
            Box::new(NoWake),
            "apple".to_owned(),
            "again".to_owned(),
            "corebluetooth".to_owned(),
        )
        .is_err());
        assert!(host
            .open_session("rn".to_owned(), "ubm-mobile-wire/0".to_owned())
            .is_err());
        let Ok(session) = host.open_session("rn".to_owned(), mobile_wire_revision()) else {
            unreachable!("session opens");
        };
        assert!(session.admission_json().contains("\"buildIdentity\""));
        let (envelope_tx, envelope_rx) = mpsc::channel();
        session.invoke(
            "adapter.state".to_owned(),
            "{}".to_owned(),
            Box::new(Capture {
                tx: Mutex::new(envelope_tx),
            }),
        );
        let request = request_rx.recv_timeout(std::time::Duration::from_secs(5));
        let Ok(MobileRadioRequest::AdapterState { id }) = request else {
            unreachable!("adapter request: {request:?}");
        };
        let status = host.complete(
            id,
            MobileRadioCompletion::Adapter {
                snapshot: MobileAdapterSnapshot {
                    availability: "available".to_owned(),
                    authorization: "granted".to_owned(),
                    power: "on".to_owned(),
                    safe_reason: None,
                },
            },
        );
        assert_eq!(status, "delivered");
        let envelope = envelope_rx.recv_timeout(std::time::Duration::from_secs(5));
        let Ok(envelope) = envelope else {
            unreachable!("envelope arrives");
        };
        assert!(envelope.contains("\"power\":\"on\""), "{envelope}");
        assert_eq!(
            host.ingest(MobileRadioIngress::Dropped {
                ingress_class: "bogus".to_owned(),
                detail: String::new(),
            }),
            "dropped-control"
        );
        // Shutdown sends one Close request; answer it from another thread.
        let closer = Arc::clone(&host);
        let answer = std::thread::spawn(move || {
            while let Ok(request) = request_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                if let MobileRadioRequest::Close { id } = request {
                    closer.complete(
                        id,
                        MobileRadioCompletion::Closed {
                            failures: Vec::new(),
                        },
                    );
                    return;
                }
            }
        });
        let record = host.shutdown();
        let _ = answer.join();
        assert!(record.contains("\"released\""), "{record}");
        assert!(mobile_host_current().is_none());
    }

    #[test]
    fn connect_carries_the_preferred_phys() {
        let request = MobileRadioRequest::from(&RadioRequest::Connect {
            id: 4,
            peer_id: "p".to_owned(),
            auto_connect: false,
            preferred_phy: vec![ubm_mobile::Phy::Le2m, ubm_mobile::Phy::LeCoded],
        });
        let MobileRadioRequest::Connect { preferred_phy, .. } = request else {
            unreachable!("connect maps to connect: {request:?}");
        };
        assert_eq!(preferred_phy, ["le-2m", "le-coded"]);
    }

    #[test]
    fn mobile_surface_has_no_staged_path() {
        let source = include_str!("mobile.rs");
        let body = source.split("#[cfg(test)]").next().unwrap_or(source);
        for forbidden in [
            "ubm_fake_radio",
            "StagedDriver",
            "core_backend",
            "EchoSession",
        ] {
            assert!(
                !body.contains(forbidden),
                "{forbidden} reachable from the mobile surface"
            );
        }
    }
}
