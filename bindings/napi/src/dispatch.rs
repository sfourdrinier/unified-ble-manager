//! F01 N-API dispatch: packed Node/desktop consumers execute BLE work in
//! the candidate Rust runtime.
//!
//! [`UbmCentral`] owns one [`DesktopCentral`] over a [`DispatchRadio`]
//! boundary: the production btleplug radio ([`UbmCentral::open`]) or the
//! deterministic synthetic radio ([`UbmCentral::open_synthetic`], the
//! hardware-free CI leg). Every op — scan, connect, discover, read, write,
//! subscribe, notifications, cancel, shutdown — runs inside `ubm-desktop` /
//! `ubm-core`; TypeScript schedules nothing. Failures cross as JS `Error`s
//! whose message carries the frozen `code|domain|operation|detail` wire
//! form, so consumers assert the exact Rust-issued identity.
//!
//! Byte ownership mirrors the echo surface: `Buffer`s are copied into owned
//! `Vec<u8>` on entry and fresh `Buffer`s are built from owned `Vec<u8>` on
//! exit. Rust never retains a borrow of JS memory.
//!
//! The synthetic staging methods (`stage_*`, `block_radio_op`) are the
//! documented input surface of the synthetic radio (a simulator's virtual
//! beacons), not test probes: on the production radio they reject loudly
//! with `capability.unsupported`, never silently.

use napi::bindgen_prelude::{Buffer, Result};
use napi::{Error, Status};
use napi_derive::napi;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CoreError, OperationId};
use ubm_desktop::{
    BtleplugRadio, CompletionOutcome, DesktopCentral, DesktopError, DiscoveredPath, FakeRadio,
    FaultOp, ManufacturerData, PathSelector, PeerSnapshot, PropertyFlags, RadioBoundary,
    RadioCloseFailure, RadioEvent, ScanFilterSpec, ServiceData, ServiceSnapshot,
};

/// Typed dispatch failure carrying a frozen C-UBM identity. [`DesktopError`]
/// and [`CoreError`] identities pass through verbatim; only malformed JS
/// input and synthetic-only staging on the production radio originate here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    code: &'static str,
    domain: &'static str,
    operation: String,
    detail: String,
}

impl DispatchError {
    fn new(
        code: &'static str,
        domain: &'static str,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            domain,
            operation: operation.into(),
            detail: detail.into(),
        }
    }

    fn wire_message(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.code, self.domain, self.operation, self.detail
        )
    }
}

impl From<DesktopError> for DispatchError {
    fn from(error: DesktopError) -> Self {
        Self {
            code: error.code_str(),
            domain: error.domain().as_str(),
            operation: error.operation().to_owned(),
            detail: error.detail().unwrap_or("").to_owned(),
        }
    }
}

impl From<CoreError> for DispatchError {
    fn from(error: CoreError) -> Self {
        Self {
            code: error.code().as_str(),
            domain: error.domain().as_str(),
            operation: error.operation().to_owned(),
            detail: String::new(),
        }
    }
}

fn to_napi(error: DispatchError) -> Error {
    Error::new(Status::GenericFailure, error.wire_message())
}

fn overflow_error(operation: &'static str) -> DispatchError {
    DispatchError::new(
        "lifecycle.invariant-violation",
        "core",
        operation,
        "count-overflow",
    )
}

/// The radio behind a dispatch central: production btleplug or the
/// deterministic synthetic boundary. One enum (not a trait object) so the
/// central stays a concrete `Send + Sync + 'static` type.
enum DispatchRadio {
    // Boxed: both radios carry hundreds of bytes of queues/handles and the
    // central holds exactly one boundary, so indirection is free and the
    // enum stays pointer-sized (clippy::large-enum-variant).
    Radio(Box<BtleplugRadio>),
    Synthetic(Box<FakeRadio>),
}

impl DispatchRadio {
    /// The synthetic radio, or a loud rejection on the production radio:
    /// staging methods never touch real hardware, silently or otherwise.
    fn synthetic(&self, operation: &'static str) -> std::result::Result<&FakeRadio, DispatchError> {
        match self {
            Self::Synthetic(radio) => Ok(radio.as_ref()),
            Self::Radio(_) => Err(DispatchError::new(
                BleErrorCode::CapabilityUnsupported.as_str(),
                BleErrorDomain::Capability.as_str(),
                operation,
                "synthetic-only staging on the production radio",
            )),
        }
    }
}

// `async fn` satisfies the trait's `-> impl Future` seams; each arm's
// future is `Send`, so the combined future is too.
impl RadioBoundary for DispatchRadio {
    async fn adapter_name(&self) -> std::result::Result<String, DesktopError> {
        match self {
            Self::Radio(radio) => radio.adapter_name().await,
            Self::Synthetic(radio) => radio.adapter_name().await,
        }
    }

    async fn start_scan(&self, filter: ScanFilterSpec) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => radio.start_scan(filter).await,
            Self::Synthetic(radio) => radio.start_scan(filter).await,
        }
    }

    async fn stop_scan(&self) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => radio.stop_scan().await,
            Self::Synthetic(radio) => radio.stop_scan().await,
        }
    }

    async fn peers(&self) -> std::result::Result<Vec<PeerSnapshot>, DesktopError> {
        match self {
            Self::Radio(radio) => radio.peers().await,
            Self::Synthetic(radio) => radio.peers().await,
        }
    }

    async fn connect(&self, peer_id: &str) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => radio.connect(peer_id).await,
            Self::Synthetic(radio) => radio.connect(peer_id).await,
        }
    }

    async fn disconnect(&self, peer_id: &str) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => radio.disconnect(peer_id).await,
            Self::Synthetic(radio) => radio.disconnect(peer_id).await,
        }
    }

    async fn is_connected(&self, peer_id: &str) -> std::result::Result<bool, DesktopError> {
        match self {
            Self::Radio(radio) => radio.is_connected(peer_id).await,
            Self::Synthetic(radio) => radio.is_connected(peer_id).await,
        }
    }

    async fn discover(
        &self,
        peer_id: &str,
    ) -> std::result::Result<Vec<ServiceSnapshot>, DesktopError> {
        match self {
            Self::Radio(radio) => radio.discover(peer_id).await,
            Self::Synthetic(radio) => radio.discover(peer_id).await,
        }
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> std::result::Result<Vec<u8>, DesktopError> {
        match self {
            Self::Radio(radio) => {
                radio
                    .read_characteristic(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                    )
                    .await
            }
            Self::Synthetic(radio) => {
                radio
                    .read_characteristic(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                    )
                    .await
            }
        }
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
    ) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => {
                radio
                    .write_characteristic(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        value,
                        with_response,
                    )
                    .await
            }
            Self::Synthetic(radio) => {
                radio
                    .write_characteristic(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        value,
                        with_response,
                    )
                    .await
            }
        }
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
    ) -> std::result::Result<Vec<u8>, DesktopError> {
        match self {
            Self::Radio(radio) => {
                radio
                    .read_descriptor(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        descriptor_uuid,
                        descriptor_occurrence,
                    )
                    .await
            }
            Self::Synthetic(radio) => {
                radio
                    .read_descriptor(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        descriptor_uuid,
                        descriptor_occurrence,
                    )
                    .await
            }
        }
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
    ) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => {
                radio
                    .write_descriptor(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        descriptor_uuid,
                        descriptor_occurrence,
                        value,
                    )
                    .await
            }
            Self::Synthetic(radio) => {
                radio
                    .write_descriptor(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        descriptor_uuid,
                        descriptor_occurrence,
                        value,
                    )
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_notifications(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
    ) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => {
                radio
                    .set_notifications(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        enable,
                        epoch,
                    )
                    .await
            }
            Self::Synthetic(radio) => {
                radio
                    .set_notifications(
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        enable,
                        epoch,
                    )
                    .await
            }
        }
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        match self {
            Self::Radio(radio) => radio.mtu(peer_id).await,
            Self::Synthetic(radio) => radio.mtu(peer_id).await,
        }
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        match self {
            Self::Radio(radio) => radio.next_event().await,
            Self::Synthetic(radio) => radio.next_event().await,
        }
    }

    async fn close(&self) {
        match self {
            Self::Radio(radio) => radio.close().await,
            Self::Synthetic(radio) => radio.close().await,
        }
    }

    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        match self {
            Self::Radio(radio) => radio.take_close_failures(),
            Self::Synthetic(radio) => radio.take_close_failures(),
        }
    }
}

/// Counts cross as `u32` (exact in JS); occurrences never approach the
/// ceiling, and the conversion fails closed instead of wrapping.
fn count_wire(value: u64, operation: &'static str) -> std::result::Result<u32, DispatchError> {
    u32::try_from(value).map_err(|_| overflow_error(operation))
}

/// `scan.start` arguments.
#[napi(object)]
pub struct ScanOptions {
    pub owner: String,
    pub service_uuids: Option<Vec<String>>,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: u32,
}

/// Live scan session: the core op id backing it.
#[napi(object)]
pub struct ScanSessionInfo {
    #[napi(js_name = "operationId")]
    pub operation_id: String,
}

/// One manufacturer-data section, payload bytes verbatim.
#[napi(object)]
pub struct ManufacturerDataInfo {
    #[napi(js_name = "companyId")]
    pub company_id: u32,
    pub payload: Buffer,
}

/// One service-data section, payload bytes verbatim.
#[napi(object)]
pub struct ServiceDataInfo {
    pub uuid: String,
    pub payload: Buffer,
}

/// One queued scan observation (F22 fact set, verbatim).
#[napi(object)]
pub struct AdvertisementInfo {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub address: Option<String>,
    pub rssi: Option<i32>,
    #[napi(js_name = "localName")]
    pub local_name: Option<String>,
    #[napi(js_name = "serviceUuids")]
    pub service_uuids: Vec<String>,
    #[napi(js_name = "manufacturerData")]
    pub manufacturer_data: Vec<ManufacturerDataInfo>,
    #[napi(js_name = "serviceData")]
    pub service_data: Vec<ServiceDataInfo>,
    #[napi(js_name = "txPower")]
    pub tx_power: Option<i32>,
}

fn advertisement_info(snapshot: &PeerSnapshot) -> AdvertisementInfo {
    AdvertisementInfo {
        peer_id: snapshot.id.clone(),
        address: snapshot.address.clone(),
        rssi: snapshot.rssi.map(i32::from),
        local_name: snapshot.local_name.clone(),
        service_uuids: snapshot.service_uuids.clone(),
        manufacturer_data: snapshot
            .manufacturer_data
            .iter()
            .map(|section| ManufacturerDataInfo {
                company_id: u32::from(section.company_id),
                payload: Buffer::from(section.payload.clone()),
            })
            .collect(),
        service_data: snapshot
            .service_data
            .iter()
            .map(|section| ServiceDataInfo {
                uuid: section.uuid.clone(),
                payload: Buffer::from(section.payload.clone()),
            })
            .collect(),
        tx_power: snapshot.tx_power_level.map(i32::from),
    }
}

/// `connection.connect` arguments.
#[napi(object)]
pub struct ConnectOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: u32,
}

/// Connection handle: session peer key plus core generation.
#[napi(object)]
pub struct ConnectionInfo {
    #[napi(js_name = "peerKey")]
    pub peer_key: String,
    #[napi(js_name = "connectionGeneration")]
    pub connection_generation: Option<String>,
}

/// `connection.disconnect` arguments.
#[napi(object)]
pub struct DisconnectOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
}

/// `discovery.complete` arguments.
#[napi(object)]
pub struct DiscoverOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
}

/// One skipped discovery entry: identity plus frozen skip code.
#[napi(object)]
pub struct SkippedEntryInfo {
    pub uuid: String,
    pub code: String,
}

/// Discovery report: registered count plus skips in discovery order.
#[napi(object)]
pub struct DiscoveryInfo {
    #[napi(js_name = "pathsRegistered")]
    pub paths_registered: u32,
    pub skipped: Vec<SkippedEntryInfo>,
}

/// One registered discovery path in the current snapshot.
#[napi(object)]
pub struct PathInfo {
    #[napi(js_name = "serviceUuid")]
    pub service_uuid: String,
    #[napi(js_name = "serviceOccurrence")]
    pub service_occurrence: u32,
    #[napi(js_name = "characteristicUuid")]
    pub characteristic_uuid: Option<String>,
    #[napi(js_name = "characteristicOccurrence")]
    pub characteristic_occurrence: Option<u32>,
    #[napi(js_name = "descriptorUuid")]
    pub descriptor_uuid: Option<String>,
    #[napi(js_name = "descriptorOccurrence")]
    pub descriptor_occurrence: Option<u32>,
    pub properties: u32,
}

fn path_info(
    path: &DiscoveredPath,
    operation: &'static str,
) -> std::result::Result<PathInfo, DispatchError> {
    let characteristic_occurrence = path
        .characteristic_occurrence
        .map(|value| count_wire(value, operation))
        .transpose()?;
    let descriptor_occurrence = path
        .descriptor_occurrence
        .map(|value| count_wire(value, operation))
        .transpose()?;
    Ok(PathInfo {
        service_uuid: path.service_uuid.clone(),
        service_occurrence: count_wire(path.service_occurrence, operation)?,
        characteristic_uuid: path.characteristic_uuid.clone(),
        characteristic_occurrence,
        descriptor_uuid: path.descriptor_uuid.clone(),
        descriptor_occurrence,
        properties: u32::from(path.properties),
    })
}

/// Path selector input. UUIDs are canonicalized by
/// [`DesktopCentral::selector`]; occurrences select among duplicates.
#[napi(object)]
pub struct SelectorInput {
    #[napi(js_name = "serviceUuid")]
    pub service_uuid: String,
    #[napi(js_name = "serviceOccurrence")]
    pub service_occurrence: Option<u32>,
    #[napi(js_name = "characteristicUuid")]
    pub characteristic_uuid: Option<String>,
    #[napi(js_name = "characteristicOccurrence")]
    pub characteristic_occurrence: Option<u32>,
    #[napi(js_name = "descriptorUuid")]
    pub descriptor_uuid: Option<String>,
    #[napi(js_name = "descriptorOccurrence")]
    pub descriptor_occurrence: Option<u32>,
}

fn selector_of(input: &SelectorInput) -> std::result::Result<PathSelector, DispatchError> {
    DesktopCentral::<DispatchRadio>::selector(
        &input.service_uuid,
        input.service_occurrence.map(u64::from),
        input.characteristic_uuid.as_deref(),
        input.characteristic_occurrence.map(u64::from),
        input.descriptor_uuid.as_deref(),
        input.descriptor_occurrence.map(u64::from),
    )
    .map_err(DispatchError::from)
}

/// `gatt.read` arguments.
#[napi(object)]
pub struct ReadOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: u32,
}

/// `gatt.write` arguments. `mode` is `with-response` (default) or
/// `without-response`.
#[napi(object)]
pub struct WriteOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub value: Buffer,
    pub mode: Option<String>,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: u32,
}

/// `gatt.write-descriptor` arguments (descriptors take no write mode).
#[napi(object)]
pub struct WriteDescriptorOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub value: Buffer,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: u32,
}

fn write_mode(mode: Option<&str>) -> std::result::Result<&'static str, DispatchError> {
    match mode {
        None | Some("with-response") => Ok("with-response"),
        Some("without-response") => Ok("without-response"),
        Some(other) => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.write-mode",
            format!("unknown write mode {other:?}"),
        )),
    }
}

/// `gatt.subscribe` arguments.
#[napi(object)]
pub struct SubscribeOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub consumer: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: u32,
}

/// Subscription identity arguments (take/unsubscribe).
#[napi(object)]
pub struct SubscriptionOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub consumer: String,
}

/// Cancel outcome: the winning terminal, never a guess.
#[napi(object)]
pub struct CancelInfo {
    pub outcome: String,
    pub kind: Option<String>,
    pub cause: Option<String>,
    #[napi(js_name = "reachedRadio")]
    pub reached_radio: Option<bool>,
}

fn cancel_info(outcome: &CompletionOutcome) -> CancelInfo {
    match outcome {
        CompletionOutcome::Settled {
            kind,
            cause,
            reached_radio,
            ..
        } => CancelInfo {
            outcome: "settled".to_owned(),
            kind: Some(kind.as_str().to_owned()),
            cause: cause.map(|code| code.as_str().to_owned()),
            reached_radio: Some(*reached_radio),
        },
        CompletionOutcome::DuplicateSuppressed { .. } => CancelInfo {
            outcome: "duplicate-suppressed".to_owned(),
            kind: None,
            cause: None,
            reached_radio: None,
        },
        CompletionOutcome::ContenderIgnored => CancelInfo {
            outcome: "contender-ignored".to_owned(),
            kind: None,
            cause: None,
            reached_radio: None,
        },
    }
}

/// Synthetic advertisement staging (every fact optional but the peer).
#[napi(object)]
pub struct StageAdvertisementInput {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub address: Option<String>,
    pub rssi: Option<i32>,
    #[napi(js_name = "localName")]
    pub local_name: Option<String>,
    #[napi(js_name = "serviceUuids")]
    pub service_uuids: Option<Vec<String>>,
    #[napi(js_name = "manufacturerData")]
    pub manufacturer_data: Option<Vec<ManufacturerDataInfo>>,
    #[napi(js_name = "serviceData")]
    pub service_data: Option<Vec<ServiceDataInfo>>,
    #[napi(js_name = "txPower")]
    pub tx_power: Option<i32>,
}

fn staged_snapshot(
    input: &StageAdvertisementInput,
) -> std::result::Result<PeerSnapshot, DispatchError> {
    const OP: &str = "dispatch.stage-advertisement";
    let rssi = input
        .rssi
        .map(|value| {
            i16::try_from(value).map_err(|_| {
                DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    OP,
                    "rssi out of range",
                )
            })
        })
        .transpose()?;
    let tx_power = input
        .tx_power
        .map(|value| {
            i16::try_from(value).map_err(|_| {
                DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    OP,
                    "tx-power out of range",
                )
            })
        })
        .transpose()?;
    let mut manufacturer_data = Vec::new();
    for section in input.manufacturer_data.as_deref().unwrap_or(&[]) {
        let company_id = u16::try_from(section.company_id).map_err(|_| {
            DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                OP,
                "company-id out of range",
            )
        })?;
        manufacturer_data.push(ManufacturerData {
            company_id,
            payload: section.payload.as_ref().to_vec(),
        });
    }
    let service_data = input
        .service_data
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|section| ServiceData {
            uuid: section.uuid.clone(),
            payload: section.payload.as_ref().to_vec(),
        })
        .collect();
    Ok(PeerSnapshot {
        id: input.peer_id.clone(),
        address: input.address.clone(),
        service_uuids: input.service_uuids.clone().unwrap_or_default(),
        rssi,
        local_name: input.local_name.clone(),
        manufacturer_data,
        service_data,
        tx_power_level: tx_power,
    })
}

/// Synthetic staged descriptor.
#[napi(object)]
pub struct StageDescriptor {
    pub uuid: String,
    pub occurrence: u32,
}

/// Synthetic staged property flags.
#[napi(object)]
pub struct StageProperties {
    pub read: bool,
    pub write: bool,
    #[napi(js_name = "writeWithoutResponse")]
    pub write_without_response: bool,
    pub notify: bool,
    pub indicate: bool,
}

/// Synthetic staged characteristic.
#[napi(object)]
pub struct StageCharacteristic {
    pub uuid: String,
    pub occurrence: u32,
    pub properties: StageProperties,
    pub descriptors: Vec<StageDescriptor>,
}

/// Synthetic staged service.
#[napi(object)]
pub struct StageService {
    pub uuid: String,
    pub occurrence: u32,
    pub characteristics: Vec<StageCharacteristic>,
}

fn staged_services(services: &[StageService]) -> Vec<ServiceSnapshot> {
    services
        .iter()
        .map(|service| ServiceSnapshot {
            uuid: service.uuid.clone(),
            occurrence: u64::from(service.occurrence),
            characteristics: service
                .characteristics
                .iter()
                .map(|characteristic| ubm_desktop::CharacteristicSnapshot {
                    uuid: characteristic.uuid.clone(),
                    occurrence: u64::from(characteristic.occurrence),
                    properties: PropertyFlags {
                        read: characteristic.properties.read,
                        write: characteristic.properties.write,
                        write_without_response: characteristic.properties.write_without_response,
                        notify: characteristic.properties.notify,
                        indicate: characteristic.properties.indicate,
                    },
                    descriptors: characteristic
                        .descriptors
                        .iter()
                        .map(|descriptor| ubm_desktop::DescriptorSnapshot {
                            uuid: descriptor.uuid.clone(),
                            occurrence: u64::from(descriptor.occurrence),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

/// Synthetic notification staging. `epoch` defaults to the fresh-routing
/// epoch 0 (F10); pass an explicit epoch only to stage stale routing.
#[napi(object)]
pub struct StageNotificationInput {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    #[napi(js_name = "serviceUuid")]
    pub service_uuid: String,
    #[napi(js_name = "serviceOccurrence")]
    pub service_occurrence: Option<u32>,
    #[napi(js_name = "characteristicUuid")]
    pub characteristic_uuid: String,
    #[napi(js_name = "characteristicOccurrence")]
    pub characteristic_occurrence: Option<u32>,
    pub epoch: Option<u32>,
    pub value: Buffer,
}

/// Parse a radio-op name for synthetic flow control.
fn fault_op(name: &str) -> std::result::Result<FaultOp, DispatchError> {
    match name {
        "start-scan" => Ok(FaultOp::StartScan),
        "stop-scan" => Ok(FaultOp::StopScan),
        "connect" => Ok(FaultOp::Connect),
        "disconnect" => Ok(FaultOp::Disconnect),
        "discover" => Ok(FaultOp::Discover),
        "read" => Ok(FaultOp::Read),
        "write" => Ok(FaultOp::Write),
        "subscribe" => Ok(FaultOp::Subscribe),
        "unsubscribe" => Ok(FaultOp::Unsubscribe),
        "mtu" => Ok(FaultOp::Mtu),
        "adapter-name" => Ok(FaultOp::AdapterName),
        _ => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.radio-op",
            format!("unknown radio op {name:?}"),
        )),
    }
}

/// One dispatch central: a [`DesktopCentral`] whose ops execute in Rust.
/// Open with [`UbmCentral::open`] (production radio) or
/// [`UbmCentral::open_synthetic`] (deterministic synthetic radio); release
/// with [`UbmCentral::close`], which is idempotent.
#[napi]
pub struct UbmCentral {
    central: DesktopCentral<DispatchRadio>,
}

#[napi]
impl UbmCentral {
    /// Open on the production radio. Without usable hardware this rejects
    /// with `adapter.unavailable`, never a silent or synthetic fallback:
    /// the caller chose the production backend explicitly.
    #[napi(factory, catch_unwind)]
    pub async fn open(owner: String) -> Result<Self> {
        if owner.is_empty() {
            return Err(to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.open",
                "owner must not be empty",
            )));
        }
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            to_napi(DispatchError::new(
                "lifecycle.invariant-violation",
                "core",
                "dispatch.open",
                "no tokio runtime",
            ))
        })?;
        let radio = BtleplugRadio::open(handle, None)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        let central = DesktopCentral::open(DispatchRadio::Radio(Box::new(radio)), &owner)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(Self { central })
    }

    /// Open on the deterministic synthetic radio (hardware-free CI leg).
    /// Staging methods feed it; every op still executes the production
    /// `DesktopCentral` path (admission, core transitions, deadlines).
    #[napi(factory, catch_unwind)]
    pub async fn open_synthetic(owner: String) -> Result<Self> {
        if owner.is_empty() {
            return Err(to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.open-synthetic",
                "owner must not be empty",
            )));
        }
        let central =
            DesktopCentral::open(DispatchRadio::Synthetic(Box::new(FakeRadio::new())), &owner)
                .await
                .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(Self { central })
    }

    /// Start a scan; the session carries the backing core op id.
    #[napi(catch_unwind)]
    pub async fn start_scan(&self, options: ScanOptions) -> Result<ScanSessionInfo> {
        let service_uuids = options.service_uuids.unwrap_or_default();
        let refs: Vec<&str> = service_uuids.iter().map(String::as_str).collect();
        let session = self
            .central
            .start_scan(&options.owner, &refs, u64::from(options.timeout_ms))
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(ScanSessionInfo {
            operation_id: session.operation_id().as_str().to_owned(),
        })
    }

    /// Stop the live scan.
    #[napi(catch_unwind)]
    pub async fn stop_scan(&self) -> Result<()> {
        self.central
            .stop_scan()
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))
    }

    /// Take one queued observation (`null` when the queue is empty).
    #[napi(catch_unwind)]
    pub async fn take_advertisement(&self) -> Result<Option<AdvertisementInfo>> {
        Ok(self
            .central
            .take_advertisement()
            .await
            .map(|snapshot| advertisement_info(&snapshot)))
    }

    /// Connect to a known peer.
    #[napi(catch_unwind)]
    pub async fn connect(&self, options: ConnectOptions) -> Result<ConnectionInfo> {
        let handle = self
            .central
            .connect(
                &options.peer_id,
                &options.lease,
                u64::from(options.timeout_ms),
            )
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(ConnectionInfo {
            peer_key: handle.peer_key,
            connection_generation: handle.connection_generation,
        })
    }

    /// Disconnect a connected peer.
    #[napi(catch_unwind)]
    pub async fn disconnect(&self, options: DisconnectOptions) -> Result<()> {
        self.central
            .disconnect(&options.peer_id, &options.lease)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))
    }

    /// Discover the peer database; returns the registration report.
    #[napi(catch_unwind)]
    pub async fn discover(&self, options: DiscoverOptions) -> Result<DiscoveryInfo> {
        let report = self
            .central
            .discover(&options.peer_id, &options.lease)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        let paths_registered =
            count_wire(report.paths_registered as u64, "dispatch.discover").map_err(to_napi)?;
        Ok(DiscoveryInfo {
            paths_registered,
            skipped: report
                .skipped
                .iter()
                .map(|(uuid, code)| SkippedEntryInfo {
                    uuid: uuid.clone(),
                    code: code.clone(),
                })
                .collect(),
        })
    }

    /// Read the peer's current discovery tree in registration order.
    #[napi(catch_unwind)]
    pub async fn discovered_paths(&self, peer_id: String) -> Result<Vec<PathInfo>> {
        let paths = self
            .central
            .discovered_paths(&peer_id)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        paths
            .iter()
            .map(|path| path_info(path, "dispatch.discovered-paths"))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(to_napi)
    }

    /// GATT read through a validated path.
    #[napi(catch_unwind)]
    pub async fn read(&self, options: ReadOptions) -> Result<Buffer> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let value = self
            .central
            .read(&options.peer_id, &selector, u64::from(options.timeout_ms))
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(Buffer::from(value))
    }

    /// GATT write through a validated path.
    #[napi(catch_unwind)]
    pub async fn write(&self, options: WriteOptions) -> Result<()> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let mode = write_mode(options.mode.as_deref()).map_err(to_napi)?;
        self.central
            .write(
                &options.peer_id,
                &selector,
                options.value.as_ref().to_vec(),
                mode,
                u64::from(options.timeout_ms),
            )
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))
    }

    /// GATT descriptor read through a validated path.
    #[napi(catch_unwind)]
    pub async fn read_descriptor(&self, options: ReadOptions) -> Result<Buffer> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let value = self
            .central
            .read_descriptor(&options.peer_id, &selector, u64::from(options.timeout_ms))
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(Buffer::from(value))
    }

    /// GATT descriptor write through a validated path.
    #[napi(catch_unwind)]
    pub async fn write_descriptor(&self, options: WriteDescriptorOptions) -> Result<()> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        self.central
            .write_descriptor(
                &options.peer_id,
                &selector,
                options.value.as_ref().to_vec(),
                u64::from(options.timeout_ms),
            )
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))
    }

    /// Subscribe one consumer; the physical CCCD enable is owned by Rust.
    #[napi(catch_unwind)]
    pub async fn subscribe(&self, options: SubscribeOptions) -> Result<()> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        self.central
            .subscribe(
                &options.peer_id,
                &selector,
                &options.consumer,
                u64::from(options.timeout_ms),
            )
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))
    }

    /// Take one queued notification value (`null` when none is queued).
    #[napi(catch_unwind)]
    pub async fn take_notification(&self, options: SubscriptionOptions) -> Result<Option<Buffer>> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let value = self
            .central
            .take_notification(&options.peer_id, &selector, &options.consumer)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(value.map(Buffer::from))
    }

    /// Unsubscribe one consumer; returns whether the physical CCCD was
    /// disabled (true when the last consumer leaves).
    #[napi(catch_unwind)]
    pub async fn unsubscribe(&self, options: SubscriptionOptions) -> Result<bool> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        self.central
            .unsubscribe(&options.peer_id, &selector, &options.consumer)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))
    }

    /// Cancel one live operation by core op id; returns the winning
    /// terminal, never a guess.
    #[napi(catch_unwind)]
    pub async fn cancel_operation(&self, operation_id: String) -> Result<CancelInfo> {
        let id =
            OperationId::new(operation_id).map_err(|error| to_napi(DispatchError::from(error)))?;
        let outcome = self
            .central
            .cancel_operation(&id)
            .await
            .map_err(|error| to_napi(DispatchError::from(error)))?;
        Ok(cancel_info(&outcome))
    }

    /// Shut the central down: stops the scan, releases ops, closes the
    /// radio. Idempotent; later ops refuse loudly.
    #[napi(catch_unwind)]
    pub async fn close(&self) -> Result<()> {
        self.central.shutdown().await;
        Ok(())
    }

    /// Stage one synthetic advertisement (synthetic radio only).
    #[napi(catch_unwind)]
    pub async fn stage_advertisement(&self, input: StageAdvertisementInput) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-advertisement")
            .map_err(to_napi)?;
        let snapshot = staged_snapshot(&input).map_err(to_napi)?;
        radio.push_event(RadioEvent::Advertisement(snapshot));
        Ok(())
    }

    /// Stage the synthetic GATT database for one peer (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_services(&self, peer_id: String, services: Vec<StageService>) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-services")
            .map_err(to_napi)?;
        radio.set_services(&peer_id, staged_services(&services));
        Ok(())
    }

    /// Stage the synthetic OS-measured ATT MTU for one peer (synthetic
    /// only): writes fail closed without a measured maximum, never guess.
    #[napi(catch_unwind)]
    pub async fn stage_mtu(&self, peer_id: String, mtu: u32) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-mtu")
            .map_err(to_napi)?;
        let mtu = u16::try_from(mtu).map_err(|_| {
            to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.stage-mtu",
                "mtu out of range",
            ))
        })?;
        radio.set_mtu(&peer_id, mtu);
        Ok(())
    }

    /// Stage one synthetic notification (synthetic radio only).
    #[napi(catch_unwind)]
    pub async fn stage_notification(&self, input: StageNotificationInput) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-notification")
            .map_err(to_napi)?;
        radio.push_event(RadioEvent::Notification {
            peer_id: input.peer_id,
            service_uuid: input.service_uuid,
            service_occurrence: u64::from(input.service_occurrence.unwrap_or(0)),
            characteristic_uuid: input.characteristic_uuid,
            characteristic_occurrence: u64::from(input.characteristic_occurrence.unwrap_or(0)),
            epoch: u64::from(input.epoch.unwrap_or(0)),
            value: input.value.as_ref().to_vec(),
        });
        Ok(())
    }

    /// Park one synthetic radio op until unblocked (synthetic only):
    /// deterministic deadline/cancel proofs.
    #[napi(catch_unwind)]
    pub async fn block_radio_op(&self, op: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.block-radio-op")
            .map_err(to_napi)?;
        let parsed = fault_op(&op).map_err(to_napi)?;
        radio.block_op(parsed);
        Ok(())
    }

    /// Release a parked synthetic radio op (synthetic only).
    #[napi(catch_unwind)]
    pub async fn unblock_radio_op(&self, op: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.unblock-radio-op")
            .map_err(to_napi)?;
        let parsed = fault_op(&op).map_err(to_napi)?;
        radio.unblock_op(parsed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ubm_desktop::{CharacteristicSnapshot, DescriptorSnapshot};

    use super::*;

    // N-API symbol stubs for `cargo test`: no Node runtime links the unit
    // test binary, so the handful of N-API symbols the compiled binding
    // references must resolve to something. The dispatch tests never invoke
    // an N-API function (Vec-backed `Buffer`s, constructed-never-thrown
    // errors, direct async calls), so every stub panics: a call is a test
    // bug, loud by construction. If a future binding change references a
    // new symbol, the test link fails naming it — stub it here the same way.
    #[no_mangle]
    pub extern "C" fn napi_create_error(
        _env: *mut std::ffi::c_void,
        _code: *mut std::ffi::c_void,
        _msg: *mut std::ffi::c_void,
        _result: *mut *mut std::ffi::c_void,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_create_error");
    }

    #[no_mangle]
    pub extern "C" fn napi_throw(
        _env: *mut std::ffi::c_void,
        _error: *mut std::ffi::c_void,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_throw");
    }

    #[no_mangle]
    pub extern "C" fn napi_is_exception_pending(
        _env: *mut std::ffi::c_void,
        _result: *mut bool,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_is_exception_pending");
    }

    #[no_mangle]
    pub extern "C" fn napi_get_and_clear_last_exception(
        _env: *mut std::ffi::c_void,
        _result: *mut *mut std::ffi::c_void,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_get_and_clear_last_exception");
    }

    #[no_mangle]
    pub extern "C" fn napi_call_threadsafe_function(
        _func: *mut std::ffi::c_void,
        _data: *mut std::ffi::c_void,
        _is_blocking: i32,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_call_threadsafe_function");
    }

    #[no_mangle]
    pub extern "C" fn napi_reference_unref(
        _env: *mut std::ffi::c_void,
        _ref: *mut std::ffi::c_void,
        _result: *mut u32,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_reference_unref");
    }

    #[no_mangle]
    pub extern "C" fn napi_delete_reference(
        _env: *mut std::ffi::c_void,
        _ref: *mut std::ffi::c_void,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_delete_reference");
    }

    #[no_mangle]
    pub extern "C" fn napi_get_reference_value(
        _env: *mut std::ffi::c_void,
        _ref: *mut std::ffi::c_void,
        _result: *mut *mut std::ffi::c_void,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_get_reference_value");
    }

    #[no_mangle]
    pub extern "C" fn napi_is_error(
        _env: *mut std::ffi::c_void,
        _value: *mut std::ffi::c_void,
        _result: *mut bool,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_is_error");
    }

    #[no_mangle]
    pub extern "C" fn napi_create_string_utf8(
        _env: *mut std::ffi::c_void,
        _str: *const std::ffi::c_char,
        _length: usize,
        _result: *mut *mut std::ffi::c_void,
    ) -> i32 {
        panic!("napi stub invoked in unit test: napi_create_string_utf8");
    }

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

    #[test]
    fn dispatch_error_wire_form_is_frozen() {
        let error = DispatchError::new("gatt.read-failed", "gatt", "dispatch.read", "os-eio");
        assert_eq!(
            error.wire_message(),
            "gatt.read-failed|gatt|dispatch.read|os-eio"
        );
    }

    #[test]
    fn desktop_error_identity_passes_through_verbatim() {
        let error = DispatchError::from(DesktopError::new(
            BleErrorCode::PeerNotFound,
            BleErrorDomain::Connection,
            "peer.known",
        ));
        assert_eq!(
            error.wire_message(),
            "peer.not-found|connection|peer.known|"
        );
    }

    #[test]
    fn write_mode_defaults_and_rejects_loudly() {
        assert_eq!(write_mode(None).expect("default"), "with-response");
        assert_eq!(
            write_mode(Some("without-response")).expect("writes"),
            "without-response"
        );
        let error = write_mode(Some("long-write")).expect_err("unknown mode");
        assert!(error.wire_message().starts_with("argument.invalid|core|"));
    }

    #[test]
    fn fault_op_parses_every_gate_and_rejects_loudly() {
        for name in [
            "start-scan",
            "stop-scan",
            "connect",
            "disconnect",
            "discover",
            "read",
            "write",
            "subscribe",
            "unsubscribe",
            "mtu",
            "adapter-name",
        ] {
            assert!(fault_op(name).is_ok(), "{name}");
        }
        let error = fault_op("launch").expect_err("unknown op");
        assert!(error.wire_message().starts_with("argument.invalid|core|"));
    }

    #[test]
    fn selector_rejects_malformed_uuids() {
        let input = SelectorInput {
            service_uuid: "not-a-uuid".to_owned(),
            service_occurrence: None,
            characteristic_uuid: None,
            characteristic_occurrence: None,
            descriptor_uuid: None,
            descriptor_occurrence: None,
        };
        let error = selector_of(&input).expect_err("malformed uuid");
        assert!(error.wire_message().starts_with("argument.invalid|"));
    }

    #[test]
    fn advertisement_mapping_preserves_nulls_and_bytes() {
        let snapshot = PeerSnapshot {
            id: "peer-1".to_owned(),
            address: None,
            service_uuids: Vec::new(),
            rssi: None,
            local_name: Some(String::new()),
            manufacturer_data: vec![ManufacturerData {
                company_id: 107,
                payload: vec![0x02, 0x15],
            }],
            service_data: Vec::new(),
            tx_power_level: Some(-4),
        };
        let info = advertisement_info(&snapshot);
        assert_eq!(info.peer_id, "peer-1");
        assert_eq!(info.address, None);
        assert_eq!(info.rssi, None);
        assert_eq!(info.local_name, Some(String::new()));
        assert!(info.service_uuids.is_empty());
        assert_eq!(info.manufacturer_data.len(), 1);
        assert_eq!(info.manufacturer_data[0].company_id, 107);
        assert_eq!(
            info.manufacturer_data[0].payload.as_ref(),
            &[0x02u8, 0x15][..]
        );
        assert_eq!(info.tx_power, Some(-4));
    }

    #[test]
    fn staged_snapshot_validates_ranges() {
        let input = StageAdvertisementInput {
            peer_id: "peer-1".to_owned(),
            address: None,
            rssi: Some(100_000),
            local_name: None,
            service_uuids: None,
            manufacturer_data: None,
            service_data: None,
            tx_power: None,
        };
        let error = staged_snapshot(&input).expect_err("rssi range");
        assert!(error.wire_message().starts_with("argument.invalid|"));
    }

    fn selector_input() -> SelectorInput {
        SelectorInput {
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: Some(0),
            characteristic_uuid: Some(HRM_MEASUREMENT.to_owned()),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        }
    }

    fn hrm_services() -> Vec<StageService> {
        vec![StageService {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![StageCharacteristic {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: StageProperties {
                    read: true,
                    write: false,
                    write_without_response: false,
                    notify: true,
                    indicate: false,
                },
                descriptors: vec![StageDescriptor {
                    uuid: "00002901-0000-1000-8000-00805f9b34fb".to_owned(),
                    occurrence: 0,
                }],
            }],
        }]
    }

    #[tokio::test]
    async fn synthetic_flow_runs_ops_in_rust() {
        let central = UbmCentral::open_synthetic("dispatch-test-a".to_owned())
            .await
            .expect("open");
        let session = central
            .start_scan(ScanOptions {
                owner: "dispatch-test-a".to_owned(),
                service_uuids: None,
                timeout_ms: 5000,
            })
            .await
            .expect("scan");
        assert!(!session.operation_id.is_empty());
        central
            .stage_advertisement(StageAdvertisementInput {
                peer_id: "peer-1".to_owned(),
                address: None,
                rssi: Some(-60),
                local_name: None,
                service_uuids: Some(vec![HRM_SERVICE.to_owned()]),
                manufacturer_data: None,
                service_data: None,
                tx_power: None,
            })
            .await
            .expect("stage");
        let mut observed = None;
        for _ in 0..200 {
            observed = central.take_advertisement().await.expect("take");
            if observed.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let observed = observed.expect("forwarded observation");
        assert_eq!(observed.peer_id, "peer-1");
        assert_eq!(observed.rssi, Some(-60));
        central.stop_scan().await.expect("stop");
        let handle = central
            .connect(ConnectOptions {
                peer_id: "peer-1".to_owned(),
                lease: "lease-a".to_owned(),
                timeout_ms: 5000,
            })
            .await
            .expect("connect");
        assert!(!handle.peer_key.is_empty());
        central
            .stage_services("peer-1".to_owned(), hrm_services())
            .await
            .expect("stage services");
        let report = central
            .discover(DiscoverOptions {
                peer_id: "peer-1".to_owned(),
                lease: "lease-a".to_owned(),
            })
            .await
            .expect("discover");
        assert_eq!(report.paths_registered, 3);
        let paths = central
            .discovered_paths("peer-1".to_owned())
            .await
            .expect("paths");
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[1].properties, 0x09);
        let value = central
            .read(ReadOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                timeout_ms: 5000,
            })
            .await
            .expect("read");
        assert_eq!(value.as_ref(), &[0x42u8][..]);
        central
            .subscribe(SubscribeOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                consumer: "app".to_owned(),
                timeout_ms: 5000,
            })
            .await
            .expect("subscribe");
        central
            .stage_notification(StageNotificationInput {
                peer_id: "peer-1".to_owned(),
                service_uuid: HRM_SERVICE.to_owned(),
                service_occurrence: Some(0),
                characteristic_uuid: HRM_MEASUREMENT.to_owned(),
                characteristic_occurrence: Some(0),
                epoch: None,
                value: Buffer::from(vec![0x06, 0x40]),
            })
            .await
            .expect("stage notification");
        let mut note = None;
        for _ in 0..200 {
            note = central
                .take_notification(SubscriptionOptions {
                    peer_id: "peer-1".to_owned(),
                    selector: selector_input(),
                    consumer: "app".to_owned(),
                })
                .await
                .expect("take");
            if note.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(note.expect("note").as_ref(), &[0x06u8, 0x40][..]);
        let disabled = central
            .unsubscribe(SubscriptionOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                consumer: "app".to_owned(),
            })
            .await
            .expect("unsubscribe");
        assert!(disabled);
        central
            .disconnect(DisconnectOptions {
                peer_id: "peer-1".to_owned(),
                lease: "lease-a".to_owned(),
            })
            .await
            .expect("disconnect");
        central.close().await.expect("close");
    }

    #[tokio::test]
    async fn synthetic_timeout_and_dispose_refuse_loudly() {
        let central = UbmCentral::open_synthetic("dispatch-test-b".to_owned())
            .await
            .expect("open");
        central
            .stage_advertisement(StageAdvertisementInput {
                peer_id: "peer-9".to_owned(),
                address: None,
                rssi: Some(-70),
                local_name: None,
                service_uuids: None,
                manufacturer_data: None,
                service_data: None,
                tx_power: None,
            })
            .await
            .expect("stage");
        central
            .block_radio_op("connect".to_owned())
            .await
            .expect("block");
        let error = match central
            .connect(ConnectOptions {
                peer_id: "peer-9".to_owned(),
                lease: "lease-a".to_owned(),
                timeout_ms: 200,
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("blocked connect must time out"),
        };
        assert!(
            error.reason.starts_with("operation.timed-out|"),
            "{}",
            error.reason
        );
        central
            .unblock_radio_op("connect".to_owned())
            .await
            .expect("unblock");
        central.close().await.expect("close");
        central.close().await.expect("close idempotent");
        let error = match central
            .start_scan(ScanOptions {
                owner: "dispatch-test-b".to_owned(),
                service_uuids: None,
                timeout_ms: 100,
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("post-close scan must refuse"),
        };
        assert!(
            error.reason.starts_with("adapter.unavailable|"),
            "{}",
            error.reason
        );
    }

    #[test]
    fn staged_services_keep_occurrences() {
        let staged = staged_services(&hrm_services());
        assert_eq!(staged.len(), 1);
        let characteristic: &CharacteristicSnapshot = &staged[0].characteristics[0];
        assert_eq!(characteristic.occurrence, 0);
        assert!(characteristic.properties.read);
        assert!(characteristic.properties.notify);
        let descriptor: &DescriptorSnapshot = &characteristic.descriptors[0];
        assert_eq!(descriptor.occurrence, 0);
    }
}
