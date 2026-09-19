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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;

use std::sync::Arc;

use ubm_desktop::central::ControllerFuture;

use napi::bindgen_prelude::{Buffer, Either3, Promise, Result};
use napi::threadsafe_function::{
    ErrorStrategy, ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Env, JsFunction};
use napi::{Error, Status};
use napi_derive::napi;
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::Mutex as AsyncMutex;
use ubm_core::central::ScanDuplicatePolicy;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CommitState, CoreError, OperationId};
use ubm_core::ownership::CleanupState;
use ubm_core::streams::OverflowPolicy;
use ubm_desktop::boundary::{AdapterLossCause, AdmissionPolicy};
use ubm_desktop::central::AdapterResetEvent;
use ubm_desktop::executor::desktop_runtime;
use ubm_desktop::{
    desktop_capability_states, AdapterAuthorization, AdapterEvent, AdapterPowerState, AddressType,
    AdvertisementExtras, BluezBus, BtleplugRadio, Budget, CancelAck, CancelPairingOutcome,
    CentralProfile, CharacteristicAccess, CompletionOutcome, DeliveryMode, DesktopCentral,
    DesktopError, DesktopOs, DiscoveredPath, FakeRadio, FaultOp, InstanceKey, InvalidationCause,
    LifecycleEvent, LifecycleKind, LinkRelease, ManufacturerData, NotificationPoll,
    ObservationSource, ObservedDelivery, OpControl, OpTicket, PairOutcome, PairRequest,
    PairingGeneration, PairingGenerationController, PathSelector, PeerSnapshot, PlatformDetail,
    PlatformValue, PropertyFlags, RadioBoundary, RadioCloseFailure, RadioEvent, Retryability,
    ScanFilterSpec, ScanStop, ScanTerminalEvent, SecureConnections, SecurityEvent, SecurityState,
    ServiceData, ServiceSnapshot, UnpairOutcome, WriteLimits, WriteReadinessEvent,
};

/// Typed dispatch failure carrying a frozen C-UBM identity. [`DesktopError`]
/// and [`CoreError`] identities pass through verbatim, with the outcome facts
/// the central observed (retryability, commit state); only malformed JS
/// input and synthetic-only staging on the production radio originate here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    code: &'static str,
    domain: &'static str,
    operation: String,
    retryability: &'static str,
    commit: &'static str,
    /// The platform's own answer (finding 113) as the wire's JSON field;
    /// absent when the error carries none (boxed: the error stays small).
    platform: Option<Box<str>>,
    detail: Box<str>,
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
            retryability: Retryability::Never.as_str(),
            commit: "",
            platform: None,
            detail: detail.into().into_boxed_str(),
        }
    }

    /// Frozen wire form
    /// `code|domain|operation|retryability|commit|platform|detail`
    /// (`ubm-napi-error/3`). `commit` is empty when the central does not know
    /// it; `retryability` is the central's answer, never derived from the
    /// code, so a dispatched write that may have committed stays `never`.
    /// `platform` is the platform detail as JSON (`{domain, code, message,
    /// metadata}`, no `|`), or empty.
    fn wire_message(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.code,
            self.domain,
            self.operation,
            self.retryability,
            self.commit,
            self.platform.as_deref().unwrap_or(""),
            self.detail
        )
    }
}

/// The largest integer a JS number holds exactly.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// A JSON string literal; `|` is escaped so the field never splits the wire.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\u007c"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The wire's platform field: the platform detail as one JSON object. An
/// integer a JS number cannot hold exactly crosses as its decimal string.
fn platform_wire(platform: &PlatformDetail) -> String {
    let metadata = platform
        .metadata
        .iter()
        .map(|(key, value)| {
            let value = match value {
                PlatformValue::Int(number)
                    if number.unsigned_abs() <= MAX_SAFE_INTEGER.unsigned_abs() =>
                {
                    number.to_string()
                }
                PlatformValue::Int(number) => json_string(&number.to_string()),
                PlatformValue::Text(text) => json_string(text),
                PlatformValue::Bool(flag) => flag.to_string(),
            };
            format!("{}:{}", json_string(key), value)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"domain\":{},\"code\":{},\"message\":{},\"metadata\":{{{}}}}}",
        json_string(&platform.domain),
        json_string(&platform.code),
        platform
            .message
            .as_deref()
            .map_or_else(|| "null".to_owned(), json_string),
        metadata
    )
}

impl From<DesktopError> for DispatchError {
    fn from(error: DesktopError) -> Self {
        Self {
            code: error.code_str(),
            domain: error.domain().as_str(),
            operation: error.operation().to_owned(),
            retryability: error.retryability().as_str(),
            commit: error.commit().map_or("", CommitState::as_str),
            platform: error
                .platform()
                .map(|platform| platform_wire(platform).into_boxed_str()),
            detail: error.detail().unwrap_or("").into(),
        }
    }
}

impl From<CoreError> for DispatchError {
    fn from(error: CoreError) -> Self {
        Self {
            code: error.code().as_str(),
            domain: error.domain().as_str(),
            operation: error.operation().to_owned(),
            retryability: Retryability::Never.as_str(),
            commit: "",
            platform: None,
            detail: "".into(),
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
    // Each radio's own per-OS admission and loss policy (findings 57/58):
    // the trait defaults (no gate, no teardown) would silently disable both
    // on the production radio.
    fn admission_policy(&self) -> AdmissionPolicy {
        match self {
            Self::Radio(radio) => radio.admission_policy(),
            Self::Synthetic(radio) => radio.admission_policy(),
        }
    }

    fn tears_down_on_adapter_loss(&self) -> bool {
        match self {
            Self::Radio(radio) => radio.tears_down_on_adapter_loss(),
            Self::Synthetic(radio) => radio.tears_down_on_adapter_loss(),
        }
    }

    // Notifications the radio's intake dropped (finding 131): the trait
    // default of 0 would hide every ingress drop on both radios.
    fn ingress_notification_drops(&self) -> u64 {
        match self {
            Self::Radio(radio) => radio.ingress_notification_drops(),
            Self::Synthetic(radio) => radio.ingress_notification_drops(),
        }
    }

    // Whether the OS itself answers a subscribe on a characteristic that
    // does not flag notify/indicate (BlueZ StartNotify, finding 98): the
    // default (`false`) would keep a property gate the legacy BlueZ backend
    // never had.
    fn os_answers_unflagged_subscribe(&self) -> bool {
        match self {
            Self::Radio(radio) => radio.os_answers_unflagged_subscribe(),
            Self::Synthetic(radio) => radio.os_answers_unflagged_subscribe(),
        }
    }

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
    ) -> std::result::Result<ubm_desktop::CharacteristicRead, DesktopError> {
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
        requested: Option<DeliveryMode>,
    ) -> std::result::Result<ObservedDelivery, DesktopError> {
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
                        requested,
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
                        requested,
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

    async fn read_rssi(&self, peer_id: &str) -> std::result::Result<i16, DesktopError> {
        match self {
            Self::Radio(radio) => radio.read_rssi(peer_id).await,
            Self::Synthetic(radio) => radio.read_rssi(peer_id).await,
        }
    }

    async fn adapter_state(&self) -> std::result::Result<AdapterPowerState, DesktopError> {
        match self {
            Self::Radio(radio) => radio.adapter_state().await,
            Self::Synthetic(radio) => radio.adapter_state().await,
        }
    }

    // Every capability seam forwards: a default body here would hide a real
    // OS adapter behind `capability.unsupported`.

    async fn adapter_authorization(
        &self,
    ) -> std::result::Result<AdapterAuthorization, DesktopError> {
        match self {
            Self::Radio(radio) => radio.adapter_authorization().await,
            Self::Synthetic(radio) => radio.adapter_authorization().await,
        }
    }

    async fn write_limits(&self, peer_id: &str) -> Option<WriteLimits> {
        match self {
            Self::Radio(radio) => radio.write_limits(peer_id).await,
            Self::Synthetic(radio) => radio.write_limits(peer_id).await,
        }
    }

    fn reports_security_changes(&self) -> bool {
        match self {
            Self::Radio(radio) => radio.reports_security_changes(),
            Self::Synthetic(radio) => radio.reports_security_changes(),
        }
    }

    async fn security_state(
        &self,
        peer_id: &str,
    ) -> std::result::Result<SecurityState, DesktopError> {
        match self {
            Self::Radio(radio) => radio.security_state(peer_id).await,
            Self::Synthetic(radio) => radio.security_state(peer_id).await,
        }
    }

    async fn pair(&self, peer_id: &str) -> std::result::Result<PairOutcome, DesktopError> {
        match self {
            Self::Radio(radio) => radio.pair(peer_id).await,
            Self::Synthetic(radio) => radio.pair(peer_id).await,
        }
    }

    async fn cancel_pairing(&self, peer_id: &str) -> std::result::Result<(), DesktopError> {
        match self {
            Self::Radio(radio) => radio.cancel_pairing(peer_id).await,
            Self::Synthetic(radio) => radio.cancel_pairing(peer_id).await,
        }
    }

    async fn unpair(&self, peer_id: &str) -> std::result::Result<UnpairOutcome, DesktopError> {
        match self {
            Self::Radio(radio) => radio.unpair(peer_id).await,
            Self::Synthetic(radio) => radio.unpair(peer_id).await,
        }
    }

    async fn resolve_address(
        &self,
        address: &str,
        address_type: AddressType,
    ) -> std::result::Result<String, DesktopError> {
        match self {
            Self::Radio(radio) => radio.resolve_address(address, address_type).await,
            Self::Synthetic(radio) => radio.resolve_address(address, address_type).await,
        }
    }

    async fn address_type(
        &self,
        peer_id: &str,
    ) -> std::result::Result<Option<AddressType>, DesktopError> {
        match self {
            Self::Radio(radio) => radio.address_type(peer_id).await,
            Self::Synthetic(radio) => radio.address_type(peer_id).await,
        }
    }

    async fn write_without_response_ready(
        &self,
        peer_id: &str,
    ) -> std::result::Result<bool, DesktopError> {
        match self {
            Self::Radio(radio) => radio.write_without_response_ready(peer_id).await,
            Self::Synthetic(radio) => radio.write_without_response_ready(peer_id).await,
        }
    }

    async fn characteristic_access(
        &self,
        peer_id: &str,
    ) -> std::result::Result<HashMap<InstanceKey, CharacteristicAccess>, DesktopError> {
        match self {
            Self::Radio(radio) => radio.characteristic_access(peer_id).await,
            Self::Synthetic(radio) => radio.characteristic_access(peer_id).await,
        }
    }
}

/// Counts cross as `u32` (exact in JS); occurrences never approach the
/// ceiling, and the conversion fails closed instead of wrapping.
fn count_wire(value: u64, operation: &'static str) -> std::result::Result<u32, DispatchError> {
    u32::try_from(value).map_err(|_| overflow_error(operation))
}

/// Operation budget for one JS call: `timeoutMs` becomes the caller budget,
/// measured from the moment the call reached Rust. Absent means the caller
/// gave no budget, so the central's named liveness backstop bounds the
/// operation (`LIVENESS_OP` / `LIVENESS_CLEANUP` / `LIVENESS_SCAN_START`),
/// never an invented caller deadline.
fn budget_of(timeout_ms: Option<u32>) -> Budget {
    timeout_ms.map_or_else(Budget::unbounded, |ms| Budget::from_ms(u64::from(ms)))
}

fn scan_stop_wire(stop: ScanStop) -> String {
    match stop {
        ScanStop::Stopped => "stopped".to_owned(),
        ScanStop::NotActive => "not-active".to_owned(),
    }
}

fn link_release_wire(release: LinkRelease) -> String {
    match release {
        LinkRelease::Released => "released".to_owned(),
        LinkRelease::AlreadyReleased => "already-released".to_owned(),
    }
}

/// The platform family this binary's radio speaks, fixed at compile time.
/// btleplug selects CoreBluetooth, WinRT or BlueZ by target OS; the name is
/// what the JS provider asked for, so a mismatch is caught before any radio.
const fn compiled_platform() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("corebluetooth")
    } else if cfg!(target_os = "windows") {
        Some("winrt")
    } else if cfg!(target_os = "linux") {
        Some("bluez")
    } else {
        None
    }
}

/// Refuse a platform the binary does not drive: a darwin addon never opens
/// CoreBluetooth while the caller believes it is BlueZ (PR210-29).
fn check_platform(platform: &str) -> std::result::Result<(), DispatchError> {
    if !matches!(platform, "bluez" | "corebluetooth" | "winrt") {
        return Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.open.platform",
            format!("unknown platform {platform:?} (bluez, corebluetooth, winrt)"),
        ));
    }
    match compiled_platform() {
        Some(compiled) if compiled == platform => Ok(()),
        compiled => Err(DispatchError::new(
            BleErrorCode::CapabilityUnavailable.as_str(),
            BleErrorDomain::Platform.as_str(),
            "dispatch.open.platform",
            format!(
                "platform {platform:?} requested; this binary drives {}",
                compiled.unwrap_or("no desktop radio")
            ),
        )),
    }
}

fn scan_duplicate_policy(
    value: Option<&str>,
) -> std::result::Result<ScanDuplicatePolicy, DispatchError> {
    match value {
        None => Ok(ScanDuplicatePolicy::All),
        Some(text) => ScanDuplicatePolicy::from_str(text).ok_or_else(|| {
            DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Scan.as_str(),
                "dispatch.scan-duplicate-policy",
                format!("unknown duplicate policy {text:?} (all, first, merged)"),
            )
        }),
    }
}

/// The per-OS admission a desktop platform's legacy backend applied
/// (`bluez` had none beyond lifecycle).
fn platform_admission(os: DesktopOs) -> AdmissionPolicy {
    match os {
        DesktopOs::MacOs => AdmissionPolicy::CoreBluetooth,
        DesktopOs::Windows => AdmissionPolicy::WinRt,
        DesktopOs::Linux => AdmissionPolicy::LifecycleOnly,
    }
}

fn parse_adapter_power(
    value: &str,
    operation: &'static str,
) -> std::result::Result<AdapterPowerState, DispatchError> {
    match value {
        "powered-on" => Ok(AdapterPowerState::PoweredOn),
        "powered-off" => Ok(AdapterPowerState::PoweredOff),
        "resetting" => Ok(AdapterPowerState::Resetting),
        "unsupported" => Ok(AdapterPowerState::Unsupported),
        "unauthorized" => Ok(AdapterPowerState::Unauthorized),
        "unknown" => Ok(AdapterPowerState::Unknown),
        other => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            operation,
            format!("unknown adapter state {other:?}"),
        )),
    }
}

fn parse_loss_cause(value: &str) -> std::result::Result<AdapterLossCause, DispatchError> {
    match value {
        "powered-off" => Ok(AdapterLossCause::PoweredOff),
        "resetting" => Ok(AdapterLossCause::Resetting),
        "unsupported" => Ok(AdapterLossCause::Unsupported),
        "unauthorized" => Ok(AdapterLossCause::Unauthorized),
        "removed" => Ok(AdapterLossCause::Removed),
        "daemon-restarted" => Ok(AdapterLossCause::DaemonRestarted),
        other => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.stage-adapter-reset",
            format!("unknown adapter loss cause {other:?}"),
        )),
    }
}

fn adapter_reset_wire(
    event: AdapterResetEvent,
) -> std::result::Result<AdapterResetEventInfo, DispatchError> {
    const OP: &str = "dispatch.take-adapter-reset-event";
    let tuple = |tuple: &ubm_core::contracts::AttachmentTuple| AttachmentTupleInfo {
        attachment_id: tuple.attachment_id().as_str().to_owned(),
        backend_instance_id: tuple.backend_instance_id().as_str().to_owned(),
        backend_generation: tuple.backend_generation().as_str().to_owned(),
        adapter_id: tuple.adapter_id().as_str().to_owned(),
        adapter_generation: tuple.adapter_generation().as_str().to_owned(),
    };
    Ok(AdapterResetEventInfo {
        kind: "reset".to_owned(),
        sequence: Some(number_wire(event.sequence, OP)?),
        cause: Some(event.cause.as_str().to_owned()),
        previous: Some(tuple(&event.previous)),
        current: Some(tuple(&event.current)),
        cancelled_operations: Some(number_wire(event.cancelled_operations as u64, OP)?),
        ended_scan: event.ended_scan.map(|id| id.as_str().to_owned()),
        released_links: Some(event.released_links),
        ended_subscriptions: Some(number_wire(event.ended_subscriptions as u64, OP)?),
        release_failures: Some(
            event
                .release_failures
                .into_iter()
                .map(|error| DispatchError::from(error).wire_message())
                .collect(),
        ),
        missed: None,
    })
}

fn delivery_mode(mode: Option<&str>) -> std::result::Result<Option<DeliveryMode>, DispatchError> {
    match mode {
        None => Ok(None),
        Some("notification") => Ok(Some(DeliveryMode::Notification)),
        Some("indication") => Ok(Some(DeliveryMode::Indication)),
        Some(other) => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.delivery-mode",
            format!("unknown delivery mode {other:?}"),
        )),
    }
}

fn lifecycle_kind_wire(kind: LifecycleKind) -> (&'static str, Option<bool>) {
    match kind {
        LifecycleKind::LinkLost => ("link-lost", None),
        LifecycleKind::Released { requested } => ("released", Some(requested)),
        LifecycleKind::ServicesChanged => ("services-changed", None),
        LifecycleKind::AdapterLost => ("adapter-lost", None),
    }
}

/// `u64` sequences and counts cross as JS numbers: exact to 2^53, and the
/// conversion fails closed beyond it instead of rounding.
fn number_wire(value: u64, operation: &'static str) -> std::result::Result<i64, DispatchError> {
    const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
    if value > MAX_SAFE_INTEGER {
        return Err(overflow_error(operation));
    }
    i64::try_from(value).map_err(|_| overflow_error(operation))
}

/// Open a central on the shared desktop executor (`DesktopCentral::open`'s
/// contract): its event loop and the radio's forwarders must outlive napi's
/// own runtime, which is torn down during environment cleanup.
async fn open_on_desktop_runtime<F>(open: F) -> Result<DesktopCentral<DispatchRadio>>
where
    F: std::future::Future<
            Output = std::result::Result<DesktopCentral<DispatchRadio>, DesktopError>,
        > + Send
        + 'static,
{
    desktop_runtime()
        .spawn(open)
        .await
        .map_err(|join| {
            to_napi(DispatchError::new(
                BleErrorCode::LifecycleInvariantViolation.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.open",
                format!("open task failed: {join}"),
            ))
        })?
        .map_err(|error| to_napi(DispatchError::from(error)))
}

/// `UbmCentral.open` arguments. `platform` names the radio family the JS
/// provider expects (`bluez` / `corebluetooth` / `winrt`); `adapterId`
/// selects one OS adapter by its `listAdapters` label (`None` = default).
#[napi(object)]
pub struct OpenOptions {
    pub owner: String,
    pub platform: String,
    #[napi(js_name = "adapterId")]
    pub adapter_id: Option<String>,
    /// BlueZ D-Bus bus (`system` default / `session`); a bus this build
    /// cannot reach is `capability.unsupported`, never replaced.
    #[napi(js_name = "bluezBus")]
    pub bluez_bus: Option<String>,
    /// The host will install a privileged pairing-generation controller
    /// ([`UbmCentral::install_pairing_generation_controller`]): registers
    /// `security:pairing-generation` where an OS adapter can hold it.
    #[napi(js_name = "pairingGeneration")]
    pub pairing_generation: Option<bool>,
}

fn parse_bluez_bus(value: Option<&str>) -> std::result::Result<BluezBus, DispatchError> {
    match value {
        None | Some("system") => Ok(BluezBus::System),
        Some("session") => Ok(BluezBus::Session),
        Some(other) => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.bluez-bus",
            format!("unknown bus {other:?} (system, session)"),
        )),
    }
}

fn parse_generation(value: &str) -> std::result::Result<PairingGeneration, String> {
    match value {
        "legacy-only" => Ok(PairingGeneration::LegacyOnly),
        "enabled" => Ok(PairingGeneration::Enabled),
        "required" => Ok(PairingGeneration::Required),
        other => Err(format!(
            "controller returned an unknown generation {other:?}"
        )),
    }
}

/// The host's privileged pairing-generation operations (JS `read` / `set`)
/// behind the core's [`PairingGenerationController`]. The package never
/// escalates: these run only because the host supplied them.
struct JsGenerationController {
    read: ThreadsafeFunction<String, ErrorStrategy::Fatal>,
    set: ThreadsafeFunction<(String, String), ErrorStrategy::Fatal>,
}

impl PairingGenerationController for JsGenerationController {
    fn read<'a>(&'a self, adapter_id: &'a str) -> ControllerFuture<'a, PairingGeneration> {
        let adapter = adapter_id.to_owned();
        Box::pin(async move {
            let pending: Promise<String> = self
                .read
                .call_async(adapter)
                .await
                .map_err(|error| format!("controller read did not start: {error}"))?;
            let value = pending
                .await
                .map_err(|error| format!("controller read rejected: {error}"))?;
            parse_generation(&value)
        })
    }

    fn set<'a>(
        &'a self,
        adapter_id: &'a str,
        generation: PairingGeneration,
    ) -> ControllerFuture<'a, ()> {
        let args = (adapter_id.to_owned(), generation.as_str().to_owned());
        Box::pin(async move {
            let pending: Promise<()> = self
                .set
                .call_async(args)
                .await
                .map_err(|error| format!("controller set did not start: {error}"))?;
            pending
                .await
                .map(|_| ())
                .map_err(|error| format!("controller set rejected: {error}"))
        })
    }
}

/// Synthetic central options: `pairingGeneration` registers
/// `security:pairing-generation` as a host controller would.
#[napi(object)]
pub struct SyntheticOptions {
    #[napi(js_name = "pairingGeneration")]
    pub pairing_generation: Option<bool>,
    /// The desktop platform whose capability rows the synthetic central
    /// registers (default: this build's OS), so a hardware-free test of one
    /// platform's provider runs under that platform's truth on any host.
    pub platform: Option<String>,
}

fn register_macos(core: &mut ubm_core::central::Central) -> std::result::Result<(), CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(DesktopOs::MacOs), false)
}
fn register_windows(core: &mut ubm_core::central::Central) -> std::result::Result<(), CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(DesktopOs::Windows), false)
}
fn register_linux(core: &mut ubm_core::central::Central) -> std::result::Result<(), CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(DesktopOs::Linux), false)
}
fn register_macos_generation(
    core: &mut ubm_core::central::Central,
) -> std::result::Result<(), CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(DesktopOs::MacOs), true)
}
fn register_windows_generation(
    core: &mut ubm_core::central::Central,
) -> std::result::Result<(), CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(DesktopOs::Windows), true)
}
fn register_linux_generation(
    core: &mut ubm_core::central::Central,
) -> std::result::Result<(), CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(DesktopOs::Linux), true)
}

type RegisterCapabilities =
    fn(&mut ubm_core::central::Central) -> std::result::Result<(), CoreError>;

fn synthetic_registration(
    platform: Option<&str>,
    pairing_generation: bool,
) -> std::result::Result<RegisterCapabilities, DispatchError> {
    let os = match platform {
        None => DesktopOs::current(),
        Some(name) => Some(desktop_os(name)?),
    };
    Ok(match (os, pairing_generation) {
        (Some(DesktopOs::MacOs), false) => register_macos,
        (Some(DesktopOs::Windows), false) => register_windows,
        (Some(DesktopOs::Linux), false) => register_linux,
        (Some(DesktopOs::MacOs), true) => register_macos_generation,
        (Some(DesktopOs::Windows), true) => register_windows_generation,
        (Some(DesktopOs::Linux), true) => register_linux_generation,
        (None, false) => ubm_desktop::register_desktop_capabilities,
        (None, true) => ubm_desktop::register_desktop_capabilities_with_pairing_generation,
    })
}

/// One OS adapter listing: its label, or the error the OS returned while
/// reading it (never a synthesized label).
#[napi(object)]
pub struct AdapterListingInfo {
    pub index: u32,
    pub label: Option<String>,
    pub error: Option<String>,
    /// The OS's descriptive name, when it gives one.
    #[napi(js_name = "displayName")]
    pub display_name: Option<String>,
    /// Whether an unnamed open selects this adapter.
    pub default: bool,
    /// Windows: `packaged` / `unpackaged` (the legacy per-adapter
    /// `deployment`); absent elsewhere.
    pub deployment: Option<String>,
}

/// `scan.start` arguments.
#[napi(object)]
pub struct ScanOptions {
    pub owner: String,
    pub service_uuids: Option<Vec<String>>,
    /// `all` (default) / `first` / `merged`: the OS duplicate filter the
    /// radio applies (finding 63/64).
    #[napi(js_name = "duplicatePolicy")]
    pub duplicate_policy: Option<String>,
    /// The local-name prefix the OS filter narrows by (BlueZ
    /// `SetDiscoveryFilter` `Pattern`, finding 89); the caller's software
    /// match stays the final filter. Absent: no OS name filter.
    #[napi(js_name = "localNamePrefix")]
    pub local_name_prefix: Option<String>,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// `scan.stop` / cleanup arguments: an optional budget and ticket.
#[napi(object)]
pub struct ControlOptions {
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
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

/// One characteristic read: the value and what the radio says it is
/// (`read-response` | `read-or-notification`).
#[napi(object)]
pub struct ReadInfo {
    pub value: Buffer,
    pub provenance: String,
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
    /// Solicited service UUIDs (`null` = not carried / not reported).
    #[napi(js_name = "solicitedServiceUuids")]
    pub solicited_service_uuids: Option<Vec<String>>,
    /// Overflow-area service UUIDs (`null` = not carried / not reported).
    #[napi(js_name = "overflowServiceUuids")]
    pub overflow_service_uuids: Option<Vec<String>>,
    /// Whether the advertisement is connectable (`null` = not reported).
    pub connectable: Option<bool>,
    /// What the observation is: `advertisement` (this one advertisement's
    /// own data) or `device-state` (the OS's merged device state).
    pub source: String,
}

/// Process-wide OS-adapter failure counts (never silent: each is also
/// logged by the core).
#[napi(object)]
pub struct OsAdapterFailuresInfo {
    #[napi(js_name = "linkStateRelease")]
    pub link_state_release: i64,
    #[napi(js_name = "eventDrops")]
    pub event_drops: i64,
    #[napi(js_name = "watchFailures")]
    pub watch_failures: i64,
    /// Sightings whose data the OS could not return (finding 122).
    #[napi(js_name = "advertisementReadFailures")]
    pub advertisement_read_failures: i64,
}

/// One observation of the live scan (finding 121): the scan it belongs to
/// and how long ago the core received it from the radio.
#[napi(object)]
pub struct ScanObservationInfo {
    pub advertisement: AdvertisementInfo,
    #[napi(js_name = "scanOperationId")]
    pub scan_operation_id: String,
    /// Milliseconds since the core received the sighting (measured at take).
    #[napi(js_name = "ageMs")]
    pub age_ms: f64,
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
        solicited_service_uuids: snapshot.extras.solicited_service_uuids.clone(),
        overflow_service_uuids: snapshot.extras.overflow_service_uuids.clone(),
        connectable: snapshot.extras.connectable,
        source: snapshot.extras.source.as_str().to_owned(),
    }
}

/// `connection.connect` arguments.
#[napi(object)]
pub struct ConnectOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// Connection handle: session peer key plus core generation.
#[napi(object)]
pub struct ConnectionInfo {
    #[napi(js_name = "peerKey")]
    pub peer_key: String,
    #[napi(js_name = "connectionGeneration")]
    pub connection_generation: Option<String>,
}

/// Lease-scoped connection arguments (disconnect, discover, RSSI).
#[napi(object)]
pub struct LeaseOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// Discovery report: the paths the whole snapshot registered. A snapshot
/// registers whole or the discovery fails with a typed error (finding 95);
/// no entry is ever skipped.
#[napi(object)]
pub struct DiscoveryInfo {
    #[napi(js_name = "pathsRegistered")]
    pub paths_registered: u32,
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
    /// Characteristic facts beyond the five property bits, when the radio
    /// reports them for this platform (`null` otherwise; each field `null`
    /// when the OS does not report that fact — never `false`).
    pub access: Option<CharacteristicAccessInfo>,
}

/// Wire form of [`CharacteristicAccess`].
#[napi(object)]
pub struct CharacteristicAccessInfo {
    pub broadcast: Option<bool>,
    #[napi(js_name = "authenticatedSignedWrites")]
    pub authenticated_signed_writes: Option<bool>,
    #[napi(js_name = "extendedProperties")]
    pub extended_properties: Option<bool>,
    #[napi(js_name = "reliableWrite")]
    pub reliable_write: Option<bool>,
    #[napi(js_name = "writableAuxiliaries")]
    pub writable_auxiliaries: Option<bool>,
    #[napi(js_name = "encryptRead")]
    pub encrypt_read: Option<bool>,
    #[napi(js_name = "encryptWrite")]
    pub encrypt_write: Option<bool>,
    #[napi(js_name = "encryptAuthenticatedRead")]
    pub encrypt_authenticated_read: Option<bool>,
    #[napi(js_name = "encryptAuthenticatedWrite")]
    pub encrypt_authenticated_write: Option<bool>,
    #[napi(js_name = "secureRead")]
    pub secure_read: Option<bool>,
    #[napi(js_name = "secureWrite")]
    pub secure_write: Option<bool>,
    pub authorize: Option<bool>,
}

fn access_info(access: &CharacteristicAccess) -> CharacteristicAccessInfo {
    CharacteristicAccessInfo {
        broadcast: access.broadcast,
        authenticated_signed_writes: access.authenticated_signed_writes,
        extended_properties: access.extended_properties,
        reliable_write: access.reliable_write,
        writable_auxiliaries: access.writable_auxiliaries,
        encrypt_read: access.encrypt_read,
        encrypt_write: access.encrypt_write,
        encrypt_authenticated_read: access.encrypt_authenticated_read,
        encrypt_authenticated_write: access.encrypt_authenticated_write,
        secure_read: access.secure_read,
        secure_write: access.secure_write,
        authorize: access.authorize,
    }
}

fn access_of(info: &CharacteristicAccessInfo) -> CharacteristicAccess {
    CharacteristicAccess {
        broadcast: info.broadcast,
        authenticated_signed_writes: info.authenticated_signed_writes,
        extended_properties: info.extended_properties,
        reliable_write: info.reliable_write,
        writable_auxiliaries: info.writable_auxiliaries,
        encrypt_read: info.encrypt_read,
        encrypt_write: info.encrypt_write,
        encrypt_authenticated_read: info.encrypt_authenticated_read,
        encrypt_authenticated_write: info.encrypt_authenticated_write,
        secure_read: info.secure_read,
        secure_write: info.secure_write,
        authorize: info.authorize,
    }
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
        access: path.access.as_ref().map(access_info),
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

/// `gatt.read` / `gatt.read-descriptor` arguments.
#[napi(object)]
pub struct ReadOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
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
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// `gatt.write-descriptor` arguments (the core writes descriptors with a
/// response; it takes no write mode).
#[napi(object)]
pub struct WriteDescriptorOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub value: Buffer,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
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

/// `gatt.subscribe` arguments. `deliveryMode` (`notification` /
/// `indication`) is a hard requirement carried to the radio, which writes
/// that CCCD mode or refuses before any effect; absent, the radio picks.
#[napi(object)]
pub struct SubscribeOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub consumer: String,
    #[napi(js_name = "deliveryMode")]
    pub delivery_mode: Option<String>,
    /// The consumer's overflow policy (`error` by default): what the core
    /// does when values outrun the consumer or are lost upstream
    /// (finding 131).
    #[napi(js_name = "overflowPolicy")]
    pub overflow_policy: Option<String>,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// One consumer's cumulative stream accounting in the core (finding 131).
#[napi(object)]
pub struct ConsumerCountersInfo {
    #[napi(js_name = "droppedItems")]
    pub dropped_items: i64,
    #[napi(js_name = "droppedBytes")]
    pub dropped_bytes: i64,
    #[napi(js_name = "replacedItems")]
    pub replaced_items: i64,
    /// Values lost before they reached the core (radio intake or forwarder).
    #[napi(js_name = "upstreamLost")]
    pub upstream_lost: i64,
    pub terminated: bool,
}

/// What the radio reported for one enablement: `notification`,
/// `indication`, or `unknown` when the platform does not say.
#[napi(object)]
pub struct SubscribeInfo {
    pub delivery: String,
}

/// Subscription identity arguments (take/poll/unsubscribe). The budget and
/// ticket apply to `unsubscribe` only.
#[napi(object)]
pub struct SubscriptionOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub selector: SelectorInput,
    pub consumer: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// One typed notification poll outcome (F17): `value`, `empty` (live),
/// `terminal` (overflow, exactly once), `invalidated` (with `cause`
/// `services-changed` / `link-ended`), or `closed`.
#[napi(object)]
pub struct NotificationPollInfo {
    pub kind: String,
    pub value: Option<Buffer>,
    pub cause: Option<String>,
    #[napi(js_name = "droppedItems")]
    pub dropped_items: Option<i64>,
    #[napi(js_name = "droppedBytes")]
    pub dropped_bytes: Option<i64>,
    #[napi(js_name = "replacedItems")]
    pub replaced_items: Option<i64>,
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

/// What a ticket cancel did: `recorded-before-admission` (the op ends
/// `operation.aborted` without a radio call), `forwarded` (exactly the one
/// core op the ticket names was cancelled; `cancel` is the core's answer),
/// or `already-settled`.
#[napi(object)]
pub struct TicketCancelInfo {
    pub outcome: String,
    #[napi(js_name = "operationId")]
    pub operation_id: Option<String>,
    pub cancel: Option<CancelInfo>,
}

/// One connection-lifecycle event (PR210-11) or a gap marker. `kind` is
/// `link-lost`, `released` (with `requested`), `services-changed`, or
/// `lagged` (the receiver fell more than `missed` events behind: every
/// stream it owns must be treated as overflowed) / `closed`.
#[napi(object)]
pub struct LifecycleEventInfo {
    pub kind: String,
    pub sequence: Option<i64>,
    #[napi(js_name = "peerId")]
    pub peer_id: Option<String>,
    #[napi(js_name = "peerKey")]
    pub peer_key: Option<String>,
    #[napi(js_name = "connectionGeneration")]
    pub connection_generation: Option<String>,
    pub requested: Option<bool>,
    pub missed: Option<i64>,
}

/// One adapter power-state change the OS reported, or a gap marker
/// (`kind`: `state` / `lagged` / `closed`).
#[napi(object)]
pub struct AdapterEventInfo {
    pub kind: String,
    pub sequence: Option<i64>,
    pub state: Option<String>,
    pub missed: Option<i64>,
}

/// One core attachment tuple (the core's own generation identities).
#[napi(object)]
pub struct AttachmentTupleInfo {
    #[napi(js_name = "attachmentId")]
    pub attachment_id: String,
    #[napi(js_name = "backendInstanceId")]
    pub backend_instance_id: String,
    #[napi(js_name = "backendGeneration")]
    pub backend_generation: String,
    #[napi(js_name = "adapterId")]
    pub adapter_id: String,
    #[napi(js_name = "adapterGeneration")]
    pub adapter_generation: String,
}

/// One adapter reset (finding 57) after the core's teardown, or a gap
/// marker (`lagged` / `closed`).
#[napi(object)]
pub struct AdapterResetEventInfo {
    pub kind: String,
    pub sequence: Option<i64>,
    /// `powered-off` / `resetting` / `unsupported` / `unauthorized` /
    /// `removed` / `daemon-restarted`.
    pub cause: Option<String>,
    pub previous: Option<AttachmentTupleInfo>,
    pub current: Option<AttachmentTupleInfo>,
    #[napi(js_name = "cancelledOperations")]
    pub cancelled_operations: Option<i64>,
    #[napi(js_name = "endedScan")]
    pub ended_scan: Option<String>,
    #[napi(js_name = "releasedLinks")]
    pub released_links: Option<Vec<String>>,
    #[napi(js_name = "endedSubscriptions")]
    pub ended_subscriptions: Option<i64>,
    /// OS releases that did not complete, each in the error wire form.
    #[napi(js_name = "releaseFailures")]
    pub release_failures: Option<Vec<String>>,
    pub missed: Option<i64>,
}

/// The adapter facts the core admits against (finding 58); a fact never
/// reported is absent.
#[napi(object)]
pub struct AdapterStatusInfo {
    pub power: Option<String>,
    pub authorization: Option<String>,
    pub availability: String,
    pub lost: bool,
}

/// One resolved radio peer with the core's connection facts (LEGACY-AUDIT-2
/// N5): what a lagged lifecycle receiver re-reads instead of guessing.
/// An absent fact means the core holds no connection record for the peer.
#[napi(object)]
pub struct PeerRecordInfo {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    #[napi(js_name = "peerKey")]
    pub peer_key: String,
    #[napi(js_name = "connectionState")]
    pub connection_state: Option<String>,
    #[napi(js_name = "connectionGeneration")]
    pub connection_generation: Option<String>,
    #[napi(js_name = "databaseGeneration")]
    pub database_generation: Option<String>,
    #[napi(js_name = "databaseState")]
    pub database_state: Option<String>,
}

/// A platform's structured answer staged on a synthetic fault (finding 113).
#[napi(object)]
pub struct StagePlatformDetail {
    pub domain: String,
    pub code: String,
    pub message: Option<String>,
    pub metadata: Option<HashMap<String, Either3<String, i64, bool>>>,
}

/// Peer-scoped control arguments (security, address type).
#[napi(object)]
pub struct PeerControlOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// Link-security facts the OS reports (`bond`: bonded / not-bonded /
/// unknown; `pairingPossible` `null` when the OS does not say).
#[napi(object)]
pub struct SecurityStateInfo {
    pub bond: String,
    #[napi(js_name = "pairingPossible")]
    pub pairing_possible: Option<bool>,
}

fn security_info(state: &SecurityState) -> SecurityStateInfo {
    SecurityStateInfo {
        bond: state.bond.as_str().to_owned(),
        pairing_possible: state.pairing_possible,
    }
}

/// `security.pair` arguments. `secureConnections` `require` / `disallow`
/// directs the LE pairing generation (needs a host generation controller;
/// without one it is `capability.unsupported` before any effect); absent
/// defers to the platform.
#[napi(object)]
pub struct PairOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    #[napi(js_name = "secureConnections")]
    pub secure_connections: Option<String>,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// What one ceremony achieved: `paired` / `already-paired` (with `state`),
/// `rejected` (with `reason`), or `cancelled`.
#[napi(object)]
pub struct PairOutcomeInfo {
    pub outcome: String,
    pub state: Option<SecurityStateInfo>,
    pub reason: Option<String>,
}

/// What a cancel achieved, read from the ceremony's own answer.
#[napi(object)]
pub struct CancelPairingInfo {
    pub outcome: String,
    pub reason: Option<String>,
}

/// One link-security change, or a gap marker (`state` / `lagged` / `closed`).
#[napi(object)]
pub struct SecurityEventInfo {
    pub kind: String,
    pub sequence: Option<i64>,
    #[napi(js_name = "peerId")]
    pub peer_id: Option<String>,
    pub state: Option<SecurityStateInfo>,
    pub missed: Option<i64>,
}

/// `peer.address-targeting` arguments (`addressType`: public / random).
#[napi(object)]
pub struct ResolveAddressOptions {
    pub address: String,
    #[napi(js_name = "addressType")]
    pub address_type: String,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// `gatt.maximum-write-length` arguments: the lease-held link, one
/// characteristic of the current database, and the write mode.
#[napi(object)]
pub struct ConnectionMaximumWriteLengthOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
    #[napi(js_name = "withResponse")]
    pub with_response: bool,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// `maximumWriteLength` arguments.
#[napi(object)]
pub struct MaximumWriteLengthOptions {
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    pub lease: String,
    pub selector: SelectorInput,
    #[napi(js_name = "withResponse")]
    pub with_response: bool,
    #[napi(js_name = "timeoutMs")]
    pub timeout_ms: Option<u32>,
    pub ticket: Option<String>,
}

/// One row of the core's capability registration for a desktop OS.
#[napi(object)]
pub struct CapabilityStateInfo {
    pub id: String,
    pub state: String,
    pub limitation: Option<String>,
}

fn parse_address_type(value: &str) -> std::result::Result<AddressType, DispatchError> {
    match value {
        "public" => Ok(AddressType::Public),
        "random" => Ok(AddressType::Random),
        other => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.address-type",
            format!("unknown address type {other:?} (public, random)"),
        )),
    }
}

fn parse_secure_connections(
    value: Option<&str>,
) -> std::result::Result<Option<SecureConnections>, DispatchError> {
    match value {
        None | Some("prefer") => Ok(None),
        Some("require") => Ok(Some(SecureConnections::Require)),
        Some("disallow") => Ok(Some(SecureConnections::Disallow)),
        Some(other) => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.secure-connections",
            format!("unknown secureConnections {other:?} (prefer, require, disallow)"),
        )),
    }
}

fn desktop_os(platform: &str) -> std::result::Result<DesktopOs, DispatchError> {
    match platform {
        "corebluetooth" => Ok(DesktopOs::MacOs),
        "winrt" => Ok(DesktopOs::Windows),
        "bluez" => Ok(DesktopOs::Linux),
        other => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.capability-states",
            format!("unknown platform {other:?} (bluez, corebluetooth, winrt)"),
        )),
    }
}

fn pair_outcome_info(outcome: PairOutcome) -> PairOutcomeInfo {
    match outcome {
        PairOutcome::Paired(state) => PairOutcomeInfo {
            outcome: "paired".to_owned(),
            state: Some(security_info(&state)),
            reason: None,
        },
        PairOutcome::AlreadyPaired(state) => PairOutcomeInfo {
            outcome: "already-paired".to_owned(),
            state: Some(security_info(&state)),
            reason: None,
        },
        PairOutcome::Rejected(reason) => PairOutcomeInfo {
            outcome: "rejected".to_owned(),
            state: None,
            reason,
        },
        PairOutcome::Cancelled => PairOutcomeInfo {
            outcome: "cancelled".to_owned(),
            state: None,
            reason: None,
        },
    }
}

/// One write-without-response readiness report (`state`) or a gap marker.
#[napi(object)]
pub struct WriteReadinessEventInfo {
    pub kind: String,
    pub sequence: Option<i64>,
    #[napi(js_name = "peerId")]
    pub peer_id: Option<String>,
    #[napi(js_name = "connectionGeneration")]
    pub connection_generation: Option<String>,
    pub ready: Option<bool>,
    pub missed: Option<i64>,
}

/// A scan the OS ended without a stop request (`terminal`), or a gap marker.
#[napi(object)]
pub struct ScanTerminalEventInfo {
    pub kind: String,
    pub sequence: Option<i64>,
    #[napi(js_name = "operationId")]
    pub operation_id: Option<String>,
    pub aborted: Option<bool>,
    pub detail: Option<String>,
    pub missed: Option<i64>,
}

/// Rust-side execution witness: how many calls of each verb this central
/// admitted into `DesktopCentral` (the "Rust ingress" count acceptance
/// reads — TypeScript never schedules these).
#[napi(object)]
pub struct DispatchCountersInfo {
    #[napi(js_name = "scanStart")]
    pub scan_start: i64,
    #[napi(js_name = "scanStop")]
    pub scan_stop: i64,
    pub connect: i64,
    pub disconnect: i64,
    pub discover: i64,
    pub read: i64,
    pub write: i64,
    #[napi(js_name = "readDescriptor")]
    pub read_descriptor: i64,
    #[napi(js_name = "writeDescriptor")]
    pub write_descriptor: i64,
    pub subscribe: i64,
    pub unsubscribe: i64,
    #[napi(js_name = "notificationValues")]
    pub notification_values: i64,
    #[napi(js_name = "readRssi")]
    pub read_rssi: i64,
    #[napi(js_name = "adapterState")]
    pub adapter_state: i64,
    pub cancel: i64,
    pub security: i64,
    #[napi(js_name = "resolveAddress")]
    pub resolve_address: i64,
    #[napi(js_name = "maximumWriteLength")]
    pub maximum_write_length: i64,
}

#[derive(Default)]
struct DispatchCounters {
    scan_start: AtomicU64,
    scan_stop: AtomicU64,
    connect: AtomicU64,
    disconnect: AtomicU64,
    discover: AtomicU64,
    read: AtomicU64,
    write: AtomicU64,
    read_descriptor: AtomicU64,
    write_descriptor: AtomicU64,
    subscribe: AtomicU64,
    unsubscribe: AtomicU64,
    notification_values: AtomicU64,
    read_rssi: AtomicU64,
    adapter_state: AtomicU64,
    cancel: AtomicU64,
    security: AtomicU64,
    resolve_address: AtomicU64,
    maximum_write_length: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl DispatchCounters {
    fn wire(&self) -> std::result::Result<DispatchCountersInfo, DispatchError> {
        const OP: &str = "dispatch.counters";
        let read = |counter: &AtomicU64| number_wire(counter.load(Ordering::Relaxed), OP);
        Ok(DispatchCountersInfo {
            scan_start: read(&self.scan_start)?,
            scan_stop: read(&self.scan_stop)?,
            connect: read(&self.connect)?,
            disconnect: read(&self.disconnect)?,
            discover: read(&self.discover)?,
            read: read(&self.read)?,
            write: read(&self.write)?,
            read_descriptor: read(&self.read_descriptor)?,
            write_descriptor: read(&self.write_descriptor)?,
            subscribe: read(&self.subscribe)?,
            unsubscribe: read(&self.unsubscribe)?,
            notification_values: read(&self.notification_values)?,
            read_rssi: read(&self.read_rssi)?,
            adapter_state: read(&self.adapter_state)?,
            cancel: read(&self.cancel)?,
            security: read(&self.security)?,
            resolve_address: read(&self.resolve_address)?,
            maximum_write_length: read(&self.maximum_write_length)?,
        })
    }
}

/// One release failure from shutdown, with its error in the wire form.
#[napi(object)]
pub struct CloseFailureInfo {
    #[napi(js_name = "resourceKind")]
    pub resource_kind: String,
    pub error: String,
}

/// Authoritative shutdown outcome (`released` / `release-failed`).
#[napi(object)]
pub struct CloseReportInfo {
    pub state: String,
    pub failures: Vec<CloseFailureInfo>,
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
    #[napi(js_name = "solicitedServiceUuids")]
    pub solicited_service_uuids: Option<Vec<String>>,
    #[napi(js_name = "overflowServiceUuids")]
    pub overflow_service_uuids: Option<Vec<String>>,
    pub connectable: Option<bool>,
    /// `advertisement` (default) or `device-state`.
    pub source: Option<String>,
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
        extras: AdvertisementExtras {
            solicited_service_uuids: input.solicited_service_uuids.clone(),
            overflow_service_uuids: input.overflow_service_uuids.clone(),
            connectable: input.connectable,
            appearance: None,
            raw_record: None,
            source: observation_source(input.source.as_deref())?,
        },
    })
}

fn observation_source(
    source: Option<&str>,
) -> std::result::Result<ObservationSource, DispatchError> {
    match source {
        None | Some("advertisement") => Ok(ObservationSource::Advertisement),
        Some("device-state") => Ok(ObservationSource::DeviceState),
        Some(other) => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.stage-advertisement",
            format!("unknown observation source {other:?}"),
        )),
    }
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

/// One characteristic instance of a synthetic peer.
#[napi(object)]
pub struct StageNotificationTarget {
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
}

/// One GATT access the synthetic radio received, addressed to its exact
/// instance (synthetic only): `kind` is `write-with-response`,
/// `write-without-response`, `descriptor-read` or `descriptor-write`.
#[napi(object)]
pub struct StagedGattAccess {
    pub kind: String,
    #[napi(js_name = "peerId")]
    pub peer_id: String,
    #[napi(js_name = "serviceUuid")]
    pub service_uuid: String,
    #[napi(js_name = "serviceOccurrence")]
    pub service_occurrence: u32,
    #[napi(js_name = "characteristicUuid")]
    pub characteristic_uuid: String,
    #[napi(js_name = "characteristicOccurrence")]
    pub characteristic_occurrence: u32,
    #[napi(js_name = "descriptorUuid")]
    pub descriptor_uuid: Option<String>,
    #[napi(js_name = "descriptorOccurrence")]
    pub descriptor_occurrence: Option<u32>,
}

fn staged_occurrence(value: u64) -> std::result::Result<u32, DispatchError> {
    u32::try_from(value).map_err(|_| {
        DispatchError::new(
            BleErrorCode::ProtocolViolation.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.staged-gatt-accesses",
            "occurrence out of range",
        )
    })
}

fn staged_gatt_access(
    kind: &str,
    key: InstanceKey,
    descriptor: Option<(String, u64)>,
) -> std::result::Result<StagedGattAccess, DispatchError> {
    let (peer_id, service_uuid, service_occurrence, characteristic_uuid, characteristic_occurrence) =
        key;
    let (descriptor_uuid, descriptor_occurrence) = match descriptor {
        Some((uuid, occurrence)) => (Some(uuid), Some(staged_occurrence(occurrence)?)),
        None => (None, None),
    };
    Ok(StagedGattAccess {
        kind: kind.to_owned(),
        peer_id,
        service_uuid,
        service_occurrence: staged_occurrence(service_occurrence)?,
        characteristic_uuid,
        characteristic_occurrence: staged_occurrence(characteristic_occurrence)?,
        descriptor_uuid,
        descriptor_occurrence,
    })
}

/// Every GATT access the synthetic radio received, grouped by kind and in
/// call order within each group: characteristic writes, then descriptor
/// reads, then descriptor writes.
fn staged_gatt_accesses(
    radio: &FakeRadio,
) -> std::result::Result<Vec<StagedGattAccess>, DispatchError> {
    let writes = radio.writes().into_iter().map(|(key, with_response)| {
        let kind = if with_response {
            "write-with-response"
        } else {
            "write-without-response"
        };
        staged_gatt_access(kind, key, None)
    });
    let descriptor_reads = radio
        .descriptor_reads()
        .into_iter()
        .map(|(key, uuid, occurrence)| {
            staged_gatt_access("descriptor-read", key, Some((uuid, occurrence)))
        });
    let descriptor_writes = radio
        .descriptor_writes()
        .into_iter()
        .map(|(key, uuid, occurrence)| {
            staged_gatt_access("descriptor-write", key, Some((uuid, occurrence)))
        });
    writes
        .chain(descriptor_reads)
        .chain(descriptor_writes)
        .collect()
}

fn parse_bond(value: &str) -> std::result::Result<ubm_desktop::BondState, DispatchError> {
    match value {
        "bonded" => Ok(ubm_desktop::BondState::Bonded),
        "not-bonded" => Ok(ubm_desktop::BondState::NotBonded),
        "unknown" => Ok(ubm_desktop::BondState::Unknown),
        other => Err(DispatchError::new(
            BleErrorCode::ArgumentInvalid.as_str(),
            BleErrorDomain::Core.as_str(),
            "dispatch.bond",
            format!("unknown bond {other:?}"),
        )),
    }
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
        "rssi" => Ok(FaultOp::Rssi),
        "adapter-state" => Ok(FaultOp::AdapterState),
        "security-state" => Ok(FaultOp::SecurityState),
        "pair" => Ok(FaultOp::Pair),
        "cancel-pairing" => Ok(FaultOp::CancelPairing),
        "unpair" => Ok(FaultOp::Unpair),
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
///
/// Cancellation: JS mints a ticket with [`UbmCentral::create_ticket`]
/// (synchronous, so it exists before the operation is even called), passes
/// its id in the operation's options, cancels it with
/// [`UbmCentral::cancel_ticket`] and releases it with
/// [`UbmCentral::release_ticket`] once the operation settled. A cancel that
/// arrives before core admission is recorded and the op ends
/// `operation.aborted` without a radio call; after admission it cancels
/// exactly that one core operation.
#[napi]
pub struct UbmCentral {
    central: DesktopCentral<DispatchRadio>,
    lifecycle: AsyncMutex<broadcast::Receiver<LifecycleEvent>>,
    adapter: AsyncMutex<broadcast::Receiver<AdapterEvent>>,
    security: AsyncMutex<broadcast::Receiver<SecurityEvent>>,
    write_readiness: AsyncMutex<broadcast::Receiver<WriteReadinessEvent>>,
    scan_terminals: AsyncMutex<broadcast::Receiver<ScanTerminalEvent>>,
    adapter_resets: AsyncMutex<broadcast::Receiver<AdapterResetEvent>>,
    tickets: StdMutex<HashMap<String, OpTicket>>,
    next_ticket: AtomicU64,
    counters: DispatchCounters,
    generation: StdMutex<Option<Arc<dyn PairingGenerationController>>>,
    waker: Arc<EventWaker>,
}

/// Wakes the host when the central has new work (an advertisement, a
/// notification value, a lifecycle, adapter, reset, security,
/// write-readiness or scan-end report), so the host's
/// pumps poll at once instead of on their next interval: the legacy
/// backends delivered by callback (LEGACY-AUDIT-4 R2). Wakes coalesce: one
/// call is in flight until the host thread runs it. The JS function never
/// keeps the process alive.
#[derive(Default)]
struct EventWaker {
    pending: Arc<AtomicBool>,
    function: StdMutex<Option<Box<dyn WakeTarget>>>,
    /// Wakes that could not be queued to the host (its pumps' interval
    /// still delivers the work; this counts the lost immediacy).
    failures: AtomicU64,
}

/// The host's wake callback, type-erased so only the napi entry point
/// instantiates the thread-safe function (and its N-API release).
trait WakeTarget: Send + Sync {
    fn wake_host(&self) -> Status;
}

impl WakeTarget for ThreadsafeFunction<(), ErrorStrategy::Fatal> {
    fn wake_host(&self) -> Status {
        self.call((), ThreadsafeFunctionCallMode::NonBlocking)
    }
}

impl EventWaker {
    fn wake(&self) {
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(function) = self.function.lock() else {
            self.pending.store(false, Ordering::SeqCst);
            self.failures.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let queued = function
            .as_ref()
            .map_or(Status::Ok, |function| function.wake_host());
        if function.is_none() || queued != Status::Ok {
            self.pending.store(false, Ordering::SeqCst);
        }
        if queued != Status::Ok {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn observer(waker: &Arc<Self>) -> ubm_desktop::central::CentralObserver {
        let waker = Arc::clone(waker);
        Arc::new(move |_signal| waker.wake())
    }
}

impl UbmCentral {
    fn from_central(central: DesktopCentral<DispatchRadio>, waker: Arc<EventWaker>) -> Self {
        // Receivers subscribe before the central is handed to JS: no
        // lifecycle or adapter event can be published unobserved between
        // open and the first take.
        let lifecycle = AsyncMutex::new(central.lifecycle_events());
        let adapter = AsyncMutex::new(central.adapter_events());
        let security = AsyncMutex::new(central.security_events());
        let write_readiness = AsyncMutex::new(central.write_readiness_events());
        let scan_terminals = AsyncMutex::new(central.scan_terminal_events());
        let adapter_resets = AsyncMutex::new(central.adapter_reset_events());
        Self {
            central,
            lifecycle,
            adapter,
            security,
            write_readiness,
            scan_terminals,
            adapter_resets,
            tickets: StdMutex::new(HashMap::new()),
            next_ticket: AtomicU64::new(1),
            counters: DispatchCounters::default(),
            generation: StdMutex::new(None),
            waker,
        }
    }

    fn tickets(
        &self,
        operation: &'static str,
    ) -> std::result::Result<std::sync::MutexGuard<'_, HashMap<String, OpTicket>>, DispatchError>
    {
        self.tickets.lock().map_err(|_| {
            DispatchError::new(
                BleErrorCode::LifecycleInvariantViolation.as_str(),
                BleErrorDomain::Core.as_str(),
                operation,
                "ticket registry lock poisoned",
            )
        })
    }

    /// Budget plus ticket for one call. An unknown ticket id is loud
    /// (`argument.invalid`): an op never silently runs uncancellable.
    fn control(
        &self,
        timeout_ms: Option<u32>,
        ticket: Option<&str>,
        operation: &'static str,
    ) -> std::result::Result<OpControl, DispatchError> {
        let budget = budget_of(timeout_ms);
        let ticket = match ticket {
            None => OpTicket::new(),
            Some(id) => self.tickets(operation)?.get(id).cloned().ok_or_else(|| {
                DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    operation,
                    format!("unknown ticket {id:?}"),
                )
            })?,
        };
        Ok(OpControl::new(budget, ticket))
    }
}

fn fail(error: DesktopError) -> Error {
    to_napi(DispatchError::from(error))
}

#[napi]
impl UbmCentral {
    /// Open on the production radio for `platform` (checked against the
    /// radio this binary drives before anything opens) on the adapter
    /// `adapterId` names (`None` = the default adapter; any other adapter
    /// fails `adapter.unavailable`, never opening silently elsewhere).
    /// Without usable hardware this rejects with `adapter.unavailable`,
    /// never a synthetic fallback.
    #[napi(factory, catch_unwind)]
    pub async fn open(options: OpenOptions) -> Result<Self> {
        if options.owner.is_empty() {
            return Err(to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.open",
                "owner must not be empty",
            )));
        }
        check_platform(&options.platform).map_err(to_napi)?;
        let bus = parse_bluez_bus(options.bluez_bus.as_deref()).map_err(to_napi)?;
        let mut profile = CentralProfile::desktop(&options.owner);
        let waker = Arc::new(EventWaker::default());
        profile.observer = Some(EventWaker::observer(&waker));
        profile.adapter_id = options.adapter_id;
        profile.bluez_bus = bus;
        if options.pairing_generation.unwrap_or(false) {
            profile.register_capabilities =
                ubm_desktop::register_desktop_capabilities_with_pairing_generation;
        }
        // The radio and the central must never ride the host-owned ambient
        // runtime: napi tears its executor down during environment cleanup
        // (before finalizers run), stranding radio drops, forwarders and the
        // central's event loop on a dead handle. Both open on the shared
        // desktop executor, which outlives every central.
        let central = open_on_desktop_runtime(async move {
            let radio = BtleplugRadio::open_on(
                desktop_runtime(),
                profile.adapter_id.clone(),
                profile.bluez_bus,
            )
            .await?;
            DesktopCentral::open_with(DispatchRadio::Radio(Box::new(radio)), profile).await
        })
        .await?;
        Ok(Self::from_central(central, waker))
    }

    /// Open on the deterministic synthetic radio (hardware-free CI leg).
    /// Staging methods feed it; every op still executes the production
    /// `DesktopCentral` path (admission, core transitions, deadlines).
    #[napi(factory, catch_unwind)]
    pub async fn open_synthetic(owner: String, options: Option<SyntheticOptions>) -> Result<Self> {
        if owner.is_empty() {
            return Err(to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.open-synthetic",
                "owner must not be empty",
            )));
        }
        let mut profile = CentralProfile::desktop(&owner);
        let waker = Arc::new(EventWaker::default());
        profile.observer = Some(EventWaker::observer(&waker));
        profile.identity = Arc::new(ubm_desktop::DesktopIdentity::new("synthetic", &owner));
        let (platform, pairing_generation) = options.map_or((None, false), |options| {
            (
                options.platform,
                options.pairing_generation.unwrap_or(false),
            )
        });
        profile.register_capabilities =
            synthetic_registration(platform.as_deref(), pairing_generation).map_err(to_napi)?;
        // The synthetic radio models the named platform's legacy admission
        // and adapter-loss teardown, as the production radio does its OS's.
        let os = match platform.as_deref() {
            Some(name) => Some(desktop_os(name).map_err(to_napi)?),
            None => DesktopOs::current(),
        };
        let radio = FakeRadio::new();
        radio.set_os_policy(
            os.map_or(AdmissionPolicy::LifecycleOnly, platform_admission),
            true,
        );
        let central = open_on_desktop_runtime(async move {
            DesktopCentral::open_with(DispatchRadio::Synthetic(Box::new(radio)), profile).await
        })
        .await?;
        Ok(Self::from_central(central, waker))
    }

    /// List the adapters the OS radio stack exposes, in OS order. A listing
    /// the OS refuses rejects (`adapter.unavailable`); one unreadable
    /// adapter carries its error instead of a label.
    #[napi(catch_unwind)]
    pub async fn list_adapters(bluez_bus: Option<String>) -> Result<Vec<AdapterListingInfo>> {
        let bus = parse_bluez_bus(bluez_bus.as_deref()).map_err(to_napi)?;
        let listing = desktop_runtime()
            .spawn(ubm_desktop::btleplug_backend::list_adapters_on(bus))
            .await
            .map_err(|join| {
                to_napi(DispatchError::new(
                    BleErrorCode::LifecycleInvariantViolation.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.list-adapters",
                    format!("listing task failed: {join}"),
                ))
            })?
            .map_err(fail)?;
        listing
            .into_iter()
            .map(|entry| {
                let index = u32::try_from(entry.index)
                    .map_err(|_| to_napi(overflow_error("dispatch.list-adapters")))?;
                let (label, error) = match entry.label {
                    Ok(label) => (Some(label), None),
                    Err(error) => (None, Some(DispatchError::from(error).wire_message())),
                };
                Ok(AdapterListingInfo {
                    index,
                    label,
                    error,
                    display_name: entry.display_name,
                    default: entry.default,
                    deployment: entry
                        .deployment
                        .map(|deployment| deployment.as_str().to_owned()),
                })
            })
            .collect()
    }

    /// The adapter label this central runs on (`Adapter::adapter_info`).
    #[napi(catch_unwind)]
    pub async fn adapter_name(&self) -> Result<String> {
        self.central.boundary().adapter_name().await.map_err(fail)
    }

    /// Mint a cancellation ticket for one upcoming operation. Synchronous:
    /// the ticket exists before the operation is called, so an abort racing
    /// the call is recorded, never lost.
    #[napi(catch_unwind)]
    pub fn create_ticket(&self) -> Result<String> {
        let ordinal = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        let id = format!("ticket-{ordinal}");
        self.tickets("dispatch.create-ticket")
            .map_err(to_napi)?
            .insert(id.clone(), OpTicket::new());
        Ok(id)
    }

    /// Cancel the operation holding `ticket` (see the type docs). An
    /// unknown ticket rejects `argument.invalid`.
    #[napi(catch_unwind)]
    pub async fn cancel_ticket(&self, ticket: String) -> Result<TicketCancelInfo> {
        let handle = self
            .tickets("dispatch.cancel-ticket")
            .map_err(to_napi)?
            .get(&ticket)
            .cloned()
            .ok_or_else(|| {
                to_napi(DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.cancel-ticket",
                    format!("unknown ticket {ticket:?}"),
                ))
            })?;
        bump(&self.counters.cancel);
        let ack = self.central.cancel(&handle).await.map_err(fail)?;
        Ok(match ack {
            CancelAck::RecordedBeforeAdmission => TicketCancelInfo {
                outcome: "recorded-before-admission".to_owned(),
                operation_id: None,
                cancel: None,
            },
            CancelAck::Forwarded { operation, outcome } => TicketCancelInfo {
                outcome: "forwarded".to_owned(),
                operation_id: Some(operation.as_str().to_owned()),
                cancel: Some(cancel_info(&outcome)),
            },
            CancelAck::AlreadySettled => TicketCancelInfo {
                outcome: "already-settled".to_owned(),
                operation_id: None,
                cancel: None,
            },
        })
    }

    /// Forget a ticket once its operation settled. Returns whether it was
    /// known.
    #[napi(catch_unwind)]
    pub fn release_ticket(&self, ticket: String) -> Result<bool> {
        Ok(self
            .tickets("dispatch.release-ticket")
            .map_err(to_napi)?
            .remove(&ticket)
            .is_some())
    }

    /// Current adapter power state as the OS reports it (`powered-on` /
    /// `powered-off` / `unknown`, the OS's own answer).
    #[napi(catch_unwind)]
    pub async fn adapter_state(&self, options: Option<ControlOptions>) -> Result<String> {
        let (timeout_ms, ticket) = options.map_or((None, None), |o| (o.timeout_ms, o.ticket));
        let ctl = self
            .control(timeout_ms, ticket.as_deref(), "dispatch.adapter-state")
            .map_err(to_napi)?;
        bump(&self.counters.adapter_state);
        self.central
            .adapter_state(ctl)
            .await
            .map(|state| state.as_str().to_owned())
            .map_err(fail)
    }

    /// Take one adapter event (`null` when none is waiting).
    #[napi(catch_unwind)]
    pub async fn take_adapter_event(&self) -> Result<Option<AdapterEventInfo>> {
        const OP: &str = "dispatch.take-adapter-event";
        let mut receiver = self.adapter.lock().await;
        match receiver.try_recv() {
            Ok(event) => Ok(Some(AdapterEventInfo {
                kind: "state".to_owned(),
                sequence: Some(number_wire(event.sequence, OP).map_err(to_napi)?),
                state: Some(event.state.as_str().to_owned()),
                missed: None,
            })),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Lagged(missed)) => Ok(Some(AdapterEventInfo {
                kind: "lagged".to_owned(),
                sequence: None,
                state: None,
                missed: Some(number_wire(missed, OP).map_err(to_napi)?),
            })),
            Err(TryRecvError::Closed) => Ok(Some(AdapterEventInfo {
                kind: "closed".to_owned(),
                sequence: None,
                state: None,
                missed: None,
            })),
        }
    }

    /// Take one adapter reset (`null` when none is waiting): published after
    /// the core tore down every live resource of the lost adapter.
    #[napi(catch_unwind)]
    pub async fn take_adapter_reset_event(&self) -> Result<Option<AdapterResetEventInfo>> {
        const OP: &str = "dispatch.take-adapter-reset-event";
        let gap = |kind: &str, missed: Option<i64>| AdapterResetEventInfo {
            kind: kind.to_owned(),
            sequence: None,
            cause: None,
            previous: None,
            current: None,
            cancelled_operations: None,
            ended_scan: None,
            released_links: None,
            ended_subscriptions: None,
            release_failures: None,
            missed,
        };
        let mut receiver = self.adapter_resets.lock().await;
        match receiver.try_recv() {
            Ok(event) => adapter_reset_wire(event).map(Some).map_err(to_napi),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Lagged(missed)) => Ok(Some(gap(
                "lagged",
                Some(number_wire(missed, OP).map_err(to_napi)?),
            ))),
            Err(TryRecvError::Closed) => Ok(Some(gap("closed", None))),
        }
    }

    /// The adapter facts the core admits against: power, authorization,
    /// availability and whether a loss is in effect.
    #[napi(catch_unwind)]
    pub fn adapter_status(&self) -> Result<AdapterStatusInfo> {
        let status = self.central.adapter_status();
        Ok(AdapterStatusInfo {
            power: status.power.map(|power| power.as_str().to_owned()),
            authorization: status
                .authorization
                .map(|authorization| authorization.as_str().to_owned()),
            availability: status.availability.as_str().to_owned(),
            lost: status.lost,
        })
    }

    /// Wait at most `withinMs` for a usable adapter (finding 59): resolves
    /// `"powered-on"`, or rejects `capability.unavailable` / `adapter.initialize`
    /// with the `adapter-initialization-timed-out` detail.
    #[napi(catch_unwind)]
    pub async fn await_usable_adapter(&self, within_ms: u32) -> Result<String> {
        self.central
            .await_usable_adapter(std::time::Duration::from_millis(u64::from(within_ms)))
            .await
            .map(|state| state.as_str().to_owned())
            .map_err(fail)
    }

    /// Take one connection-lifecycle event (`null` when none is waiting).
    #[napi(catch_unwind)]
    pub async fn take_lifecycle_event(&self) -> Result<Option<LifecycleEventInfo>> {
        const OP: &str = "dispatch.take-lifecycle-event";
        let mut receiver = self.lifecycle.lock().await;
        match receiver.try_recv() {
            Ok(event) => {
                let (kind, requested) = lifecycle_kind_wire(event.kind);
                Ok(Some(LifecycleEventInfo {
                    kind: kind.to_owned(),
                    sequence: Some(number_wire(event.sequence, OP).map_err(to_napi)?),
                    peer_id: Some(event.peer_id),
                    peer_key: Some(event.peer_key),
                    connection_generation: event.connection_generation,
                    requested,
                    missed: None,
                }))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Lagged(missed)) => Ok(Some(LifecycleEventInfo {
                kind: "lagged".to_owned(),
                sequence: None,
                peer_id: None,
                peer_key: None,
                connection_generation: None,
                requested: None,
                missed: Some(number_wire(missed, OP).map_err(to_napi)?),
            })),
            Err(TryRecvError::Closed) => Ok(Some(LifecycleEventInfo {
                kind: "closed".to_owned(),
                sequence: None,
                peer_id: None,
                peer_key: None,
                connection_generation: None,
                requested: None,
                missed: None,
            })),
        }
    }

    /// The core's capability registration for a desktop platform's OS:
    /// `limited` rows are implemented (deterministic evidence at most),
    /// `unsupported` rows are not. `pairingGeneration` states whether the
    /// host supplies a privileged generation controller.
    #[napi(catch_unwind)]
    pub fn capability_states(
        platform: String,
        pairing_generation: Option<bool>,
    ) -> Result<Vec<CapabilityStateInfo>> {
        let os = desktop_os(&platform).map_err(to_napi)?;
        Ok(
            desktop_capability_states(Some(os), pairing_generation.unwrap_or(false))
                .into_iter()
                .map(|(id, state, limitation)| CapabilityStateInfo {
                    id: id.to_owned(),
                    state: state.as_str().to_owned(),
                    limitation: limitation.map(str::to_owned),
                })
                .collect(),
        )
    }

    /// Whether this process may use the adapter, as the OS reports it
    /// (`granted` / `denied` / `restricted` / `not-determined`). A platform
    /// without the concept rejects `capability.unsupported`.
    #[napi(catch_unwind)]
    pub async fn adapter_authorization(&self, options: Option<ControlOptions>) -> Result<String> {
        let (timeout_ms, ticket) = options.map_or((None, None), |o| (o.timeout_ms, o.ticket));
        let ctl = self
            .control(
                timeout_ms,
                ticket.as_deref(),
                "dispatch.adapter-authorization",
            )
            .map_err(to_napi)?;
        bump(&self.counters.adapter_state);
        self.central
            .adapter_authorization(ctl)
            .await
            .map(|authorization| authorization.as_str().to_owned())
            .map_err(fail)
    }

    /// Link-security facts for a known peer.
    #[napi(catch_unwind)]
    pub async fn security_state(&self, options: PeerControlOptions) -> Result<SecurityStateInfo> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.security-state",
            )
            .map_err(to_napi)?;
        bump(&self.counters.security);
        self.central
            .security_state(&options.peer_id, ctl)
            .await
            .map(|state| security_info(&state))
            .map_err(fail)
    }

    /// Pair through the OS ceremony (one ceremony per peer at a time).
    #[napi(catch_unwind)]
    pub async fn pair(&self, options: PairOptions) -> Result<PairOutcomeInfo> {
        let secure_connections =
            parse_secure_connections(options.secure_connections.as_deref()).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.pair",
            )
            .map_err(to_napi)?;
        bump(&self.counters.security);
        let generation_controller = self
            .generation
            .lock()
            .map_err(|_| {
                to_napi(DispatchError::new(
                    BleErrorCode::LifecycleInvariantViolation.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.pair",
                    "generation controller lock poisoned",
                ))
            })?
            .clone();
        let request = PairRequest {
            secure_connections,
            generation_controller,
        };
        self.central
            .pair(&options.peer_id, request, ctl)
            .await
            .map(pair_outcome_info)
            .map_err(fail)
    }

    /// Cancel the in-flight pairing with a peer; reports what the ceremony
    /// ended as (`cancelled` / `not-pairing` / `paired` / `rejected`).
    #[napi(catch_unwind)]
    pub async fn cancel_pairing(&self, options: PeerControlOptions) -> Result<CancelPairingInfo> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.cancel-pairing",
            )
            .map_err(to_napi)?;
        bump(&self.counters.security);
        let outcome = self
            .central
            .cancel_pairing(&options.peer_id, ctl)
            .await
            .map_err(fail)?;
        Ok(match outcome {
            CancelPairingOutcome::Cancelled => CancelPairingInfo {
                outcome: "cancelled".to_owned(),
                reason: None,
            },
            CancelPairingOutcome::NotPairing => CancelPairingInfo {
                outcome: "not-pairing".to_owned(),
                reason: None,
            },
            CancelPairingOutcome::Paired => CancelPairingInfo {
                outcome: "paired".to_owned(),
                reason: None,
            },
            CancelPairingOutcome::Rejected(reason) => CancelPairingInfo {
                outcome: "rejected".to_owned(),
                reason,
            },
        })
    }

    /// Install the host's privileged pairing-generation controller
    /// (`read(adapterId) -> Promise<generation>`, `set(adapterId,
    /// generation) -> Promise<void>`). Used only for a pair directing
    /// `secureConnections`; the core restores the previous generation after
    /// the ceremony. The functions do not keep the process alive.
    #[napi(catch_unwind)]
    pub fn install_pairing_generation_controller(
        &self,
        env: Env,
        mut read: ThreadsafeFunction<String, ErrorStrategy::Fatal>,
        mut set: ThreadsafeFunction<(String, String), ErrorStrategy::Fatal>,
    ) -> Result<()> {
        read.unref(&env)?;
        set.unref(&env)?;
        let controller: Arc<dyn PairingGenerationController> =
            Arc::new(JsGenerationController { read, set });
        *self.generation.lock().map_err(|_| {
            to_napi(DispatchError::new(
                BleErrorCode::LifecycleInvariantViolation.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.install-pairing-generation-controller",
                "generation controller lock poisoned",
            ))
        })? = Some(controller);
        Ok(())
    }

    /// Remove the OS bond with a peer (`unpaired` / `already-unpaired`).
    #[napi(catch_unwind)]
    pub async fn unpair(&self, options: PeerControlOptions) -> Result<String> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.unpair",
            )
            .map_err(to_napi)?;
        bump(&self.counters.security);
        let outcome = self
            .central
            .unpair(&options.peer_id, ctl)
            .await
            .map_err(fail)?;
        Ok(match outcome {
            UnpairOutcome::Unpaired => "unpaired".to_owned(),
            UnpairOutcome::AlreadyUnpaired => "already-unpaired".to_owned(),
        })
    }

    /// Install the host's wake callback: called (coalesced, from the JS
    /// thread) whenever the central queues new work for the host to take.
    /// It does not keep the process alive; `close` removes it.
    #[napi(catch_unwind)]
    pub fn set_event_waker(&self, env: Env, wake: JsFunction) -> Result<()> {
        let pending = Arc::clone(&self.waker.pending);
        let mut function: ThreadsafeFunction<(), ErrorStrategy::Fatal> = wake
            .create_threadsafe_function(0, move |_context: ThreadSafeCallContext<()>| {
                pending.store(false, Ordering::SeqCst);
                Ok(Vec::<u32>::new())
            })?;
        function.unref(&env)?;
        *self.waker.function.lock().map_err(|_| {
            to_napi(DispatchError::new(
                BleErrorCode::LifecycleInvariantViolation.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.set-event-waker",
                "event waker lock poisoned",
            ))
        })? = Some(Box::new(function));
        Ok(())
    }

    /// Wakes that could not be queued to the host since open.
    #[napi(catch_unwind)]
    pub fn event_wake_failures(&self) -> Result<i64> {
        number_wire(
            self.waker.failures.load(Ordering::Relaxed),
            "dispatch.event-wake-failures",
        )
        .map_err(to_napi)
    }

    /// Take one link-security event (`null` when none is waiting).
    #[napi(catch_unwind)]
    pub async fn take_security_event(&self) -> Result<Option<SecurityEventInfo>> {
        const OP: &str = "dispatch.take-security-event";
        let mut receiver = self.security.lock().await;
        match receiver.try_recv() {
            Ok(event) => Ok(Some(SecurityEventInfo {
                kind: "state".to_owned(),
                sequence: Some(number_wire(event.sequence, OP).map_err(to_napi)?),
                peer_id: Some(event.peer_id),
                state: Some(security_info(&event.state)),
                missed: None,
            })),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Lagged(missed)) => Ok(Some(SecurityEventInfo {
                kind: "lagged".to_owned(),
                sequence: None,
                peer_id: None,
                state: None,
                missed: Some(number_wire(missed, OP).map_err(to_napi)?),
            })),
            Err(TryRecvError::Closed) => Ok(Some(SecurityEventInfo {
                kind: "closed".to_owned(),
                sequence: None,
                peer_id: None,
                state: None,
                missed: None,
            })),
        }
    }

    /// Resolve an out-of-band LE address to a known radio peer id.
    #[napi(catch_unwind)]
    pub async fn resolve_address(&self, options: ResolveAddressOptions) -> Result<String> {
        let address_type = parse_address_type(&options.address_type).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.resolve-address",
            )
            .map_err(to_napi)?;
        bump(&self.counters.resolve_address);
        self.central
            .resolve_address(&options.address, address_type, ctl)
            .await
            .map_err(fail)
    }

    /// LE address type of a known peer (`null` when the OS does not say).
    #[napi(catch_unwind)]
    pub async fn address_type(&self, options: PeerControlOptions) -> Result<Option<String>> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.address-type",
            )
            .map_err(to_napi)?;
        self.central
            .address_type(&options.peer_id, ctl)
            .await
            .map(|kind| kind.map(|kind| kind.as_str().to_owned()))
            .map_err(fail)
    }

    /// The largest single write the link accepts for one mode — the limit
    /// `write` enforces. An unmeasured limit is `capability.unavailable`.
    #[napi(catch_unwind)]
    pub async fn maximum_write_length(&self, options: MaximumWriteLengthOptions) -> Result<i64> {
        const OP: &str = "dispatch.maximum-write-length";
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let ctl = self
            .control(options.timeout_ms, options.ticket.as_deref(), OP)
            .map_err(to_napi)?;
        bump(&self.counters.maximum_write_length);
        let length = self
            .central
            .maximum_write_length(
                &options.peer_id,
                &options.lease,
                &selector,
                options.with_response,
                ctl,
            )
            .await
            .map_err(fail)?;
        number_wire(length, OP).map_err(to_napi)
    }

    /// The link's largest single write for one mode, without a discovered
    /// database (legacy `maximumWriteValueLength(for:)` on the peripheral,
    /// finding 65). An unmeasured limit is `capability.unavailable`.
    #[napi(catch_unwind)]
    pub async fn connection_maximum_write_length(
        &self,
        options: ConnectionMaximumWriteLengthOptions,
    ) -> Result<i64> {
        const OP: &str = "dispatch.connection-maximum-write-length";
        let ctl = self
            .control(options.timeout_ms, options.ticket.as_deref(), OP)
            .map_err(to_napi)?;
        bump(&self.counters.maximum_write_length);
        let length = self
            .central
            .connection_maximum_write_length(
                &options.peer_id,
                &options.lease,
                options.with_response,
                ctl,
            )
            .await
            .map_err(fail)?;
        number_wire(length, OP).map_err(to_napi)
    }

    /// The vendored btleplug patches this binary links (`UBM_PATCHES.md`),
    /// in their build order: the release evidence that the parity patches
    /// are in (findings 60-63, 68).
    #[napi(catch_unwind)]
    pub fn vendored_btleplug_patches() -> Result<Vec<String>> {
        Ok(ubm_desktop::btleplug_backend::vendored_btleplug_patches()
            .into_iter()
            .map(str::to_owned)
            .collect())
    }

    /// Whether the lease's link can take a write without response now.
    #[napi(catch_unwind)]
    pub async fn write_readiness(&self, options: LeaseOptions) -> Result<bool> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.write-readiness",
            )
            .map_err(to_napi)?;
        self.central
            .write_readiness(&options.peer_id, &options.lease, ctl)
            .await
            .map_err(fail)
    }

    /// Take one write-readiness report (`null` when none is waiting).
    #[napi(catch_unwind)]
    pub async fn take_write_readiness_event(&self) -> Result<Option<WriteReadinessEventInfo>> {
        const OP: &str = "dispatch.take-write-readiness-event";
        let mut receiver = self.write_readiness.lock().await;
        let gap = |kind: &str, missed: Option<i64>| WriteReadinessEventInfo {
            kind: kind.to_owned(),
            sequence: None,
            peer_id: None,
            connection_generation: None,
            ready: None,
            missed,
        };
        match receiver.try_recv() {
            Ok(event) => Ok(Some(WriteReadinessEventInfo {
                kind: "state".to_owned(),
                sequence: Some(number_wire(event.sequence, OP).map_err(to_napi)?),
                peer_id: Some(event.peer_id),
                connection_generation: event.connection_generation,
                ready: Some(event.ready),
                missed: None,
            })),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Lagged(missed)) => Ok(Some(gap(
                "lagged",
                Some(number_wire(missed, OP).map_err(to_napi)?),
            ))),
            Err(TryRecvError::Closed) => Ok(Some(gap("closed", None))),
        }
    }

    /// Take one OS-ended scan report (`null` when none is waiting). The
    /// core already settled the scan and released its owner.
    #[napi(catch_unwind)]
    pub async fn take_scan_terminal_event(&self) -> Result<Option<ScanTerminalEventInfo>> {
        const OP: &str = "dispatch.take-scan-terminal-event";
        let mut receiver = self.scan_terminals.lock().await;
        let gap = |kind: &str, missed: Option<i64>| ScanTerminalEventInfo {
            kind: kind.to_owned(),
            sequence: None,
            operation_id: None,
            aborted: None,
            detail: None,
            missed,
        };
        match receiver.try_recv() {
            Ok(event) => Ok(Some(ScanTerminalEventInfo {
                kind: "terminal".to_owned(),
                sequence: Some(number_wire(event.sequence, OP).map_err(to_napi)?),
                operation_id: Some(event.operation_id.as_str().to_owned()),
                aborted: Some(event.aborted),
                detail: Some(event.detail),
                missed: None,
            })),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Lagged(missed)) => Ok(Some(gap(
                "lagged",
                Some(number_wire(missed, OP).map_err(to_napi)?),
            ))),
            Err(TryRecvError::Closed) => Ok(Some(gap("closed", None))),
        }
    }

    /// Every resolved radio peer with the core's connection facts, ordered
    /// by radio peer id (the lifecycle re-read after a lag, N5).
    #[napi(catch_unwind)]
    pub async fn peer_records(&self) -> Result<Vec<PeerRecordInfo>> {
        Ok(self
            .central
            .peer_records()
            .await
            .into_iter()
            .map(|record| PeerRecordInfo {
                peer_id: record.peer_id,
                peer_key: record.peer_key,
                connection_state: record
                    .connection_state
                    .map(|state| state.as_str().to_owned()),
                connection_generation: record.connection_generation,
                database_generation: record.database_generation,
                database_state: record.database_state.map(|state| state.as_str().to_owned()),
            })
            .collect())
    }

    /// The core operation id of the scan this central owns, or `null` when
    /// none is owned (the scan-terminal re-read after a lag, N5).
    #[napi(catch_unwind)]
    pub fn active_scan_id(&self) -> Result<Option<String>> {
        Ok(self
            .central
            .active_scan_id()
            .map(|id| id.as_str().to_owned()))
    }

    /// Rust-side execution counts for this central.
    #[napi(catch_unwind)]
    pub fn dispatch_counters(&self) -> Result<DispatchCountersInfo> {
        self.counters.wire().map_err(to_napi)
    }

    /// Start a scan; the session carries the backing core op id.
    #[napi(catch_unwind)]
    pub async fn start_scan(&self, options: ScanOptions) -> Result<ScanSessionInfo> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.scan-start",
            )
            .map_err(to_napi)?;
        let duplicates =
            scan_duplicate_policy(options.duplicate_policy.as_deref()).map_err(to_napi)?;
        let service_uuids = options.service_uuids.unwrap_or_default();
        let refs: Vec<&str> = service_uuids.iter().map(String::as_str).collect();
        bump(&self.counters.scan_start);
        let session = self
            .central
            .start_scan_matching(
                &options.owner,
                &refs,
                duplicates,
                options.local_name_prefix.as_deref(),
                ctl,
            )
            .await
            .map_err(fail)?;
        Ok(ScanSessionInfo {
            operation_id: session.operation_id().as_str().to_owned(),
        })
    }

    /// Stop the scan `operationId` names (the id `startScan` returned).
    /// Resolves `"stopped"` once the OS confirmed the stop, or
    /// `"not-active"` when this central holds no scan under that id (a
    /// stale id never stops a newer scan). A failed stop rejects and keeps
    /// the scan: calling again retries the OS stop.
    #[napi(catch_unwind)]
    pub async fn stop_scan(
        &self,
        operation_id: String,
        options: Option<ControlOptions>,
    ) -> Result<String> {
        let (timeout_ms, ticket) = options.map_or((None, None), |o| (o.timeout_ms, o.ticket));
        let ctl = self
            .control(timeout_ms, ticket.as_deref(), "dispatch.scan-stop")
            .map_err(to_napi)?;
        let id = OperationId::new(operation_id).map_err(|error| to_napi(error.into()))?;
        bump(&self.counters.scan_stop);
        self.central
            .stop_scan(&id, ctl)
            .await
            .map(scan_stop_wire)
            .map_err(fail)
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

    /// OS-adapter failure counts since process start (all centrals).
    #[napi(catch_unwind)]
    pub fn os_adapter_failures() -> Result<OsAdapterFailuresInfo> {
        const OP: &str = "dispatch.os-adapter-failures";
        let failures = ubm_desktop::btleplug_backend::os_adapter_failures();
        Ok(OsAdapterFailuresInfo {
            link_state_release: number_wire(failures.link_state_release, OP).map_err(to_napi)?,
            event_drops: number_wire(failures.event_drops, OP).map_err(to_napi)?,
            watch_failures: number_wire(failures.watch_failures, OP).map_err(to_napi)?,
            advertisement_read_failures: number_wire(failures.advertisement_read_failures, OP)
                .map_err(to_napi)?,
        })
    }

    /// Take one observation of the live scan with its scan id and age
    /// (`null` when none is waiting). The core queues only while a scan is
    /// live and clears the queue when one starts or stops (finding 121).
    #[napi(catch_unwind)]
    pub async fn take_scan_observation(&self) -> Result<Option<ScanObservationInfo>> {
        Ok(self
            .central
            .take_scan_observation()
            .await
            .map(|observation| ScanObservationInfo {
                advertisement: advertisement_info(&observation.snapshot),
                scan_operation_id: observation.scan_operation_id.as_str().to_owned(),
                age_ms: observation.age.as_secs_f64() * 1000.0,
            }))
    }

    /// Observations evicted past the queue cap (loss is counted, never
    /// silent).
    #[napi(catch_unwind)]
    pub fn advertisement_overflow_count(&self) -> Result<i64> {
        number_wire(
            self.central.advertisement_overflow_count(),
            "dispatch.advertisement-overflow",
        )
        .map_err(to_napi)
    }

    /// Connect to a known peer.
    #[napi(catch_unwind)]
    pub async fn connect(&self, options: ConnectOptions) -> Result<ConnectionInfo> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.connect",
            )
            .map_err(to_napi)?;
        bump(&self.counters.connect);
        let handle = self
            .central
            .connect(&options.peer_id, &options.lease, ctl)
            .await
            .map_err(fail)?;
        Ok(ConnectionInfo {
            peer_key: handle.peer_key,
            connection_generation: handle.connection_generation,
        })
    }

    /// Disconnect a connected peer: `"released"` once the OS confirmed, or
    /// `"already-released"` when the link had already ended (no radio
    /// call).
    #[napi(catch_unwind)]
    pub async fn disconnect(&self, options: LeaseOptions) -> Result<String> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.disconnect",
            )
            .map_err(to_napi)?;
        bump(&self.counters.disconnect);
        self.central
            .disconnect(&options.peer_id, &options.lease, ctl)
            .await
            .map(link_release_wire)
            .map_err(fail)
    }

    /// RSSI of the live link, in dBm, measured by the OS for this lease.
    #[napi(catch_unwind)]
    pub async fn read_rssi(&self, options: LeaseOptions) -> Result<i32> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.read-rssi",
            )
            .map_err(to_napi)?;
        bump(&self.counters.read_rssi);
        self.central
            .read_rssi(&options.peer_id, &options.lease, ctl)
            .await
            .map(i32::from)
            .map_err(fail)
    }

    /// Discover the peer database; returns the registration report.
    #[napi(catch_unwind)]
    pub async fn discover(&self, options: LeaseOptions) -> Result<DiscoveryInfo> {
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.discover",
            )
            .map_err(to_napi)?;
        bump(&self.counters.discover);
        let report = self
            .central
            .discover(&options.peer_id, &options.lease, ctl)
            .await
            .map_err(fail)?;
        let paths_registered =
            count_wire(report.paths_registered as u64, "dispatch.discover").map_err(to_napi)?;
        Ok(DiscoveryInfo { paths_registered })
    }

    /// Read the peer's current discovery tree in registration order.
    #[napi(catch_unwind)]
    pub async fn discovered_paths(&self, peer_id: String) -> Result<Vec<PathInfo>> {
        let paths = self
            .central
            .discovered_paths(&peer_id)
            .await
            .map_err(fail)?;
        paths
            .iter()
            .map(|path| path_info(path, "dispatch.discovered-paths"))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(to_napi)
    }

    /// GATT read through a validated path, with the radio's provenance.
    #[napi(catch_unwind)]
    pub async fn read(&self, options: ReadOptions) -> Result<ReadInfo> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.read",
            )
            .map_err(to_napi)?;
        bump(&self.counters.read);
        let read = self
            .central
            .read(&options.peer_id, &selector, ctl)
            .await
            .map_err(fail)?;
        Ok(ReadInfo {
            value: Buffer::from(read.value),
            provenance: read.provenance.as_str().to_owned(),
        })
    }

    /// GATT write through a validated path.
    #[napi(catch_unwind)]
    pub async fn write(&self, options: WriteOptions) -> Result<()> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let mode = write_mode(options.mode.as_deref()).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.write",
            )
            .map_err(to_napi)?;
        bump(&self.counters.write);
        self.central
            .write(
                &options.peer_id,
                &selector,
                options.value.as_ref().to_vec(),
                mode,
                ctl,
            )
            .await
            .map_err(fail)
    }

    /// GATT descriptor read through a validated path.
    #[napi(catch_unwind)]
    pub async fn read_descriptor(&self, options: ReadOptions) -> Result<Buffer> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.read-descriptor",
            )
            .map_err(to_napi)?;
        bump(&self.counters.read_descriptor);
        let value = self
            .central
            .read_descriptor(&options.peer_id, &selector, ctl)
            .await
            .map_err(fail)?;
        Ok(Buffer::from(value))
    }

    /// GATT descriptor write through a validated path.
    #[napi(catch_unwind)]
    pub async fn write_descriptor(&self, options: WriteDescriptorOptions) -> Result<()> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.write-descriptor",
            )
            .map_err(to_napi)?;
        bump(&self.counters.write_descriptor);
        self.central
            .write_descriptor(
                &options.peer_id,
                &selector,
                options.value.as_ref().to_vec(),
                ctl,
            )
            .await
            .map_err(fail)
    }

    /// Subscribe one consumer; the physical CCCD enable is owned by Rust.
    /// Resolves with the delivery the radio reported.
    #[napi(catch_unwind)]
    pub async fn subscribe(&self, options: SubscribeOptions) -> Result<SubscribeInfo> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let delivery = delivery_mode(options.delivery_mode.as_deref()).map_err(to_napi)?;
        let policy = match options.overflow_policy.as_deref() {
            None => OverflowPolicy::Error,
            Some(name) => OverflowPolicy::from_str(name).ok_or_else(|| {
                to_napi(DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.subscribe.overflow-policy",
                    format!("unknown overflow policy {name:?}"),
                ))
            })?,
        };
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.subscribe",
            )
            .map_err(to_napi)?;
        bump(&self.counters.subscribe);
        let observed: ObservedDelivery = self
            .central
            .subscribe_with_policy(
                &options.peer_id,
                &selector,
                &options.consumer,
                delivery,
                policy,
                ctl,
            )
            .await
            .map_err(fail)?;
        Ok(SubscribeInfo {
            delivery: observed.as_str().to_owned(),
        })
    }

    /// Take one queued notification value (`null` when none is queued).
    /// `null` hides live-empty vs terminal vs closed; prefer
    /// [`UbmCentral::poll_notification`].
    #[napi(catch_unwind)]
    pub async fn take_notification(&self, options: SubscriptionOptions) -> Result<Option<Buffer>> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let value = self
            .central
            .take_notification(&options.peer_id, &selector, &options.consumer)
            .await
            .map_err(fail)?;
        if value.is_some() {
            bump(&self.counters.notification_values);
        }
        Ok(value.map(Buffer::from))
    }

    /// Notifications this central's radio intake dropped before any
    /// subscription could hold them (finding 131), cumulative. Each is also
    /// charged to its subscription under the consumer's overflow policy.
    #[napi(catch_unwind)]
    pub async fn ingress_notification_drops(&self) -> Result<i64> {
        number_wire(
            self.central
                .resource_counters()
                .await
                .ingress_notification_drops,
            "dispatch.ingress-notification-drops",
        )
        .map_err(to_napi)
    }

    /// One consumer's cumulative stream accounting (`null` when the core
    /// holds no record for it): drops, replacements and upstream loss the
    /// host surfaces under the consumer's own overflow policy (finding 131).
    #[napi(catch_unwind)]
    pub async fn consumer_counters(
        &self,
        options: SubscriptionOptions,
    ) -> Result<Option<ConsumerCountersInfo>> {
        const OP: &str = "dispatch.consumer-counters";
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let accounting = self
            .central
            .consumer_counters(&options.peer_id, &selector, &options.consumer)
            .await
            .map_err(fail)?;
        accounting
            .map(|counts| {
                Ok(ConsumerCountersInfo {
                    dropped_items: number_wire(counts.dropped_oldest(), OP)?,
                    dropped_bytes: number_wire(counts.dropped_bytes(), OP)?,
                    replaced_items: number_wire(counts.replaced(), OP)?,
                    upstream_lost: number_wire(counts.upstream_lost(), OP)?,
                    terminated: counts.terminated(),
                })
            })
            .transpose()
            .map_err(to_napi)
    }

    /// Poll one consumer's stream with a typed outcome (F17).
    #[napi(catch_unwind)]
    pub async fn poll_notification(
        &self,
        options: SubscriptionOptions,
    ) -> Result<NotificationPollInfo> {
        const OP: &str = "dispatch.poll-notification";
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let poll = self
            .central
            .poll_notification(&options.peer_id, &selector, &options.consumer)
            .await
            .map_err(fail)?;
        let empty = |kind: &str, cause: Option<&str>| NotificationPollInfo {
            kind: kind.to_owned(),
            value: None,
            cause: cause.map(str::to_owned),
            dropped_items: None,
            dropped_bytes: None,
            replaced_items: None,
        };
        Ok(match poll {
            NotificationPoll::Value(value) => {
                bump(&self.counters.notification_values);
                NotificationPollInfo {
                    value: Some(Buffer::from(value)),
                    ..empty("value", None)
                }
            }
            NotificationPoll::Empty => empty("empty", None),
            NotificationPoll::Terminal(terminal) => NotificationPollInfo {
                dropped_items: Some(number_wire(terminal.dropped_items(), OP).map_err(to_napi)?),
                dropped_bytes: Some(number_wire(terminal.dropped_bytes(), OP).map_err(to_napi)?),
                replaced_items: Some(number_wire(terminal.replaced_items(), OP).map_err(to_napi)?),
                ..empty("terminal", Some(terminal.reason()))
            },
            NotificationPoll::Invalidated(InvalidationCause::ServicesChanged) => {
                empty("invalidated", Some("services-changed"))
            }
            NotificationPoll::Invalidated(InvalidationCause::LinkEnded) => {
                empty("invalidated", Some("link-ended"))
            }
            NotificationPoll::Invalidated(InvalidationCause::AdapterReset) => {
                empty("invalidated", Some("adapter-reset"))
            }
            NotificationPoll::Closed => empty("closed", None),
        })
    }

    /// Unsubscribe one consumer; returns whether the physical CCCD was
    /// disabled (true when the last consumer leaves).
    #[napi(catch_unwind)]
    pub async fn unsubscribe(&self, options: SubscriptionOptions) -> Result<bool> {
        let selector = selector_of(&options.selector).map_err(to_napi)?;
        let ctl = self
            .control(
                options.timeout_ms,
                options.ticket.as_deref(),
                "dispatch.unsubscribe",
            )
            .map_err(to_napi)?;
        bump(&self.counters.unsubscribe);
        self.central
            .unsubscribe(&options.peer_id, &selector, &options.consumer, ctl)
            .await
            .map_err(fail)
    }

    /// Cancel one live operation by core op id; returns the winning
    /// terminal, never a guess. Prefer tickets, which also cover a cancel
    /// that arrives before the id exists.
    #[napi(catch_unwind)]
    pub async fn cancel_operation(&self, operation_id: String) -> Result<CancelInfo> {
        let id = OperationId::new(operation_id).map_err(|error| to_napi(error.into()))?;
        bump(&self.counters.cancel);
        let outcome = self.central.cancel_operation(&id).await.map_err(fail)?;
        Ok(cancel_info(&outcome))
    }

    /// Shut the central down: stops the scan, releases ops, closes the
    /// radio. Idempotent; later ops refuse loudly. Resolves with the
    /// authoritative shutdown report: every release failure the core
    /// recorded, every radio scope that did not release, and a failed final
    /// scan stop — never a clean answer over a failed release.
    #[napi(catch_unwind)]
    pub async fn close(&self) -> Result<CloseReportInfo> {
        let report = self.central.shutdown().await;
        if let Ok(mut function) = self.waker.function.lock() {
            function.take();
        }
        let mut failures = Vec::new();
        let released = match &report.record {
            Err(error) => {
                failures.push(CloseFailureInfo {
                    resource_kind: "central".to_owned(),
                    error: DispatchError::from(error.clone()).wire_message(),
                });
                false
            }
            Ok(record) => {
                for failure in record.failures() {
                    failures.push(CloseFailureInfo {
                        resource_kind: failure.resource_kind().to_owned(),
                        error: DispatchError::new(
                            failure.code().as_str(),
                            BleErrorDomain::Cleanup.as_str(),
                            "dispatch.close",
                            "",
                        )
                        .wire_message(),
                    });
                }
                record.state() == CleanupState::Released
            }
        };
        for failure in &report.radio_close_failures {
            failures.push(CloseFailureInfo {
                resource_kind: "subscription".to_owned(),
                error: DispatchError::new(
                    BleErrorCode::PlatformFailure.as_str(),
                    BleErrorDomain::Cleanup.as_str(),
                    "dispatch.close.radio-scope",
                    format!("{:?}: {}", failure.scope, failure.detail),
                )
                .wire_message(),
            });
        }
        if let Some(error) = &report.scan_stop_failure {
            failures.push(CloseFailureInfo {
                resource_kind: "scan".to_owned(),
                error: DispatchError::from(error.clone()).wire_message(),
            });
        }
        let state = if released && failures.is_empty() {
            "released"
        } else {
            "release-failed"
        };
        Ok(CloseReportInfo {
            state: state.to_owned(),
            failures,
        })
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

    /// Stage the radio losing `lost` notifications of one characteristic
    /// instance before they reached the central (synthetic only; finding
    /// 131): the core applies each consumer's overflow policy.
    #[napi(catch_unwind)]
    pub async fn stage_notifications_lost(
        &self,
        input: StageNotificationTarget,
        lost: u32,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-notifications-lost")
            .map_err(to_napi)?;
        radio.push_event(RadioEvent::NotificationsLost {
            peer_id: input.peer_id,
            service_uuid: input.service_uuid,
            service_occurrence: u64::from(input.service_occurrence.unwrap_or(0)),
            characteristic_uuid: input.characteristic_uuid,
            characteristic_occurrence: u64::from(input.characteristic_occurrence.unwrap_or(0)),
            epoch: 0,
            lost: u64::from(lost),
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

    /// Script the next call of one synthetic radio op to fail with `detail`
    /// (synthetic only).
    #[napi(catch_unwind)]
    pub async fn fail_next_radio_op(&self, op: String, detail: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.fail-next-radio-op")
            .map_err(to_napi)?;
        let parsed = fault_op(&op).map_err(to_napi)?;
        radio.fail_next(parsed, &detail);
        Ok(())
    }

    /// Script the next call of one synthetic radio op to fail with `detail`
    /// and the platform's structured answer (synthetic only, finding 113).
    #[napi(catch_unwind)]
    pub async fn fail_next_radio_op_with_platform(
        &self,
        op: String,
        detail: String,
        platform: StagePlatformDetail,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.fail-next-radio-op-with-platform")
            .map_err(to_napi)?;
        let parsed = fault_op(&op).map_err(to_napi)?;
        let mut staged = PlatformDetail::new(platform.domain, platform.code);
        if let Some(message) = platform.message {
            staged = staged.with_message(message);
        }
        for (key, value) in platform.metadata.unwrap_or_default() {
            let value = match value {
                Either3::A(text) => PlatformValue::Text(text),
                Either3::B(number) => PlatformValue::Int(number),
                Either3::C(flag) => PlatformValue::Bool(flag),
            };
            staged = staged.with_metadata(key, value);
        }
        radio.fail_next_with_platform(parsed, &detail, staged);
        Ok(())
    }

    /// Stage the synthetic connected RSSI for one peer (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_rssi(&self, peer_id: String, rssi: i32) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-rssi")
            .map_err(to_napi)?;
        let rssi = i16::try_from(rssi).map_err(|_| {
            to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.stage-rssi",
                "rssi out of range",
            ))
        })?;
        radio.set_rssi(&peer_id, rssi);
        Ok(())
    }

    /// Stage what the synthetic radio says characteristic reads are
    /// (`read-response` | `read-or-notification`, synthetic only): models a
    /// radio that reports read responses and notifications through one
    /// callback (CoreBluetooth reading a notifying characteristic).
    #[napi(catch_unwind)]
    pub async fn stage_read_provenance(&self, provenance: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-read-provenance")
            .map_err(to_napi)?;
        let provenance = ubm_desktop::ReadProvenance::from_wire(&provenance).ok_or_else(|| {
            to_napi(DispatchError::new(
                BleErrorCode::ArgumentInvalid.as_str(),
                BleErrorDomain::Core.as_str(),
                "dispatch.stage-read-provenance",
                "provenance must be read-response or read-or-notification",
            ))
        })?;
        radio.script_read_provenance(provenance);
        Ok(())
    }

    /// Stage the synthetic adapter power state (synthetic only); with
    /// `announce` the radio also reports the change as an OS event.
    #[napi(catch_unwind)]
    pub async fn stage_adapter_state(&self, state: String, announce: Option<bool>) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-adapter-state")
            .map_err(to_napi)?;
        let parsed =
            parse_adapter_power(&state, "dispatch.stage-adapter-state").map_err(to_napi)?;
        radio.set_adapter_state(parsed);
        if announce.unwrap_or(false) {
            radio.push_event(RadioEvent::AdapterState(parsed));
        }
        Ok(())
    }

    /// Stage an OS-reported link loss for one peer (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_link_loss(&self, peer_id: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-link-loss")
            .map_err(to_napi)?;
        radio.push_event(RadioEvent::Disconnected(peer_id));
        Ok(())
    }

    /// Stage an OS-reported GATT database change for one peer (synthetic
    /// only).
    #[napi(catch_unwind)]
    pub async fn stage_services_changed(&self, peer_id: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-services-changed")
            .map_err(to_napi)?;
        radio.push_event(RadioEvent::ServicesChanged(peer_id));
        Ok(())
    }

    /// Script the delivery the synthetic radio reports for later CCCD
    /// enables (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_observed_delivery(&self, delivery: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-observed-delivery")
            .map_err(to_napi)?;
        let observed = match delivery.as_str() {
            "notification" => ObservedDelivery::Notification,
            "indication" => ObservedDelivery::Indication,
            "unknown" => ObservedDelivery::Unknown,
            other => {
                return Err(to_napi(DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.stage-observed-delivery",
                    format!("unknown delivery {other:?}"),
                )))
            }
        };
        radio.script_observed_delivery(observed);
        Ok(())
    }

    /// Delivery requirements the synthetic radio received, in order
    /// (`null` = no requirement; synthetic only).
    #[napi(catch_unwind)]
    pub async fn staged_delivery_requests(&self) -> Result<Vec<Option<String>>> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.staged-delivery-requests")
            .map_err(to_napi)?;
        Ok(radio
            .delivery_requests()
            .into_iter()
            .map(|mode| mode.map(|mode| mode.as_str().to_owned()))
            .collect())
    }

    /// Stage the synthetic adapter authorization (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_adapter_authorization(
        &self,
        authorization: String,
        announce: Option<bool>,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-adapter-authorization")
            .map_err(to_napi)?;
        let parsed = match authorization.as_str() {
            "granted" => AdapterAuthorization::Granted,
            "denied" => AdapterAuthorization::Denied,
            "restricted" => AdapterAuthorization::Restricted,
            "not-determined" => AdapterAuthorization::NotDetermined,
            other => {
                return Err(to_napi(DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.stage-adapter-authorization",
                    format!("unknown authorization {other:?}"),
                )))
            }
        };
        radio.set_adapter_authorization(parsed);
        if announce.unwrap_or(false) {
            radio.push_event(RadioEvent::AdapterAuthorization(parsed));
        }
        Ok(())
    }

    /// Stage an adapter loss the power state does not show (synthetic
    /// only): `removed` / `daemon-restarted` (or any loss cause).
    #[napi(catch_unwind)]
    pub async fn stage_adapter_reset(&self, cause: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-adapter-reset")
            .map_err(to_napi)?;
        radio.push_event(RadioEvent::AdapterLost(
            parse_loss_cause(&cause).map_err(to_napi)?,
        ));
        Ok(())
    }

    /// The duplicate policy each synthetic scan start handed the radio, in
    /// order (what the OS scan filter would have applied).
    #[napi(catch_unwind)]
    pub async fn staged_scan_duplicate_policies(&self) -> Result<Vec<String>> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.staged-scan-duplicate-policies")
            .map_err(to_napi)?;
        Ok(radio
            .scan_filters()
            .into_iter()
            .map(|filter| filter.duplicates.as_str().to_owned())
            .collect())
    }

    /// Every call the synthetic radio received, in order (synthetic only):
    /// a harness waits on it to know an operation reached the radio.
    #[napi(catch_unwind)]
    pub async fn staged_radio_calls(&self) -> Result<Vec<String>> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.staged-radio-calls")
            .map_err(to_napi)?;
        Ok(radio.calls())
    }

    /// The OS name-prefix filter each synthetic scan start handed the radio,
    /// in order (`null` = none; the BlueZ `Pattern`, finding 89).
    #[napi(catch_unwind)]
    pub async fn staged_scan_name_prefixes(&self) -> Result<Vec<Option<String>>> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.staged-scan-name-prefixes")
            .map_err(to_napi)?;
        Ok(radio
            .scan_filters()
            .into_iter()
            .map(|filter| filter.name_prefix)
            .collect())
    }

    /// Stage a peer's link-security facts (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_security(
        &self,
        peer_id: String,
        bond: String,
        pairing_possible: Option<bool>,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-security")
            .map_err(to_napi)?;
        radio.set_security(
            &peer_id,
            SecurityState {
                bond: parse_bond(&bond).map_err(to_napi)?,
                pairing_possible,
            },
        );
        Ok(())
    }

    /// Script what the next ceremony with a peer ends as (synthetic only):
    /// `paired` / `already-paired` (with `bond`), `rejected` (with
    /// `reason`) or `cancelled`.
    #[napi(catch_unwind)]
    pub async fn stage_pair_outcome(
        &self,
        peer_id: String,
        outcome: String,
        reason: Option<String>,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-pair-outcome")
            .map_err(to_napi)?;
        let bonded = SecurityState {
            bond: ubm_desktop::BondState::Bonded,
            pairing_possible: Some(true),
        };
        let parsed = match outcome.as_str() {
            "paired" => PairOutcome::Paired(bonded),
            "already-paired" => PairOutcome::AlreadyPaired(bonded),
            "rejected" => PairOutcome::Rejected(reason),
            "cancelled" => PairOutcome::Cancelled,
            other => {
                return Err(to_napi(DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.stage-pair-outcome",
                    format!("unknown pair outcome {other:?}"),
                )))
            }
        };
        radio.script_pair_outcome(&peer_id, parsed);
        Ok(())
    }

    /// Stage per-mode write limits for a peer (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_write_limits(
        &self,
        peer_id: String,
        with_response: u32,
        without_response: u32,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-write-limits")
            .map_err(to_napi)?;
        let bound = |value: u32| {
            u16::try_from(value).map_err(|_| {
                to_napi(DispatchError::new(
                    BleErrorCode::ArgumentInvalid.as_str(),
                    BleErrorDomain::Core.as_str(),
                    "dispatch.stage-write-limits",
                    "limit out of range",
                ))
            })
        };
        radio.set_write_limits(
            &peer_id,
            WriteLimits {
                with_response: bound(with_response)?,
                without_response: bound(without_response)?,
            },
        );
        Ok(())
    }

    /// Stage an address the synthetic radio resolves to `peer_id`, with its
    /// type (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_address(
        &self,
        address: String,
        address_type: String,
        peer_id: String,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-address")
            .map_err(to_napi)?;
        let parsed = parse_address_type(&address_type).map_err(to_napi)?;
        radio.set_address(&address, parsed, &peer_id);
        radio.set_address_type(&peer_id, parsed);
        Ok(())
    }

    /// Stage characteristic facts beyond the core bits for one
    /// characteristic instance of a peer (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_characteristic_access(
        &self,
        input: StageNotificationTarget,
        access: CharacteristicAccessInfo,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-characteristic-access")
            .map_err(to_napi)?;
        let key: InstanceKey = (
            input.peer_id.clone(),
            input.service_uuid,
            u64::from(input.service_occurrence.unwrap_or(0)),
            input.characteristic_uuid,
            u64::from(input.characteristic_occurrence.unwrap_or(0)),
        );
        let mut map = HashMap::new();
        map.insert(key, access_of(&access));
        radio.set_characteristic_access(&input.peer_id, map);
        Ok(())
    }

    /// Stage the value the synthetic radio answers when one characteristic
    /// instance is read (synthetic only): proves reads route to the exact
    /// instance a complete path names.
    #[napi(catch_unwind)]
    pub async fn stage_characteristic_value(
        &self,
        target: StageNotificationTarget,
        value: Buffer,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-characteristic-value")
            .map_err(to_napi)?;
        radio.set_characteristic_value(
            &target.peer_id,
            &target.service_uuid,
            u64::from(target.service_occurrence.unwrap_or(0)),
            &target.characteristic_uuid,
            u64::from(target.characteristic_occurrence.unwrap_or(0)),
            value.as_ref().to_vec(),
        );
        Ok(())
    }

    /// Every characteristic write, descriptor read and descriptor write the
    /// synthetic radio received, each with its exact instance (synthetic
    /// only).
    #[napi(catch_unwind)]
    pub async fn staged_gatt_accesses(&self) -> Result<Vec<StagedGattAccess>> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.staged-gatt-accesses")
            .map_err(to_napi)?;
        staged_gatt_accesses(radio).map_err(to_napi)
    }

    /// Stage the synthetic write-without-response readiness for a peer;
    /// with `announce` the radio also reports it as an OS event (synthetic
    /// only).
    #[napi(catch_unwind)]
    pub async fn stage_write_readiness(
        &self,
        peer_id: String,
        ready: bool,
        announce: Option<bool>,
    ) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-write-readiness")
            .map_err(to_napi)?;
        radio.set_write_readiness(&peer_id, ready);
        if announce.unwrap_or(false) {
            radio.push_event(RadioEvent::WriteReadiness { peer_id, ready });
        }
        Ok(())
    }

    /// Stage the OS ending the active scan on its own (synthetic only).
    #[napi(catch_unwind)]
    pub async fn stage_scan_terminated(&self, aborted: bool, detail: String) -> Result<()> {
        let radio = self
            .central
            .boundary()
            .synthetic("dispatch.stage-scan-terminated")
            .map_err(to_napi)?;
        radio.push_event(RadioEvent::ScanTerminated { aborted, detail });
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
            "gatt.read-failed|gatt|dispatch.read|never|||os-eio"
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
            "peer.not-found|connection|peer.known|never|||"
        );
    }

    #[test]
    fn platform_detail_crosses_the_wire_as_one_pipe_free_json_field() {
        let error = DispatchError::from(
            DesktopError::new(
                BleErrorCode::GattReadFailed,
                BleErrorDomain::Gatt,
                "gatt.read",
            )
            .with_detail("a|b")
            .with_platform(
                PlatformDetail::new("winrt", "unreachable")
                    .with_message("GATT \"x\" | y")
                    .with_metadata("hresult", PlatformValue::Text("0x80650002".to_owned()))
                    .with_metadata("gattStatus", PlatformValue::Text("unreachable".to_owned()))
                    .with_metadata("attStatus", PlatformValue::Int(5))
                    .with_metadata("huge", PlatformValue::Int(i64::MAX))
                    .with_metadata("bonded", PlatformValue::Bool(false)),
            ),
        );
        assert_eq!(
            error.wire_message(),
            "gatt.read-failed|gatt|gatt.read|never||{\"domain\":\"winrt\",\"code\":\"unreachable\",\"message\":\"GATT \\\"x\\\" \\u007c y\",\"metadata\":{\"attStatus\":5,\"bonded\":false,\"gattStatus\":\"unreachable\",\"hresult\":\"0x80650002\",\"huge\":\"9223372036854775807\"}}|a|b"
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
            "rssi",
            "adapter-state",
            "security-state",
            "pair",
            "cancel-pairing",
            "unpair",
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
            extras: AdvertisementExtras {
                solicited_service_uuids: Some(vec![HRM_SERVICE.to_owned()]),
                overflow_service_uuids: None,
                connectable: Some(true),
                appearance: None,
                raw_record: None,
                source: ObservationSource::Advertisement,
            },
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
        assert_eq!(
            info.solicited_service_uuids,
            Some(vec![HRM_SERVICE.to_owned()])
        );
        assert_eq!(
            info.overflow_service_uuids, None,
            "not carried stays null, never []"
        );
        assert_eq!(info.connectable, Some(true));
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
            solicited_service_uuids: None,
            overflow_service_uuids: None,
            connectable: None,
            source: None,
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

    #[test]
    fn dispatch_radio_forwards_the_unflagged_subscribe_answer() {
        for answers in [false, true] {
            let radio = FakeRadio::new();
            radio.set_os_answers_unflagged_subscribe(answers);
            let dispatch = DispatchRadio::Synthetic(Box::new(radio));
            assert_eq!(dispatch.os_answers_unflagged_subscribe(), answers);
        }
    }

    #[tokio::test]
    async fn staged_gatt_accesses_report_exact_instances_grouped_by_kind() {
        let radio = FakeRadio::new();
        let level = "00002a19-0000-1000-8000-00805f9b34fb";
        let battery = "0000180f-0000-1000-8000-00805f9b34fb";
        let user_description = "00002901-0000-1000-8000-00805f9b34fb";
        radio
            .write_characteristic("peer-1", battery, 1, level, 0, vec![1], true)
            .await
            .expect("write");
        radio
            .write_characteristic("peer-1", battery, 0, level, 1, vec![2], false)
            .await
            .expect("write without response");
        radio
            .read_descriptor("peer-1", battery, 0, level, 0, user_description, 1)
            .await
            .expect("descriptor read");
        radio
            .write_descriptor("peer-1", battery, 0, level, 0, user_description, 0, vec![3])
            .await
            .expect("descriptor write");
        let accesses = staged_gatt_accesses(&radio).expect("accesses");
        let summary: Vec<(String, u32, u32, Option<u32>)> = accesses
            .iter()
            .map(|access| {
                (
                    access.kind.clone(),
                    access.service_occurrence,
                    access.characteristic_occurrence,
                    access.descriptor_occurrence,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("write-with-response".to_owned(), 1, 0, None),
                ("write-without-response".to_owned(), 0, 1, None),
                ("descriptor-read".to_owned(), 0, 0, Some(1)),
                ("descriptor-write".to_owned(), 0, 0, Some(0)),
            ]
        );
        assert!(accesses
            .iter()
            .all(|access| access.peer_id == "peer-1" && access.service_uuid == battery));
        assert_eq!(
            accesses[2].descriptor_uuid.as_deref(),
            Some(user_description)
        );
    }

    #[test]
    fn staged_occurrences_beyond_u32_fail_loudly() {
        let error = staged_occurrence(u64::from(u32::MAX) + 1).expect_err("out of range");
        assert!(error
            .wire_message()
            .starts_with("protocol.violation|core|dispatch.staged-gatt-accesses|"));
    }

    #[tokio::test]
    async fn synthetic_flow_runs_ops_in_rust() {
        let central = UbmCentral::open_synthetic("dispatch-test-a".to_owned(), None)
            .await
            .expect("open");
        let session = central
            .start_scan(ScanOptions {
                owner: "dispatch-test-a".to_owned(),
                duplicate_policy: None,
                local_name_prefix: None,
                service_uuids: None,
                timeout_ms: Some(5000),
                ticket: None,
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
                solicited_service_uuids: None,
                overflow_service_uuids: None,
                connectable: None,
                source: None,
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
        central
            .stop_scan(session.operation_id.clone(), None)
            .await
            .expect("stop");
        let handle = central
            .connect(ConnectOptions {
                peer_id: "peer-1".to_owned(),
                lease: "lease-a".to_owned(),
                timeout_ms: Some(5000),
                ticket: None,
            })
            .await
            .expect("connect");
        assert!(!handle.peer_key.is_empty());
        central
            .stage_services("peer-1".to_owned(), hrm_services())
            .await
            .expect("stage services");
        let report = central
            .discover(LeaseOptions {
                peer_id: "peer-1".to_owned(),
                lease: "lease-a".to_owned(),
                timeout_ms: None,
                ticket: None,
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
                timeout_ms: Some(5000),
                ticket: None,
            })
            .await
            .expect("read");
        assert_eq!(value.value.as_ref(), &[0x42u8][..]);
        assert_eq!(value.provenance, "read-response");
        central
            .stage_read_provenance("read-or-notification".to_owned())
            .await
            .expect("stage provenance");
        let fused = central
            .read(ReadOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                timeout_ms: Some(5000),
                ticket: None,
            })
            .await
            .expect("read while notifying");
        assert_eq!(fused.provenance, "read-or-notification");
        assert!(
            central
                .stage_read_provenance("notification".to_owned())
                .await
                .is_err(),
            "an unknown provenance word is refused"
        );
        central
            .stage_read_provenance("read-response".to_owned())
            .await
            .expect("restore provenance");
        central
            .subscribe(SubscribeOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                consumer: "app".to_owned(),
                delivery_mode: None,
                overflow_policy: None,
                timeout_ms: Some(5000),
                ticket: None,
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
                    timeout_ms: None,
                    ticket: None,
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
                timeout_ms: None,
                ticket: None,
            })
            .await
            .expect("unsubscribe");
        assert!(disabled);
        central
            .disconnect(LeaseOptions {
                peer_id: "peer-1".to_owned(),
                lease: "lease-a".to_owned(),
                timeout_ms: None,
                ticket: None,
            })
            .await
            .expect("disconnect");
        central.close().await.expect("close");
    }

    #[tokio::test]
    async fn synthetic_timeout_and_dispose_refuse_loudly() {
        let central = UbmCentral::open_synthetic("dispatch-test-b".to_owned(), None)
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
                solicited_service_uuids: None,
                overflow_service_uuids: None,
                connectable: None,
                source: None,
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
                timeout_ms: Some(200),
                ticket: None,
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("blocked connect must time out"),
        };
        // Finding 161: a connect whose deadline expires before any link came
        // up is `connection.failed` (caller-decides) on every host.
        assert!(
            error
                .reason
                .starts_with("connection.failed|connection|connection.connect|caller-decides|"),
            "{}",
            error.reason
        );
        assert!(
            error.reason.contains("\"code\":\"deadline-expired\""),
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
                duplicate_policy: None,
                local_name_prefix: None,
                service_uuids: None,
                timeout_ms: Some(100),
                ticket: None,
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

    #[tokio::test]
    async fn synthetic_stop_scan_targets_only_its_own_scan() {
        let central = UbmCentral::open_synthetic("dispatch-test-c".to_owned(), None)
            .await
            .expect("open");
        let first = central
            .start_scan(ScanOptions {
                owner: "dispatch-test-c".to_owned(),
                duplicate_policy: None,
                local_name_prefix: None,
                service_uuids: None,
                timeout_ms: Some(5000),
                ticket: None,
            })
            .await
            .expect("scan");
        assert_eq!(
            central
                .stop_scan(first.operation_id.clone(), None)
                .await
                .expect("stop"),
            "stopped"
        );
        let second = central
            .start_scan(ScanOptions {
                owner: "dispatch-test-c".to_owned(),
                duplicate_policy: None,
                local_name_prefix: None,
                service_uuids: None,
                timeout_ms: Some(5000),
                ticket: None,
            })
            .await
            .expect("restart");
        assert_eq!(
            central
                .stop_scan(first.operation_id.clone(), None)
                .await
                .expect("stale stop"),
            "not-active",
            "a stale scan id never stops the newer scan"
        );
        assert_eq!(
            central
                .stop_scan(second.operation_id.clone(), None)
                .await
                .expect("own stop"),
            "stopped"
        );
        central.close().await.expect("close");
    }

    async fn connected_synthetic(owner: &str) -> UbmCentral {
        let central = UbmCentral::open_synthetic(owner.to_owned(), None)
            .await
            .expect("open");
        central
            .stage_advertisement(StageAdvertisementInput {
                peer_id: "peer-1".to_owned(),
                address: None,
                rssi: Some(-60),
                local_name: None,
                service_uuids: None,
                manufacturer_data: None,
                service_data: None,
                tx_power: None,
                solicited_service_uuids: None,
                overflow_service_uuids: None,
                connectable: None,
                source: None,
            })
            .await
            .expect("stage");
        let mut connected = None;
        for _ in 0..200 {
            match central
                .connect(ConnectOptions {
                    peer_id: "peer-1".to_owned(),
                    lease: "lease-a".to_owned(),
                    timeout_ms: Some(5000),
                    ticket: None,
                })
                .await
            {
                Ok(handle) => {
                    connected = Some(handle);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        connected.expect("connect after the observation is ingested");
        central
    }

    fn lease(timeout_ms: Option<u32>) -> LeaseOptions {
        LeaseOptions {
            peer_id: "peer-1".to_owned(),
            lease: "lease-a".to_owned(),
            timeout_ms,
            ticket: None,
        }
    }

    #[test]
    fn platform_check_accepts_only_the_compiled_radio() {
        let compiled = compiled_platform().expect("desktop test host");
        check_platform(compiled).expect("compiled platform opens");
        for other in ["bluez", "corebluetooth", "winrt"] {
            if other == compiled {
                continue;
            }
            let error = check_platform(other).expect_err("foreign platform");
            assert!(
                error
                    .wire_message()
                    .starts_with("capability.unavailable|platform|dispatch.open.platform|"),
                "{}",
                error.wire_message()
            );
        }
        let error = check_platform("android").expect_err("unknown platform");
        assert!(error.wire_message().starts_with("argument.invalid|core|"));
    }

    #[test]
    fn delivery_mode_parses_requirements_and_rejects_loudly() {
        assert_eq!(delivery_mode(None).expect("none"), None);
        assert_eq!(
            delivery_mode(Some("indication")).expect("indication"),
            Some(DeliveryMode::Indication)
        );
        assert_eq!(
            delivery_mode(Some("notification")).expect("notification"),
            Some(DeliveryMode::Notification)
        );
        let error = delivery_mode(Some("prefer-indication")).expect_err("preference is TS-side");
        assert!(error.wire_message().starts_with("argument.invalid|core|"));
    }

    #[test]
    fn desktop_error_carries_outcome_facts_on_the_wire() {
        let error = DispatchError::from(
            DesktopError::new(
                BleErrorCode::OperationTimedOut,
                BleErrorDomain::Gatt,
                "gatt.write",
            )
            .with_outcome(Some(CommitState::Unknown), Retryability::Never)
            .with_detail("late"),
        );
        assert_eq!(
            error.wire_message(),
            "operation.timed-out|gatt|gatt.write|never|unknown||late"
        );
        let retry = DispatchError::from(
            DesktopError::new(
                BleErrorCode::OperationAborted,
                BleErrorDomain::Gatt,
                "gatt.read",
            )
            .with_outcome(
                Some(CommitState::NotDispatched),
                Retryability::CallerDecides,
            ),
        );
        assert_eq!(
            retry.wire_message(),
            "operation.aborted|gatt|gatt.read|caller-decides|not-dispatched||"
        );
    }

    #[test]
    fn build_identity_is_the_frozen_schema_with_the_core_revision() {
        let identity = crate::native_build_identity();
        assert!(identity.starts_with(
            "{\"schema\":\"ubm-native-build-identity/1\",\"binding\":\"napi\",\"contractRevision\":\""
        ));
        assert!(identity.contains(ubm_core::contracts::CONTRACT_REVISION));
        assert!(identity.contains("\"sourceDigest\":"));
        assert!(identity.contains("\"bindingSchema\":"));
    }

    #[tokio::test]
    async fn ticket_cancel_before_admission_aborts_without_a_radio_call() {
        let central = connected_synthetic("dispatch-test-ticket").await;
        let ticket = central.create_ticket().expect("ticket");
        let ack = central.cancel_ticket(ticket.clone()).await.expect("cancel");
        assert_eq!(ack.outcome, "recorded-before-admission");
        let error = match central
            .read(ReadOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                timeout_ms: Some(5000),
                ticket: Some(ticket.clone()),
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("a pre-cancelled read must abort"),
        };
        assert!(
            error.reason.starts_with("operation.aborted|"),
            "{}",
            error.reason
        );
        assert!(central.release_ticket(ticket.clone()).expect("release"));
        assert!(!central.release_ticket(ticket).expect("release twice"));
        let unknown = match central.cancel_ticket("ticket-999".to_owned()).await {
            Err(error) => error,
            Ok(_) => panic!("unknown ticket"),
        };
        assert!(unknown.reason.starts_with("argument.invalid|"));
        central.close().await.expect("close");
    }

    #[tokio::test]
    async fn ticket_cancel_forwards_to_the_blocked_operation() {
        let central = std::sync::Arc::new(connected_synthetic("dispatch-test-forward").await);
        central
            .stage_services("peer-1".to_owned(), hrm_services())
            .await
            .expect("services");
        central.discover(lease(None)).await.expect("discover");
        central
            .block_radio_op("read".to_owned())
            .await
            .expect("block");
        let ticket = central.create_ticket().expect("ticket");
        let reader = {
            let central = std::sync::Arc::clone(&central);
            let ticket = ticket.clone();
            tokio::spawn(async move {
                central
                    .read(ReadOptions {
                        peer_id: "peer-1".to_owned(),
                        selector: selector_input(),
                        timeout_ms: Some(5000),
                        ticket: Some(ticket),
                    })
                    .await
                    .map(|_| ())
                    .map_err(|error| error.reason)
            })
        };
        let mut ack = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let answer = central.cancel_ticket(ticket.clone()).await.expect("cancel");
            if answer.outcome == "forwarded" {
                ack = Some(answer);
                break;
            }
        }
        let ack = ack.expect("the admitted read is cancelled by its own op id");
        assert!(ack.operation_id.is_some());
        let outcome = reader.await.expect("join");
        let reason = outcome.expect_err("cancelled read");
        assert!(reason.starts_with("operation.aborted|"), "{reason}");
        central
            .unblock_radio_op("read".to_owned())
            .await
            .expect("unblock");
        central.close().await.expect("close");
    }

    #[tokio::test]
    async fn rssi_adapter_state_and_events_cross_the_boundary() {
        let central = connected_synthetic("dispatch-test-events").await;
        central
            .stage_rssi("peer-1".to_owned(), -47)
            .await
            .expect("rssi");
        assert_eq!(
            central.read_rssi(lease(Some(2000))).await.expect("rssi"),
            -47
        );
        central
            .stage_adapter_state("powered-on".to_owned(), None)
            .await
            .expect("state");
        assert_eq!(
            central.adapter_state(None).await.expect("adapter"),
            "powered-on"
        );
        central
            .stage_adapter_state("powered-off".to_owned(), Some(true))
            .await
            .expect("announce");
        let mut adapter_event = None;
        for _ in 0..200 {
            adapter_event = central.take_adapter_event().await.expect("take");
            if adapter_event.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let adapter_event = adapter_event.expect("adapter event");
        assert_eq!(adapter_event.kind, "state");
        assert_eq!(adapter_event.state.as_deref(), Some("powered-off"));
        // The power-off is an adapter loss: the core releases the link
        // (`adapter-lost`) and then reports one reset (finding 57).
        let mut lifecycle = None;
        for _ in 0..200 {
            lifecycle = central.take_lifecycle_event().await.expect("take");
            if lifecycle.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let lifecycle = lifecycle.expect("lifecycle event");
        assert_eq!(lifecycle.kind, "adapter-lost");
        assert_eq!(lifecycle.peer_id.as_deref(), Some("peer-1"));
        assert!(lifecycle.connection_generation.is_some());
        let mut reset = None;
        for _ in 0..200 {
            reset = central.take_adapter_reset_event().await.expect("take");
            if reset.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let reset = reset.expect("adapter reset");
        assert_eq!(reset.kind, "reset");
        assert_eq!(reset.cause.as_deref(), Some("powered-off"));
        assert_eq!(
            reset.released_links.as_deref(),
            Some(&["peer-1".to_owned()][..])
        );
        let (previous, current) = (
            reset.previous.expect("previous"),
            reset.current.expect("current"),
        );
        assert_ne!(previous.backend_generation, current.backend_generation);
        let status = central.adapter_status().expect("status");
        assert_eq!(status.power.as_deref(), Some("powered-off"));
        assert!(status.lost);
        let counters = central.dispatch_counters().expect("counters");
        assert_eq!(counters.read_rssi, 1);
        assert_eq!(counters.adapter_state, 1);
        assert!(counters.connect >= 1);
        let report = central.close().await.expect("close");
        assert_eq!(report.state, "released", "{:?}", report.failures.len());
    }

    #[tokio::test]
    async fn subscribe_carries_the_requirement_and_reports_the_observation() {
        let central = connected_synthetic("dispatch-test-delivery").await;
        central
            .stage_services("peer-1".to_owned(), hrm_services())
            .await
            .expect("services");
        central.discover(lease(None)).await.expect("discover");
        central
            .stage_observed_delivery("notification".to_owned())
            .await
            .expect("observed");
        let info = central
            .subscribe(SubscribeOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                consumer: "app".to_owned(),
                delivery_mode: Some("notification".to_owned()),
                overflow_policy: None,
                timeout_ms: Some(5000),
                ticket: None,
            })
            .await
            .expect("subscribe");
        assert_eq!(info.delivery, "notification");
        assert_eq!(
            central.staged_delivery_requests().await.expect("requests"),
            vec![Some("notification".to_owned())]
        );
        let poll = central
            .poll_notification(SubscriptionOptions {
                peer_id: "peer-1".to_owned(),
                selector: selector_input(),
                consumer: "app".to_owned(),
                timeout_ms: None,
                ticket: None,
            })
            .await
            .expect("poll");
        assert_eq!(poll.kind, "empty");
        central
            .stage_link_loss("peer-1".to_owned())
            .await
            .expect("loss");
        let mut invalidated = None;
        for _ in 0..200 {
            let poll = central
                .poll_notification(SubscriptionOptions {
                    peer_id: "peer-1".to_owned(),
                    selector: selector_input(),
                    consumer: "app".to_owned(),
                    timeout_ms: None,
                    ticket: None,
                })
                .await
                .expect("poll");
            if poll.kind != "empty" {
                invalidated = Some(poll);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let invalidated = invalidated.expect("link loss ends the stream");
        assert_eq!(invalidated.kind, "invalidated");
        assert_eq!(invalidated.cause.as_deref(), Some("link-ended"));
        central.close().await.expect("close");
    }

    #[tokio::test]
    async fn security_address_and_write_limits_cross_the_boundary() {
        let central = connected_synthetic("dispatch-test-parity").await;
        central
            .stage_security("peer-1".to_owned(), "not-bonded".to_owned(), Some(true))
            .await
            .expect("stage security");
        let peer = || PeerControlOptions {
            peer_id: "peer-1".to_owned(),
            timeout_ms: Some(2000),
            ticket: None,
        };
        let state = central.security_state(peer()).await.expect("state");
        assert_eq!(state.bond, "not-bonded");
        assert_eq!(state.pairing_possible, Some(true));
        central
            .stage_pair_outcome("peer-1".to_owned(), "paired".to_owned(), None)
            .await
            .expect("script pair");
        let paired = central
            .pair(PairOptions {
                peer_id: "peer-1".to_owned(),
                secure_connections: None,
                timeout_ms: Some(2000),
                ticket: None,
            })
            .await
            .expect("pair");
        assert_eq!(paired.outcome, "paired");
        let mut event = None;
        for _ in 0..200 {
            event = central.take_security_event().await.expect("take");
            if event.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let event = event.expect("security event after pair");
        assert_eq!(event.kind, "state");
        assert_eq!(event.state.expect("state").bond, "bonded");
        let directed = match central
            .pair(PairOptions {
                peer_id: "peer-1".to_owned(),
                secure_connections: Some("require".to_owned()),
                timeout_ms: Some(2000),
                ticket: None,
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("a directed generation without a controller is unsupported"),
        };
        assert!(
            directed.reason.starts_with("capability.unsupported|"),
            "{}",
            directed.reason
        );
        assert_eq!(
            central
                .cancel_pairing(peer())
                .await
                .expect("cancel")
                .outcome,
            "not-pairing"
        );
        let unpaired = central.unpair(peer()).await.expect("unpair");
        assert!(
            unpaired == "unpaired" || unpaired == "already-unpaired",
            "{unpaired}"
        );

        central
            .stage_address(
                "AA:BB:CC:DD:EE:01".to_owned(),
                "public".to_owned(),
                "peer-7".to_owned(),
            )
            .await
            .expect("stage address");
        let resolved = central
            .resolve_address(ResolveAddressOptions {
                address: "aa:bb:cc:dd:ee:01".to_owned(),
                address_type: "public".to_owned(),
                timeout_ms: Some(2000),
                ticket: None,
            })
            .await
            .expect("resolve");
        assert_eq!(resolved, "peer-7");
        let kind = central
            .address_type(PeerControlOptions {
                peer_id: "peer-7".to_owned(),
                timeout_ms: None,
                ticket: None,
            })
            .await
            .expect("address type");
        assert_eq!(kind.as_deref(), Some("public"));
        let bad = match central
            .resolve_address(ResolveAddressOptions {
                address: "not-an-address".to_owned(),
                address_type: "public".to_owned(),
                timeout_ms: None,
                ticket: None,
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("malformed address"),
        };
        assert!(
            bad.reason.starts_with("argument.invalid|"),
            "{}",
            bad.reason
        );

        central
            .stage_services("peer-1".to_owned(), hrm_services())
            .await
            .expect("services");
        central.discover(lease(None)).await.expect("discover");
        central
            .stage_write_limits("peer-1".to_owned(), 244, 182)
            .await
            .expect("limits");
        let limit = |with_response| MaximumWriteLengthOptions {
            peer_id: "peer-1".to_owned(),
            lease: "lease-a".to_owned(),
            selector: selector_input(),
            with_response,
            timeout_ms: None,
            ticket: None,
        };
        assert_eq!(
            central.maximum_write_length(limit(true)).await.expect("wr"),
            244
        );
        assert_eq!(
            central
                .maximum_write_length(limit(false))
                .await
                .expect("wwr"),
            182
        );
        central
            .stage_adapter_authorization("denied".to_owned(), None)
            .await
            .expect("auth");
        assert_eq!(
            central.adapter_authorization(None).await.expect("auth"),
            "denied"
        );
        central.close().await.expect("close");
    }

    #[test]
    fn capability_states_answer_per_desktop_os() {
        for platform in ["bluez", "corebluetooth", "winrt"] {
            let states = UbmCentral::capability_states(platform.to_owned(), None).expect("states");
            assert!(states
                .iter()
                .any(|row| row.id == "connection:direct" && row.state == "limited"));
            assert!(states
                .iter()
                .all(|row| row.state == "limited" || row.state == "unsupported"));
        }
        assert!(UbmCentral::capability_states("android".to_owned(), None).is_err());
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
