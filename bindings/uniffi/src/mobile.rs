//! UniFFI facade of the process-owned mobile owner (`ubm-mobile`): the
//! `Mobile*` types of `ubm_echo.udl`. Swift implements
//! `MobilePlatformRadio` over `OwnedCoreBluetoothProtocolRadio`; every
//! request is answered through `MobileCoreHost.complete`.
//!
//! No path from here reaches the staged/fake radio (guarded by
//! `mobile_surface_has_no_staged_path`).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ubm_desktop::continuation_outbox::{PendingScanFailure, ScanFailureHead, ScanFailureRawToken};
use ubm_desktop::{
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, DesktopError, ObservedDelivery,
    PropertyFlags, ReadProvenance, ServiceSnapshot,
};
use ubm_mobile::{
    AdapterAuthorization, AdapterAvailability, AdapterPower, AdapterSnapshot, Advertisement,
    AuthenticationState, BondState, BondedPeer, CloseFailure, CompanionRecord, CompletionStatus,
    EncryptionState, FailureKind, HostOptions, IngressClass, Instance, ManufacturerData,
    MobileHost, MobilePlatform, MobileSession, OrderedActivation, OrderedEnableStatus, OrderedEnd,
    OrderedLossScope, OrderedLossUnit, OrderedOpenOwnership, OrderedProvenance, OrderedRecord,
    OrderedRegistration, OrderedRejected, OrderedSourceGeneration, PhyObservation, PlatformFailure,
    PlatformRadio, RadioCompletion, RadioIngress, RadioRequest, RestoredPeer,
    SecureConnectionsState, SecurityState, ServiceData, WakeSink, WriteLimits,
};

use crate::build_identity::ubm_build_identity_json;
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

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

enum BindingCleanupFailure {
    NoPending,
    ConstructionUnknown,
    BindingInProgress,
    ShutdownInProgress,
}

fn binding_cleanup_failure(reason: BindingCleanupFailure) -> String {
    let detail = match reason {
        BindingCleanupFailure::NoPending => "no pending installation to retry",
        BindingCleanupFailure::ConstructionUnknown => {
            "construction cleanup has no retirement receipt"
        }
        BindingCleanupFailure::BindingInProgress => "binding in progress",
        BindingCleanupFailure::ShutdownInProgress => "cleanup in progress",
    };
    format!(
        r#"{{"failures":[{{"code":"lifecycle.invalid-state","detail":"{detail}","domain":"cleanup","operation":"mobile.host.shutdown","resourceKind":"host"}}],"state":"release-failed"}}"#
    )
}

/// Mirrors UDL `callback interface MobilePlatformRadio`.
pub trait MobilePlatformRadio: Send + Sync {
    fn ordered_admission_owner(&self, version: u32) -> String;
    fn submit(&self, request: MobileRadioRequest);
    fn cancel(&self, request_id: u64);
}

/// Acceptance of the exact host route, before it is made process-visible.
pub trait MobileHostBinding: Send + Sync {
    fn bind(&self, host: Arc<MobileCoreHost>) -> bool;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileGattSnapshotIdentity {
    pub owner: String,
    pub attachment: u64,
    pub revision: u64,
}
impl From<&ubm_desktop::boundary::GattSnapshotIdentity> for MobileGattSnapshotIdentity {
    fn from(value: &ubm_desktop::boundary::GattSnapshotIdentity) -> Self {
        Self {
            owner: value.owner.clone(),
            attachment: value.attachment,
            revision: value.revision,
        }
    }
}
impl From<MobileGattSnapshotIdentity> for ubm_desktop::boundary::GattSnapshotIdentity {
    fn from(value: MobileGattSnapshotIdentity) -> Self {
        Self {
            owner: value.owner,
            attachment: value.attachment,
            revision: value.revision,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedGeneration {
    Counter { value: u64 },
    Opaque { value: String },
}
impl From<&OrderedSourceGeneration> for MobileOrderedGeneration {
    fn from(value: &OrderedSourceGeneration) -> Self {
        match value {
            OrderedSourceGeneration::Counter(value) => Self::Counter { value: *value },
            OrderedSourceGeneration::Opaque(value) => Self::Opaque {
                value: value.clone(),
            },
        }
    }
}
impl From<MobileOrderedGeneration> for OrderedSourceGeneration {
    fn from(value: MobileOrderedGeneration) -> Self {
        match value {
            MobileOrderedGeneration::Counter { value } => Self::Counter(value),
            MobileOrderedGeneration::Opaque { value } => Self::Opaque(value),
        }
    }
}
fn ordered_generation(value: MobileOrderedGeneration) -> Result<OrderedSourceGeneration, String> {
    if matches!(&value, MobileOrderedGeneration::Opaque { value } if value.is_empty()) {
        return Err("empty ordered source generation".into());
    }
    Ok(value.into())
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedMember {
    pub member_id: u64,
    pub instance: MobileInstance,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedRegistration {
    pub member_id: u64,
    pub source_member: u64,
    pub epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedActivation {
    pub instance: MobileInstance,
    pub connection_generation: String,
    pub source_generation: MobileOrderedGeneration,
    pub group_id: u64,
    pub member: MobileOrderedRegistration,
    pub activation_epoch: u64,
}
impl From<&OrderedActivation> for MobileOrderedActivation {
    fn from(value: &OrderedActivation) -> Self {
        Self {
            instance: (&value.instance).into(),
            connection_generation: value.connection_generation.clone(),
            source_generation: (&value.source_generation).into(),
            group_id: value.group_id,
            member: value.member.into(),
            activation_epoch: value.activation_epoch,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedMarkerAdmission {
    Pending,
    NotAdmitted,
    Admitted { ordinal: u64 },
    Rejected { failure: MobilePlatformFailure },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedMarkerWriteState {
    Initiating,
    NotStarted {
        failure: Option<MobilePlatformFailure>,
    },
    Started {
        failure: Option<MobilePlatformFailure>,
    },
    Settled {
        failure: Option<MobilePlatformFailure>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOrderedMarkerWriteStatus {
    pub admission: MobileOrderedMarkerAdmission,
    pub write: MobileOrderedMarkerWriteState,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedEnableStatus {
    Pending,
    Enabled {
        delivery: String,
    },
    Released {
        failure: Option<MobilePlatformFailure>,
    },
    Uncertain {
        failure: MobilePlatformFailure,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobilePlatformFailure {
    pub kind: String,
    pub gatt_status: Option<i32>,
    pub native_domain: Option<String>,
    pub native_code: Option<i64>,
    pub detail: String,
    pub dispatched: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedOpenOwnership {
    NotCreated,
    Created {
        generation: MobileOrderedGeneration,
        members: Vec<MobileOrderedRegistration>,
    },
    Indeterminate {
        recovery_key: u64,
    },
}
fn mobile_failure(value: MobilePlatformFailure) -> Result<PlatformFailure, String> {
    Ok(PlatformFailure {
        kind: FailureKind::parse(&value.kind)
            .ok_or_else(|| format!("unknown failure kind {}", value.kind))?,
        gatt_status: value.gatt_status,
        native_domain: value.native_domain.filter(|domain| !domain.is_empty()),
        native_code: value.native_code,
        native_name: None,
        detail: value.detail,
        dispatched: value.dispatched,
    })
}
impl From<OrderedRegistration> for MobileOrderedRegistration {
    fn from(value: OrderedRegistration) -> Self {
        Self {
            member_id: value.member_id,
            source_member: value.source_member,
            epoch: value.epoch,
        }
    }
}
impl From<MobileOrderedRegistration> for OrderedRegistration {
    fn from(value: MobileOrderedRegistration) -> Self {
        Self {
            member_id: value.member_id,
            source_member: value.source_member,
            epoch: value.epoch,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedRejected {
    Value {
        member: MobileOrderedRegistration,
        byte_count: u64,
    },
    Loss {
        scope: String,
        member_id: Option<u64>,
        unit: String,
        count: Option<u64>,
        byte_count: Option<u64>,
    },
    Fence {
        member: MobileOrderedRegistration,
    },
    Barrier {
        barrier_id: u64,
    },
    Marker {
        marker_id: u64,
    },
    Unmapped,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOrderedRecord {
    Value {
        member: MobileOrderedRegistration,
        ordinal: u64,
        value: Vec<u8>,
        delivery: String,
        provenance: String,
    },
    Loss {
        scope: String,
        member_id: Option<u64>,
        unit: String,
        count: Option<u64>,
        byte_count: Option<u64>,
    },
    Fence {
        member: MobileOrderedRegistration,
        uncertain_tail: bool,
    },
    Barrier {
        barrier_id: u64,
        uncertain_tail: bool,
    },
    Marker {
        marker_id: u64,
        ordinal: u64,
    },
    Terminal {
        reason: String,
        uncertain_tail: bool,
        native_domain: Option<String>,
        native_code: Option<String>,
        failure_member: Option<MobileOrderedRegistration>,
        rejected: Option<MobileOrderedRejected>,
    },
    SourceFailed {
        uncertain_tail: bool,
        failure: MobilePlatformFailure,
        native_domain: Option<String>,
        native_code: Option<String>,
        failure_member: Option<MobileOrderedRegistration>,
        rejected: Option<MobileOrderedRejected>,
    },
    SubsequentSourceFailure {
        failure: MobilePlatformFailure,
    },
    FailureObservationsOmitted,
    SubsequentSourceFailureV2 {
        failure: MobilePlatformFailure,
        instance: MobileInstance,
        source_generation: String,
        source_member: u64,
        epoch: u64,
        batch_id: u64,
        record_index: u32,
    },
    BridgeFailure {
        code: String,
        detail: String,
    },
}

fn ordered_loss(
    scope: &str,
    member_id: Option<u64>,
    unit: &str,
) -> Result<(OrderedLossScope, OrderedLossUnit), String> {
    let scope = match (scope, member_id) {
        ("member", Some(id)) => OrderedLossScope::Member(id),
        ("group", None) => OrderedLossScope::Group,
        ("unknown", None) => OrderedLossScope::Unknown,
        _ => return Err("invalid ordered loss scope".into()),
    };
    let unit = match unit {
        "values" => OrderedLossUnit::Values,
        "source-records" => OrderedLossUnit::SourceRecords,
        _ => return Err("invalid ordered loss unit".into()),
    };
    Ok((scope, unit))
}
fn ordered_rejected(value: MobileOrderedRejected) -> Result<OrderedRejected, String> {
    Ok(match value {
        MobileOrderedRejected::Value { member, byte_count } => OrderedRejected::Value {
            member: member.into(),
            byte_count,
        },
        MobileOrderedRejected::Fence { member } => OrderedRejected::Fence {
            member: member.into(),
        },
        MobileOrderedRejected::Barrier { barrier_id } => OrderedRejected::Barrier { barrier_id },
        MobileOrderedRejected::Marker { marker_id } => OrderedRejected::Marker { marker_id },
        MobileOrderedRejected::Unmapped => OrderedRejected::Unmapped,
        MobileOrderedRejected::Loss {
            scope,
            member_id,
            unit,
            count,
            byte_count,
        } => {
            let (scope, unit) = ordered_loss(&scope, member_id, &unit)?;
            OrderedRejected::Loss {
                scope,
                unit,
                count,
                byte_count,
            }
        }
    })
}
fn ordered_record(value: MobileOrderedRecord) -> Result<OrderedRecord, String> {
    Ok(match value {
        MobileOrderedRecord::Value {
            member,
            ordinal,
            value,
            delivery,
            provenance,
        } => {
            let delivery = match delivery.as_str() {
                "notification" => ObservedDelivery::Notification,
                "indication" => ObservedDelivery::Indication,
                "unknown" => ObservedDelivery::Unknown,
                _ => return Err("invalid ordered delivery".into()),
            };
            let provenance = match provenance.as_str() {
                "notification" => OrderedProvenance::Notification,
                "read-or-notification" => OrderedProvenance::ReadOrNotification,
                "unknown" => OrderedProvenance::Unknown,
                _ => return Err("invalid ordered provenance".into()),
            };
            OrderedRecord::Value {
                member: member.into(),
                ordinal,
                value,
                delivery,
                provenance,
            }
        }
        MobileOrderedRecord::Loss {
            scope,
            member_id,
            unit,
            count,
            byte_count,
        } => {
            let (scope, unit) = ordered_loss(&scope, member_id, &unit)?;
            OrderedRecord::Loss {
                scope,
                unit,
                count,
                byte_count,
            }
        }
        MobileOrderedRecord::Fence {
            member,
            uncertain_tail,
        } => OrderedRecord::Fence {
            member: member.into(),
            uncertain_tail,
        },
        MobileOrderedRecord::Barrier {
            barrier_id,
            uncertain_tail,
        } => OrderedRecord::Barrier {
            barrier_id,
            uncertain_tail,
        },
        MobileOrderedRecord::Marker { marker_id, ordinal } => {
            OrderedRecord::Marker { marker_id, ordinal }
        }
        MobileOrderedRecord::Terminal {
            reason,
            uncertain_tail,
            native_domain,
            native_code,
            failure_member,
            rejected,
        } => {
            let reason = match reason.as_str() {
                "closed" => OrderedEnd::Closed,
                "connection-lost" => OrderedEnd::ConnectionLost,
                "service-changed" => OrderedEnd::ServiceChanged,
                "source-replaced" => OrderedEnd::SourceReplaced,
                "shutdown" => OrderedEnd::Shutdown,
                "adapter-lost" => OrderedEnd::AdapterLost,
                "overflow" => OrderedEnd::Overflow,
                "ordinal-exhausted" => OrderedEnd::OrdinalExhausted,
                "source-failed" => OrderedEnd::SourceFailed,
                "wake-failed" => OrderedEnd::WakeFailed,
                _ => return Err("invalid ordered terminal".into()),
            };
            if native_domain.is_some() != native_code.is_some() {
                return Err("incomplete ordered native error".into());
            }
            OrderedRecord::Terminal {
                reason,
                uncertain_tail,
                native_domain,
                native_code,
                failure_member: failure_member.map(Into::into),
                rejected: rejected.map(ordered_rejected).transpose()?,
            }
        }
        MobileOrderedRecord::SourceFailed {
            uncertain_tail,
            failure,
            native_domain,
            native_code,
            failure_member,
            rejected,
        } => {
            if native_domain.is_some() != native_code.is_some() {
                return Err("incomplete ordered native error".into());
            }
            OrderedRecord::SourceFailed {
                uncertain_tail,
                failure: mobile_failure(failure)?,
                native_domain,
                native_code,
                failure_member: failure_member.map(Into::into),
                rejected: rejected.map(ordered_rejected).transpose()?,
            }
        }
        MobileOrderedRecord::SubsequentSourceFailure { failure } => {
            OrderedRecord::SubsequentSourceFailure {
                failure: mobile_failure(failure)?,
            }
        }
        MobileOrderedRecord::FailureObservationsOmitted => {
            OrderedRecord::FailureObservationsOmitted
        }
        // The mobile worker does not yet own the versioned identity or its
        // acknowledgement. Never discard it while translating to its older type.
        MobileOrderedRecord::SubsequentSourceFailureV2 { .. } => {
            return Err("versioned ordered failure requires mobile worker support".into());
        }
        MobileOrderedRecord::BridgeFailure { code, detail } => {
            OrderedRecord::BridgeFailure { code, detail }
        }
    })
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
        report_delay_ms: Option<u32>,
        phy: Option<String>,
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
        expected_generation: String,
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
    OrderedEnableNotifications {
        id: u64,
        activation: MobileOrderedActivation,
        requested: Option<String>,
        preferred: Option<String>,
    },
    OrderedDisableNotifications {
        id: u64,
        activation: MobileOrderedActivation,
    },
    OrderedEnableSettlement {
        id: u64,
        activation: MobileOrderedActivation,
    },
    OrderedForgetEnableReceipt {
        id: u64,
        activation: MobileOrderedActivation,
    },
    DisableNotifications {
        id: u64,
        instance: MobileInstance,
        epoch: u64,
    },
    OrderedOpenV2 {
        id: u64,
        identity: MobileOrderedOpenIdentity,
    },
    OrderedRecoverOpenV2 {
        id: u64,
        identity: MobileOrderedOpenIdentity,
    },
    OrderedAcknowledgeOpenV2 {
        id: u64,
        identity: MobileOrderedOpenIdentity,
    },
    OrderedOpen {
        id: u64,
        group_id: u64,
        connection_generation: String,
        database_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
        members: Vec<MobileOrderedMember>,
        item_capacity: u32,
        byte_capacity: u32,
    },
    OrderedRecoverOpen {
        id: u64,
        group_id: u64,
        open_request_id: u64,
        peer_id: String,
    },
    OrderedAcknowledgeOpen {
        id: u64,
        version: u32,
        group_id: u64,
        open_request_id: u64,
        peer_id: String,
    },
    OrderedRegister {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        member: MobileOrderedRegistration,
    },
    OrderedRetire {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        member: MobileOrderedRegistration,
    },
    OrderedDrain {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    OrderedDrainV2 {
        id: u64,
        version: u32,
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    OrderedBarrier {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        barrier_id: u64,
    },
    OrderedGraphAuthorityReserve {
        id: u64,
        request: u64,
    },
    OrderedGraphAuthorityReserveStatus {
        id: u64,
        request: u64,
    },
    OrderedGraphAuthorityReserveFence {
        id: u64,
        request: u64,
    },
    OrderedGraphAuthorityReserveForget {
        id: u64,
        request: u64,
    },
    OrderedGraphAuthorityOpen {
        id: u64,
        receipt_id: u64,
    },
    OrderedGraphAuthorityStatus {
        id: u64,
        receipt_id: u64,
    },
    OrderedGraphAuthorityFenceOpen {
        id: u64,
        receipt_id: u64,
    },
    OrderedGraphAuthorityForgetOpen {
        id: u64,
        receipt_id: u64,
    },
    OrderedGraphAuthorityClose {
        id: u64,
        authority_id: u64,
    },
    OrderedGraphAuthorityForget {
        id: u64,
        authority_id: u64,
    },
    OrderedGraphPermitReserve {
        id: u64,
        authority_id: u64,
        request: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
    },
    OrderedGraphPermitReserveStatus {
        id: u64,
        authority_id: u64,
        request: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
    },
    OrderedGraphPermitReserveFence {
        id: u64,
        authority_id: u64,
        request: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
    },
    OrderedGraphPermitForgetRequest {
        id: u64,
        authority_id: u64,
        request: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
    },
    OrderedGraphPermitAcquire {
        id: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
        authority_id: u64,
        permit_id: u64,
    },
    OrderedGraphPermitRelease {
        id: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
        authority_id: u64,
        permit_id: u64,
    },
    OrderedGraphPermitCommit {
        id: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
        authority_id: u64,
        permit_id: u64,
    },
    OrderedGraphPermitForget {
        id: u64,
        peer_id: String,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
        authority_id: u64,
        permit_id: u64,
    },
    OrderedMarkerWrite {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        marker_id: u64,
        connection_generation: String,
        snapshot_identity: MobileGattSnapshotIdentity,
        instance: MobileInstance,
        value: Vec<u8>,
        with_response: bool,
    },
    OrderedMarkerWriteSettlement {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        marker_id: u64,
    },
    OrderedCancelMarkerWrite {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        marker_id: u64,
    },
    OrderedForgetMarkerWriteReceipt {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        marker_id: u64,
    },
    OrderedAcknowledgeBatch {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        batch_id: u64,
    },
    OrderedAcknowledgeBatchV2 {
        id: u64,
        version: u32,
        generation: MobileOrderedGeneration,
        group_id: u64,
        batch_id: u64,
    },
    OrderedClose {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    OrderedSeal {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    OrderedRetainClaim {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        member: MobileOrderedRegistration,
    },
    OrderedConfirmCleanup {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
        member: MobileOrderedRegistration,
    },
    OrderedRemove {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    OrderedForgetRemovalReceipt {
        id: u64,
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    ReadMtu {
        id: u64,
        peer_id: String,
    },
    ReadWriteLimits {
        id: u64,
        peer_id: String,
    },
    ReadWriteReadiness {
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
    RequestSubrate {
        id: u64,
        peer_id: String,
        mode: String,
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
    ConnectedPeers {
        id: u64,
        services: Vec<String>,
    },
    ResolvePeer {
        id: u64,
        peer_id: String,
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
        filters_json: Option<String>,
    },
    ListCompanion {
        id: u64,
    },
    DisassociateCompanion {
        id: u64,
        association_id: i64,
    },
    ObservePresence {
        id: u64,
        peer_id: String,
    },
    StopPresence {
        id: u64,
        peer_id: String,
    },
    Close {
        id: u64,
    },
    OwnerFence {
        id: u64,
        owner_incarnation: String,
        retirement_id: String,
    },
    OwnerStatus {
        id: u64,
        owner_incarnation: String,
        retirement_id: String,
    },
}

fn mode(mode: Option<DeliveryMode>) -> Option<String> {
    mode.map(|mode| mode.as_str().to_owned())
}

impl From<&RadioRequest> for MobileRadioRequest {
    fn from(request: &RadioRequest) -> Self {
        match request {
            RadioRequest::OwnerFence {
                id,
                owner_incarnation,
                retirement_id,
            } => Self::OwnerFence {
                id: *id,
                owner_incarnation: owner_incarnation.to_string(),
                retirement_id: retirement_id.to_string(),
            },
            RadioRequest::OwnerStatus {
                id,
                owner_incarnation,
                retirement_id,
            } => Self::OwnerStatus {
                id: *id,
                owner_incarnation: owner_incarnation.to_string(),
                retirement_id: retirement_id.to_string(),
            },
            RadioRequest::OrderedOpenV2(request) => Self::from(request),
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
                    report_delay_ms: android.report_delay_ms,
                    phy: android.phy.map(|phy| phy.as_str().to_owned()),
                }
            }
            RadioRequest::StopScan { id } => Self::StopScan { id: *id },
            RadioRequest::Connect {
                id,
                peer_id,
                auto_connect,
                preferred_phy,
                expected_generation,
            } => Self::Connect {
                id: *id,
                peer_id: peer_id.clone(),
                auto_connect: *auto_connect,
                preferred_phy: preferred_phy
                    .iter()
                    .map(|phy| phy.as_str().to_owned())
                    .collect(),
                expected_generation: expected_generation.clone(),
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
            RadioRequest::OrderedEnableNotifications {
                id,
                activation,
                requested,
                preferred,
            } => Self::OrderedEnableNotifications {
                id: *id,
                activation: activation.into(),
                requested: mode(*requested),
                preferred: mode(*preferred),
            },
            RadioRequest::OrderedDisableNotifications { id, activation } => {
                Self::OrderedDisableNotifications {
                    id: *id,
                    activation: activation.into(),
                }
            }
            RadioRequest::OrderedEnableSettlement { id, activation } => {
                Self::OrderedEnableSettlement {
                    id: *id,
                    activation: activation.into(),
                }
            }
            RadioRequest::OrderedForgetEnableReceipt { id, activation } => {
                Self::OrderedForgetEnableReceipt {
                    id: *id,
                    activation: activation.into(),
                }
            }
            RadioRequest::DisableNotifications {
                id,
                instance,
                epoch,
            } => Self::DisableNotifications {
                id: *id,
                instance: instance.into(),
                epoch: *epoch,
            },
            RadioRequest::OrderedOpen {
                id,
                group_id,
                connection_generation,
                database_generation,
                snapshot_identity,
                members,
                item_capacity,
                byte_capacity,
            } => Self::OrderedOpen {
                id: *id,
                group_id: *group_id,
                connection_generation: connection_generation.clone(),
                database_generation: database_generation.clone(),
                snapshot_identity: snapshot_identity.into(),
                members: members
                    .iter()
                    .map(|m| MobileOrderedMember {
                        member_id: m.member_id,
                        instance: (&m.instance).into(),
                    })
                    .collect(),
                item_capacity: *item_capacity,
                byte_capacity: *byte_capacity,
            },
            RadioRequest::OrderedRecoverOpen {
                id,
                group_id,
                open_request_id,
                peer_id,
            } => Self::OrderedRecoverOpen {
                id: *id,
                group_id: *group_id,
                open_request_id: *open_request_id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::OrderedAcknowledgeOpen {
                id,
                version,
                group_id,
                open_request_id,
                peer_id,
            } => Self::OrderedAcknowledgeOpen {
                id: *id,
                version: *version,
                group_id: *group_id,
                open_request_id: *open_request_id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::OrderedRegister {
                id,
                generation,
                group_id,
                member,
            } => Self::OrderedRegister {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                member: (*member).into(),
            },
            RadioRequest::OrderedRetire {
                id,
                generation,
                group_id,
                member,
            } => Self::OrderedRetire {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                member: (*member).into(),
            },
            RadioRequest::OrderedDrain {
                id,
                generation,
                group_id,
            } => Self::OrderedDrain {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
            },
            RadioRequest::OrderedBarrier {
                id,
                generation,
                group_id,
                barrier_id,
            } => Self::OrderedBarrier {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                barrier_id: *barrier_id,
            },
            RadioRequest::OrderedGraphAuthorityReserveFence { id, request } => {
                Self::OrderedGraphAuthorityReserveFence {
                    id: *id,
                    request: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityReserveForget { id, request } => {
                Self::OrderedGraphAuthorityReserveForget {
                    id: *id,
                    request: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityReserveStatus { id, request } => {
                Self::OrderedGraphAuthorityReserveStatus {
                    id: *id,
                    request: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityReserve { id, request } => {
                Self::OrderedGraphAuthorityReserve {
                    id: *id,
                    request: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityForgetOpen { id, request } => {
                Self::OrderedGraphAuthorityForgetOpen {
                    id: *id,
                    receipt_id: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityFenceOpen { id, request } => {
                Self::OrderedGraphAuthorityFenceOpen {
                    id: *id,
                    receipt_id: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityStatus { id, request } => {
                Self::OrderedGraphAuthorityStatus {
                    id: *id,
                    receipt_id: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityOpen { id, request } => {
                Self::OrderedGraphAuthorityOpen {
                    id: *id,
                    receipt_id: *request,
                }
            }
            RadioRequest::OrderedGraphAuthorityClose { id, authority_id } => {
                Self::OrderedGraphAuthorityClose {
                    id: *id,
                    authority_id: *authority_id,
                }
            }
            RadioRequest::OrderedGraphAuthorityForget { id, authority_id } => {
                Self::OrderedGraphAuthorityForget {
                    id: *id,
                    authority_id: *authority_id,
                }
            }
            RadioRequest::OrderedGraphPermitReserve { id, key } => {
                Self::OrderedGraphPermitReserve {
                    id: *id,
                    authority_id: key.authority_id,
                    request: key.request_id,
                    peer_id: key.peer_id.clone(),
                    connection_generation: key.connection_generation.clone(),
                    snapshot_identity: (&key.identity).into(),
                }
            }
            RadioRequest::OrderedGraphPermitReserveStatus { id, key } => {
                Self::OrderedGraphPermitReserveStatus {
                    id: *id,
                    authority_id: key.authority_id,
                    request: key.request_id,
                    peer_id: key.peer_id.clone(),
                    connection_generation: key.connection_generation.clone(),
                    snapshot_identity: (&key.identity).into(),
                }
            }
            RadioRequest::OrderedGraphPermitReserveFence { id, key } => {
                Self::OrderedGraphPermitReserveFence {
                    id: *id,
                    authority_id: key.authority_id,
                    request: key.request_id,
                    peer_id: key.peer_id.clone(),
                    connection_generation: key.connection_generation.clone(),
                    snapshot_identity: (&key.identity).into(),
                }
            }
            RadioRequest::OrderedGraphPermitForgetRequest { id, key } => {
                Self::OrderedGraphPermitForgetRequest {
                    id: *id,
                    authority_id: key.authority_id,
                    request: key.request_id,
                    peer_id: key.peer_id.clone(),
                    connection_generation: key.connection_generation.clone(),
                    snapshot_identity: (&key.identity).into(),
                }
            }
            RadioRequest::OrderedGraphPermitAcquire { id, key } => {
                Self::OrderedGraphPermitAcquire {
                    id: *id,
                    peer_id: key.peer_id.clone(),
                    connection_generation: key.connection_generation.clone(),
                    snapshot_identity: (&key.identity).into(),
                    authority_id: key.authority_id,
                    permit_id: key.permit_id,
                }
            }
            RadioRequest::OrderedGraphPermitRelease { id, key } => {
                Self::OrderedGraphPermitRelease {
                    id: *id,
                    peer_id: key.peer_id.clone(),
                    connection_generation: key.connection_generation.clone(),
                    snapshot_identity: (&key.identity).into(),
                    authority_id: key.authority_id,
                    permit_id: key.permit_id,
                }
            }
            RadioRequest::OrderedGraphPermitCommit { id, key } => Self::OrderedGraphPermitCommit {
                id: *id,
                peer_id: key.peer_id.clone(),
                connection_generation: key.connection_generation.clone(),
                snapshot_identity: (&key.identity).into(),
                authority_id: key.authority_id,
                permit_id: key.permit_id,
            },
            RadioRequest::OrderedGraphPermitForget { id, key } => Self::OrderedGraphPermitForget {
                id: *id,
                peer_id: key.peer_id.clone(),
                connection_generation: key.connection_generation.clone(),
                snapshot_identity: (&key.identity).into(),
                authority_id: key.authority_id,
                permit_id: key.permit_id,
            },
            RadioRequest::OrderedMarkerWrite {
                id,
                generation,
                group_id,
                marker_id,
                connection_generation,
                snapshot_identity,
                instance,
                value,
                with_response,
            } => Self::OrderedMarkerWrite {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                marker_id: *marker_id,
                connection_generation: connection_generation.clone(),
                snapshot_identity: snapshot_identity.into(),
                instance: instance.into(),
                value: value.clone(),
                with_response: *with_response,
            },
            RadioRequest::OrderedMarkerWriteSettlement {
                id,
                generation,
                group_id,
                marker_id,
            } => Self::OrderedMarkerWriteSettlement {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                marker_id: *marker_id,
            },
            RadioRequest::OrderedCancelMarkerWrite {
                id,
                generation,
                group_id,
                marker_id,
            } => Self::OrderedCancelMarkerWrite {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                marker_id: *marker_id,
            },
            RadioRequest::OrderedForgetMarkerWriteReceipt {
                id,
                generation,
                group_id,
                marker_id,
            } => Self::OrderedForgetMarkerWriteReceipt {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                marker_id: *marker_id,
            },
            RadioRequest::OrderedAcknowledgeBatch {
                id,
                generation,
                group_id,
                batch_id,
            } => Self::OrderedAcknowledgeBatch {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                batch_id: *batch_id,
            },
            RadioRequest::OrderedClose {
                id,
                generation,
                group_id,
            } => Self::OrderedClose {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
            },
            RadioRequest::OrderedSeal {
                id,
                generation,
                group_id,
            } => Self::OrderedSeal {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
            },
            RadioRequest::OrderedRetainClaim {
                id,
                generation,
                group_id,
                member,
            } => Self::OrderedRetainClaim {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                member: (*member).into(),
            },
            RadioRequest::OrderedConfirmCleanup {
                id,
                generation,
                group_id,
                member,
            } => Self::OrderedConfirmCleanup {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
                member: (*member).into(),
            },
            RadioRequest::OrderedRemove {
                id,
                generation,
                group_id,
            } => Self::OrderedRemove {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
            },
            RadioRequest::OrderedForgetRemovalReceipt {
                id,
                generation,
                group_id,
            } => Self::OrderedForgetRemovalReceipt {
                id: *id,
                generation: generation.into(),
                group_id: *group_id,
            },
            RadioRequest::ReadMtu { id, peer_id } => Self::ReadMtu {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::ReadWriteLimits { id, peer_id } => Self::ReadWriteLimits {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::ReadWriteReadiness { id, peer_id } => Self::ReadWriteReadiness {
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
            RadioRequest::RequestSubrate { id, peer_id, mode } => Self::RequestSubrate {
                id: *id,
                peer_id: peer_id.clone(),
                mode: mode.as_str().to_owned(),
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
            RadioRequest::ResolvePeer { id, peer_id } => Self::ResolvePeer {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::ConnectedPeers { id, services } => Self::ConnectedPeers {
                id: *id,
                services: services.clone(),
            },
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
                filters_json,
            } => Self::AssociateCompanion {
                id: *id,
                name: name.clone(),
                service_uuid: service_uuid.clone(),
                filters_json: filters_json.clone(),
            },
            RadioRequest::ListCompanion { id } => Self::ListCompanion { id: *id },
            RadioRequest::DisassociateCompanion { id, association_id } => {
                Self::DisassociateCompanion {
                    id: *id,
                    association_id: *association_id,
                }
            }
            RadioRequest::ObservePresence { id, peer_id } => Self::ObservePresence {
                id: *id,
                peer_id: peer_id.clone(),
            },
            RadioRequest::StopPresence { id, peer_id } => Self::StopPresence {
                id: *id,
                peer_id: peer_id.clone(),
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
    pub primary: Option<bool>,
    pub included_services: Option<Vec<MobileGattDescriptor>>,
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

/// Mirrors UDL `dictionary MobileCompanionRecord`: one Companion Device Manager
/// association this app holds (finding 236).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileCompanionRecord {
    pub association_id: i64,
    pub peer_id: Option<String>,
    pub display_name: Option<String>,
}

/// Mirrors UDL `dictionary MobileCloseFailure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileCloseFailure {
    pub instance: MobileInstance,
    pub detail: String,
}

/// Mirrors UDL `[Enum] interface MobileRadioCompletion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileGraphReserveStatus {
    Pending,
    NotCreated,
    Issued { receipt_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileGraphAuthorityStatus {
    Pending,
    NotCreated,
    Opened { authority_id: u64 },
    Closed { authority_id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileGraphPermitState {
    Acquired,
    Stale,
    Busy,
    Committed,
    Released,
    Forgotten,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOwnerOutstandingClass {
    Attachments,
    Reservations,
    NativeWork,
    Callbacks,
    Cleanup,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOwnerRejection {
    WrongOwner,
    ConflictingRetirement,
    Malformed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileOwnerRetirementState {
    FencedPending {
        outstanding_classes: Vec<MobileOwnerOutstandingClass>,
        failures: Vec<MobileOrderedFailureV2>,
    },
    Quiescent,
    Rejected {
        reason: MobileOwnerRejection,
    },
}

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
    DiscoveredScoped {
        services: Vec<MobileGattService>,
        identity: MobileGattSnapshotIdentity,
    },
    NotifyEnabled {
        delivery: String,
    },
    OrderedEnableReleased {
        kind: String,
        gatt_status: Option<i32>,
        native_domain: Option<String>,
        native_code: Option<i64>,
        detail: String,
        dispatched: bool,
    },
    OrderedOpenResultV2 {
        slot_identity: MobileOrderedSlotIdentity,
        outcome: MobileOrderedOpenOutcome,
    },
    OrderedOpenRetiredV2 {
        slot_identity: MobileOrderedSlotIdentity,
    },
    OrderedOpened {
        generation: MobileOrderedGeneration,
        members: Vec<MobileOrderedRegistration>,
    },
    OrderedOpenFailed {
        failure: MobilePlatformFailure,
        ownership: MobileOrderedOpenOwnership,
    },
    OrderedRegistered {
        member: MobileOrderedRegistration,
    },
    OrderedClaim {
        generation: MobileOrderedGeneration,
        member: MobileOrderedRegistration,
    },
    OrderedEnableStatus {
        status: MobileOrderedEnableStatus,
    },
    OrderedGraphPermitReserved {
        permit_id: u64,
    },
    OrderedGraphAuthorityReserved {
        receipt_id: u64,
    },
    OrderedGraphReserveStatus {
        status: MobileGraphReserveStatus,
    },
    OrderedGraphAuthorityOpened {
        authority_id: u64,
    },
    OrderedGraphAuthorityStatus {
        status: MobileGraphAuthorityStatus,
    },
    OrderedGraphPermit {
        state: MobileGraphPermitState,
    },
    OrderedGraphPermitInvalidated {
        failure: MobilePlatformFailure,
    },
    OrderedGraphPermitNotAcquired {
        failure: MobilePlatformFailure,
    },
    OrderedMarkerWriteStatus {
        status: MobileOrderedMarkerWriteStatus,
    },
    OrderedRecords {
        batch_id: Option<u64>,
        records: Vec<MobileOrderedRecord>,
    },
    Mtu {
        mtu: Option<u16>,
    },
    WriteLimits {
        with_response: u16,
        without_response: u16,
    },
    Ready {
        ready: bool,
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
    ConnectedPeers {
        peers: Vec<MobilePeerName>,
    },
    ResolvedPeer {
        peer: Option<MobilePeerName>,
    },
    Lease {
        lease_id: String,
    },
    Companion {
        association_id: i64,
        peer_id: Option<String>,
        display_name: Option<String>,
        /// The platform already held this association (finding 236): the record
        /// is the existing one and nothing new was created.
        already_associated: bool,
    },
    CompanionList {
        records: Vec<MobileCompanionRecord>,
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
    OwnerRetirement {
        owner_incarnation: String,
        retirement_id: String,
        state: MobileOwnerRetirementState,
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

fn discovered_services(services: Vec<MobileGattService>) -> Vec<ServiceSnapshot> {
    services
        .into_iter()
        .map(|service| ServiceSnapshot {
            primary: service.primary,
            included_services: service.included_services.map(|references| {
                references
                    .into_iter()
                    .map(
                        |reference| ubm_desktop::boundary::IncludedServiceReference {
                            uuid: reference.uuid,
                            occurrence: reference.occurrence,
                        },
                    )
                    .collect()
            }),
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
            access: std::default::Default::default(),
        })
        .collect()
}

/// Typed completion from the Swift value; an unreadable value is an error
/// the host turns into a platform failure for that request.
pub fn completion(value: MobileRadioCompletion) -> Result<RadioCompletion, String> {
    Ok(match value {
        MobileRadioCompletion::OwnerRetirement {
            owner_incarnation,
            retirement_id,
            state,
        } => {
            use ubm_mobile::{
                OrderedAdmissionOwner, OwnerOutstandingClass as C, OwnerRetirementRejection as R,
                OwnerRetirementResult, OwnerRetirementState as S,
            };
            let owner_incarnation = OrderedAdmissionOwner::negotiate_slot(2, &owner_incarnation)
                .map_err(str::to_owned)?;
            let retirement_id =
                OrderedAdmissionOwner::negotiate_slot(2, &retirement_id).map_err(str::to_owned)?;
            let state = match state {
                MobileOwnerRetirementState::Quiescent => S::Quiescent,
                MobileOwnerRetirementState::Rejected { reason } => S::Rejected {
                    reason: match reason {
                        MobileOwnerRejection::WrongOwner => R::WrongOwner,
                        MobileOwnerRejection::ConflictingRetirement => R::ConflictingRetirement,
                        MobileOwnerRejection::Malformed => R::Malformed,
                    },
                },
                MobileOwnerRetirementState::FencedPending {
                    outstanding_classes,
                    failures,
                } => S::FencedPending {
                    outstanding_classes: outstanding_classes
                        .into_iter()
                        .map(|v| match v {
                            MobileOwnerOutstandingClass::Attachments => C::Attachments,
                            MobileOwnerOutstandingClass::Reservations => C::Reservations,
                            MobileOwnerOutstandingClass::NativeWork => C::NativeWork,
                            MobileOwnerOutstandingClass::Callbacks => C::Callbacks,
                            MobileOwnerOutstandingClass::Cleanup => C::Cleanup,
                        })
                        .collect(),
                    failures: failures
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                },
            };
            RadioCompletion::OwnerRetirement(OwnerRetirementResult {
                owner_incarnation,
                retirement_id,
                state,
            })
        }
        MobileRadioCompletion::Unit => RadioCompletion::Unit,
        MobileRadioCompletion::Bytes { value } => RadioCompletion::Bytes(value),
        MobileRadioCompletion::Read { value, provenance } => RadioCompletion::Read {
            value,
            provenance: ReadProvenance::from_wire(&provenance)
                .ok_or_else(|| format!("unknown read provenance {provenance}"))?,
        },
        MobileRadioCompletion::Adapter { snapshot } => RadioCompletion::Adapter(adapter(snapshot)?),
        MobileRadioCompletion::Discovered { services } => {
            RadioCompletion::Discovered(discovered_services(services))
        }
        MobileRadioCompletion::DiscoveredScoped { services, identity } => {
            if identity.owner.is_empty() {
                return Err("empty native graph owner".into());
            }
            RadioCompletion::DiscoveredScoped {
                services: discovered_services(services),
                identity: identity.into(),
            }
        }
        MobileRadioCompletion::NotifyEnabled { delivery } => {
            RadioCompletion::NotifyEnabled(match delivery.as_str() {
                "notification" => ObservedDelivery::Notification,
                "indication" => ObservedDelivery::Indication,
                "unknown" => ObservedDelivery::Unknown,
                _ => return Err(format!("unknown delivery {delivery}")),
            })
        }
        MobileRadioCompletion::OrderedEnableReleased {
            kind,
            gatt_status,
            native_domain,
            native_code,
            detail,
            dispatched,
        } => RadioCompletion::OrderedEnableReleased(PlatformFailure {
            kind: FailureKind::parse(&kind)
                .ok_or_else(|| format!("unknown failure kind {kind}"))?,
            gatt_status,
            native_domain: native_domain.filter(|domain| !domain.is_empty()),
            native_code,
            native_name: None,
            detail,
            dispatched,
        }),
        value @ (MobileRadioCompletion::OrderedOpenResultV2 { .. }
        | MobileRadioCompletion::OrderedOpenRetiredV2 { .. }) => {
            RadioCompletion::OrderedOpenV2(value.try_into()?)
        }
        MobileRadioCompletion::OrderedOpened {
            generation,
            members,
        } => RadioCompletion::OrderedOpened {
            generation: ordered_generation(generation)?,
            members: members.into_iter().map(Into::into).collect(),
        },
        MobileRadioCompletion::OrderedOpenFailed { failure, ownership } => {
            let ownership = match ownership {
                MobileOrderedOpenOwnership::NotCreated => OrderedOpenOwnership::NotCreated,
                MobileOrderedOpenOwnership::Created {
                    generation,
                    members,
                } => OrderedOpenOwnership::Created {
                    generation: ordered_generation(generation)?,
                    members: members.into_iter().map(Into::into).collect(),
                },
                MobileOrderedOpenOwnership::Indeterminate { recovery_key } => {
                    OrderedOpenOwnership::Indeterminate { recovery_key }
                }
            };
            RadioCompletion::OrderedOpenFailed {
                failure: mobile_failure(failure)?,
                ownership,
            }
        }
        MobileRadioCompletion::OrderedGraphPermitReserved { permit_id } => {
            RadioCompletion::OrderedGraphPermitReserved(permit_id)
        }
        MobileRadioCompletion::OrderedGraphReserveStatus { status } => {
            RadioCompletion::OrderedGraphReserveStatus(match status {
                MobileGraphReserveStatus::Pending => ubm_mobile::OrderedGraphReserveStatus::Pending,
                MobileGraphReserveStatus::NotCreated => {
                    ubm_mobile::OrderedGraphReserveStatus::NotCreated
                }
                MobileGraphReserveStatus::Issued { receipt_id } => {
                    ubm_mobile::OrderedGraphReserveStatus::Issued(receipt_id)
                }
            })
        }
        MobileRadioCompletion::OrderedGraphAuthorityReserved { receipt_id } => {
            RadioCompletion::OrderedGraphAuthorityReserved(receipt_id)
        }
        MobileRadioCompletion::OrderedGraphAuthorityStatus { status } => {
            RadioCompletion::OrderedGraphAuthorityStatus(match status {
                MobileGraphAuthorityStatus::Pending => {
                    ubm_mobile::OrderedGraphAuthorityStatus::Pending
                }
                MobileGraphAuthorityStatus::NotCreated => {
                    ubm_mobile::OrderedGraphAuthorityStatus::NotCreated
                }
                MobileGraphAuthorityStatus::Opened { authority_id } => {
                    ubm_mobile::OrderedGraphAuthorityStatus::Opened(authority_id)
                }
                MobileGraphAuthorityStatus::Closed { authority_id } => {
                    ubm_mobile::OrderedGraphAuthorityStatus::Closed(authority_id)
                }
            })
        }
        MobileRadioCompletion::OrderedGraphAuthorityOpened { authority_id } => {
            RadioCompletion::OrderedGraphAuthorityOpened(authority_id)
        }
        MobileRadioCompletion::OrderedGraphPermit { state } => {
            RadioCompletion::OrderedGraphPermit(match state {
                MobileGraphPermitState::Acquired => ubm_mobile::OrderedGraphPermitState::Acquired,
                MobileGraphPermitState::Stale => ubm_mobile::OrderedGraphPermitState::Stale,
                MobileGraphPermitState::Busy => ubm_mobile::OrderedGraphPermitState::Busy,
                MobileGraphPermitState::Released => ubm_mobile::OrderedGraphPermitState::Released,
                MobileGraphPermitState::Committed => ubm_mobile::OrderedGraphPermitState::Committed,
                MobileGraphPermitState::Forgotten => ubm_mobile::OrderedGraphPermitState::Forgotten,
            })
        }
        MobileRadioCompletion::OrderedGraphPermitNotAcquired { failure } => {
            RadioCompletion::OrderedGraphPermitNotAcquired(mobile_failure(failure)?)
        }
        MobileRadioCompletion::OrderedGraphPermitInvalidated { failure } => {
            RadioCompletion::OrderedGraphPermitInvalidated(mobile_failure(failure)?)
        }
        MobileRadioCompletion::OrderedMarkerWriteStatus { status } => {
            let admission = match status.admission {
                MobileOrderedMarkerAdmission::Pending => {
                    ubm_mobile::OrderedMarkerAdmission::Pending
                }
                MobileOrderedMarkerAdmission::NotAdmitted => {
                    ubm_mobile::OrderedMarkerAdmission::NotAdmitted
                }
                MobileOrderedMarkerAdmission::Admitted { ordinal } => {
                    ubm_mobile::OrderedMarkerAdmission::Admitted { ordinal }
                }
                MobileOrderedMarkerAdmission::Rejected { failure } => {
                    ubm_mobile::OrderedMarkerAdmission::Rejected {
                        failure: mobile_failure(failure)?,
                    }
                }
            };
            let write = match status.write {
                MobileOrderedMarkerWriteState::Initiating => {
                    ubm_mobile::OrderedMarkerWriteState::Initiating
                }
                MobileOrderedMarkerWriteState::NotStarted { failure } => {
                    ubm_mobile::OrderedMarkerWriteState::NotStarted {
                        failure: failure.map(mobile_failure).transpose()?,
                    }
                }
                MobileOrderedMarkerWriteState::Started { failure } => {
                    ubm_mobile::OrderedMarkerWriteState::Started {
                        failure: failure.map(mobile_failure).transpose()?,
                    }
                }
                MobileOrderedMarkerWriteState::Settled { failure } => {
                    ubm_mobile::OrderedMarkerWriteState::Settled {
                        failure: failure.map(mobile_failure).transpose()?,
                    }
                }
            };
            RadioCompletion::OrderedMarkerWriteStatus(ubm_mobile::OrderedMarkerWriteStatus {
                admission,
                write,
            })
        }
        MobileRadioCompletion::OrderedEnableStatus { status } => {
            let status = match status {
                MobileOrderedEnableStatus::Pending => OrderedEnableStatus::Pending,
                MobileOrderedEnableStatus::Enabled { delivery } => {
                    OrderedEnableStatus::Enabled(match delivery.as_str() {
                        "notification" => ObservedDelivery::Notification,
                        "indication" => ObservedDelivery::Indication,
                        "unknown" => ObservedDelivery::Unknown,
                        _ => return Err("invalid ordered delivery".into()),
                    })
                }
                MobileOrderedEnableStatus::Released { failure } => {
                    OrderedEnableStatus::Released(failure.map(mobile_failure).transpose()?)
                }
                MobileOrderedEnableStatus::Uncertain { failure } => {
                    OrderedEnableStatus::Uncertain(mobile_failure(failure)?)
                }
            };
            RadioCompletion::OrderedEnableStatus(status)
        }
        MobileRadioCompletion::OrderedRegistered { member } => {
            RadioCompletion::OrderedRegistered(member.into())
        }
        MobileRadioCompletion::OrderedClaim { generation, member } => {
            RadioCompletion::OrderedClaim {
                generation: ordered_generation(generation)?,
                member: member.into(),
            }
        }
        MobileRadioCompletion::OrderedRecords { batch_id, records } => {
            if batch_id.is_some() == records.is_empty() {
                return Err("invalid ordered batch".into());
            }
            RadioCompletion::OrderedRecords {
                batch_id,
                records: records
                    .into_iter()
                    .map(ordered_record)
                    .collect::<Result<_, _>>()?,
            }
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
        MobileRadioCompletion::Ready { ready } => RadioCompletion::Ready(ready),
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
        MobileRadioCompletion::ResolvedPeer { peer } => {
            RadioCompletion::ResolvedPeer(peer.map(|peer| ubm_mobile::ResolvedPeer {
                peer_id: peer.peer_id,
                name: peer.name,
            }))
        }
        MobileRadioCompletion::ConnectedPeers { peers } => RadioCompletion::ConnectedPeers(
            peers
                .into_iter()
                .map(|peer| ubm_mobile::ConnectedPeer {
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
            already_associated,
        } => RadioCompletion::Companion {
            association_id,
            peer_id,
            display_name,
            already_associated,
        },
        MobileRadioCompletion::CompanionList { records } => RadioCompletion::CompanionList(
            records
                .into_iter()
                .map(|record| CompanionRecord {
                    association_id: record.association_id,
                    peer_id: record.peer_id,
                    display_name: record.display_name,
                })
                .collect(),
        ),
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
    OrderedWake {
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    OrderedWakeFailure {
        generation: MobileOrderedGeneration,
        group_id: u64,
    },
    Advertisement {
        advertisement: MobileAdvertisement,
    },
    Connection {
        peer_id: String,
        connected: bool,
        status: Option<i32>,
    },
    ConnectionScoped {
        peer_id: String,
        expected_generation: String,
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
    NotificationSourceFailed {
        version: u32,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
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
    WriteReadiness {
        peer_id: String,
        ready: bool,
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
        MobileRadioIngress::OrderedWake {
            generation,
            group_id,
        } => RadioIngress::OrderedWake {
            generation: ordered_generation(generation)?,
            group_id,
        },
        MobileRadioIngress::OrderedWakeFailure {
            generation,
            group_id,
        } => RadioIngress::OrderedWakeFailure {
            generation: ordered_generation(generation)?,
            group_id,
        },
        MobileRadioIngress::Advertisement { advertisement } => {
            RadioIngress::Advertisement(Advertisement {
                capture_timestamp_ms: None,
                cached_name: None,
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
        MobileRadioIngress::ConnectionScoped {
            peer_id,
            expected_generation,
            connected,
            status,
        } => RadioIngress::ConnectionScoped {
            peer_id,
            expected_generation,
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
        MobileRadioIngress::NotificationSourceFailed {
            version,
            instance,
            epoch,
            failure,
        } => {
            if version != 1 {
                return Err(format!(
                    "unsupported notification failure ingress version {version}"
                ));
            }
            RadioIngress::NotificationSourceFailed {
                instance: instance.into(),
                epoch,
                failure: mobile_failure(failure)?,
            }
        }
        MobileRadioIngress::AdapterState { snapshot } => {
            RadioIngress::AdapterState(adapter(snapshot)?)
        }
        MobileRadioIngress::ScanFailed { detail } => RadioIngress::ScanFailed { detail },
        MobileRadioIngress::SecurityChanged { peer_id, state } => RadioIngress::SecurityChanged {
            peer_id,
            state: security(state)?,
        },
        MobileRadioIngress::WriteReadiness { peer_id, ready } => {
            RadioIngress::WriteReadiness { peer_id, ready }
        }
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
    admission: ubm_mobile::OrderedAdmissionNegotiation,
    retirement_version: u32,
    retirement_owner: Option<ubm_mobile::OrderedAdmissionOwner>,
    owner: OnceLock<std::sync::Weak<MobileCoreHost>>,
}

impl PlatformRadio for ForeignPlatformRadio {
    fn ordered_owner_retirement_version(&self) -> u32 {
        self.retirement_version
    }
    fn ordered_admission_owner(&self) -> Option<ubm_mobile::OrderedAdmissionOwner> {
        Some(self.admission.owner)
    }
    fn ordered_owner_retirement_owner(&self) -> Option<ubm_mobile::OrderedAdmissionOwner> {
        self.retirement_owner
    }
    fn ordered_admission_negotiation(&self) -> Option<ubm_mobile::OrderedAdmissionNegotiation> {
        Some(self.admission)
    }
    fn submit(&self, request: RadioRequest) {
        if let Err(detail) = request.validate_owner_retirement(
            self.retirement_version,
            self.retirement_owner.unwrap_or(self.admission.owner),
        ) {
            if let Some(host) = self.owner.get().and_then(std::sync::Weak::upgrade) {
                host.host.complete(
                    request.id(),
                    RadioCompletion::Failed(PlatformFailure::not_dispatched(
                        FailureKind::Unsupported,
                        detail,
                    )),
                );
            } else {
                eprintln!("ubm-mobile uniffi: invalid request before host existed: {detail}");
            }
            return;
        }
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

#[derive(Default)]
struct HostSlot {
    host: Option<Arc<MobileCoreHost>>,
    installing: bool,
    pending: Option<PendingInstallation>,
}

enum PendingInstallation {
    Host(Arc<MobileCoreHost>),
    // A construction failure does not supply a handle or a retirement receipt.
    // Retain both foreign callbacks and refuse replacement until the core can
    // provide positive construction cleanup evidence.
    Construction {
        _radio: Arc<ForeignPlatformRadio>,
        _wake: Arc<ForeignWake>,
    },
}

fn host_slot() -> &'static Mutex<HostSlot> {
    static SLOT: OnceLock<Mutex<HostSlot>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HostSlot::default()))
}

// Foreign callbacks can reenter the binding. Reserve the slot, but never
// hold its mutex while negotiating or constructing the host.
struct HostInstallation;

impl HostInstallation {
    fn reserve() -> Option<Self> {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.installing || slot.host.is_some() || slot.pending.is_some() {
            return None;
        }
        slot.installing = true;
        Some(Self)
    }

    fn publish(self, host: Arc<MobileCoreHost>) {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.host = Some(Arc::clone(&host));
        host.binding_state.store(BINDING_ACTIVE, Ordering::Release);
        slot.installing = false;
        drop(slot);
        std::mem::forget(self);
    }

    fn retain(self, pending: PendingInstallation) {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let PendingInstallation::Host(host) = &pending {
            host.binding_state.store(BINDING_FAILED, Ordering::Release);
        }
        slot.pending = Some(pending);
        slot.installing = false;
        drop(slot);
        std::mem::forget(self);
    }
}

impl Drop for HostInstallation {
    fn drop(&mut self) {
        host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .installing = false;
    }
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
    install_host(radio, wake, None, platform, owner, adapter_label)
}

pub fn mobile_host_install_bound(
    radio: Box<dyn MobilePlatformRadio>,
    wake: Box<dyn MobileWakeSink>,
    binding: Box<dyn MobileHostBinding>,
    platform: String,
    owner: String,
    adapter_label: String,
) -> Result<Arc<MobileCoreHost>, MobileCoreError> {
    install_host(radio, wake, Some(binding), platform, owner, adapter_label)
}

fn install_host(
    radio: Box<dyn MobilePlatformRadio>,
    wake: Box<dyn MobileWakeSink>,
    binding: Option<Box<dyn MobileHostBinding>>,
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
            ));
        }
    };
    let installation = HostInstallation::reserve().ok_or_else(|| {
        failed(
            "lifecycle.invalid-state",
            OP,
            "a mobile host is already installed or installing in this process",
        )
    })?;
    let response =
        radio.ordered_admission_owner(ubm_mobile::ORDERED_FAILURE_SUMMARY_ADMISSION_VERSION);
    let admission = ubm_mobile::OrderedAdmissionNegotiation::negotiate(
        ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION,
        &response,
    )
    .map_err(|detail| MobileCoreError::Failed {
        code: if response.is_empty() {
            "protocol.incompatible"
        } else {
            "protocol.malformed"
        }
        .to_owned(),
        domain: "boundary".to_owned(),
        operation: OP.to_owned(),
        detail: Some(detail.to_owned()),
    })?;
    let retirement_response =
        radio.ordered_admission_owner(ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION);
    let (retirement_version, retirement_owner) = if retirement_response.is_empty() {
        (0, None)
    } else {
        let retirement = ubm_mobile::OrderedAdmissionNegotiation::negotiate(
            ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION,
            &retirement_response,
        )
        .map_err(|detail| failed("protocol.malformed", OP, detail))?;
        (
            ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION,
            Some(retirement.owner),
        )
    };
    // This is the core's first validation and happens before any anchor or
    // radio construction. Preserve that no-effect failure without inventing
    // cleanup debt for an operation that never began.
    if owner.is_empty() {
        return Err(MobileCoreError::Failed {
            code: "argument.invalid".to_owned(),
            domain: "core".to_owned(),
            operation: "ubm-mobile.host.owner".to_owned(),
            detail: None,
        });
    }
    let foreign_radio = Arc::new(ForeignPlatformRadio {
        radio,
        admission,
        retirement_version,
        retirement_owner,
        owner: OnceLock::new(),
    });
    let foreign_wake = Arc::new(ForeignWake { wake });
    let host = match MobileHost::open_blocking_with_recording_registry(
        foreign_radio.clone(),
        foreign_wake.clone(),
        HostOptions {
            platform,
            owner,
            adapter_label,
        },
        ubm_desktop::executor::desktop_runtime(),
        ubm_mobile::continuation::process_recording_registry(),
    ) {
        Ok(host) => host,
        Err(error) => {
            installation.retain(PendingInstallation::Construction {
                _radio: foreign_radio,
                _wake: foreign_wake,
            });
            return Err(error.into());
        }
    };
    let handle = Arc::new(MobileCoreHost {
        host,
        binding_state: AtomicU8::new(BINDING_IN_PROGRESS),
        shutdown_gate: ShutdownGate::default(),
        source_failure_receipts: Mutex::new(SourceFailureReceipts::default()),
        notification_failure_spool: Mutex::new(None),
        notification_failure_spool_configuration_authority_claimed: AtomicBool::new(false),
        scan_failure_recovery_configuration_authority_claimed: AtomicBool::new(false),
        terminal_recovery: Mutex::new(TerminalRecoveryHandles::default()),
    });
    let _ = foreign_radio.owner.set(Arc::downgrade(&handle));
    let weak = Arc::downgrade(&handle);
    handle.host.observe_shutdown_completion(Arc::new(move |_| {
        if let Some(host) = weak.upgrade() {
            host.release_exact_binding();
        }
    }));
    if let Some(binder) = binding {
        let accepted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            binder.bind(Arc::clone(&handle))
        }));
        let detail = match accepted {
            Ok(true) => None,
            Ok(false) => Some("native host binding was refused; cleanup remains pending"),
            Err(_) => Some("native host binding callback failed; cleanup remains pending"),
        };
        if let Some(detail) = detail {
            installation.retain(PendingInstallation::Host(handle));
            return Err(failed("lifecycle.invalid-state", OP, detail));
        }
    }
    installation.publish(Arc::clone(&handle));
    Ok(handle)
}

pub fn mobile_host_has_pending_cleanup() -> bool {
    host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending
        .is_some()
}

pub fn mobile_host_retry_pending_cleanup() -> String {
    let host = {
        let slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.installing {
            return binding_cleanup_failure(BindingCleanupFailure::BindingInProgress);
        }
        match &slot.pending {
            None => return binding_cleanup_failure(BindingCleanupFailure::NoPending),
            Some(PendingInstallation::Construction { .. }) => {
                return binding_cleanup_failure(BindingCleanupFailure::ConstructionUnknown);
            }
            Some(PendingInstallation::Host(host)) => Arc::clone(host),
        }
    };
    let record = host.shutdown();
    let retired = {
        let mut slot = host_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ubm_mobile::binding_release_succeeded(&record)
            && matches!(&slot.pending, Some(PendingInstallation::Host(current)) if Arc::ptr_eq(current, &host))
        {
            slot.pending.take()
        } else {
            None
        }
    };
    drop(retired);
    record
}

/// UDL `mobile_host_current`.
pub fn mobile_host_current() -> Option<Arc<MobileCoreHost>> {
    host_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .host
        .clone()
}

pub fn mobile_ordered_recording_version() -> u32 {
    0
}
pub fn mobile_recording_ordered_offline(
    operation: String,
    id: String,
    completion: Box<dyn MobileInvokeCompletion>,
) {
    ubm_mobile::continuation::recording_ordered_offline(
        operation,
        id,
        Box::new(move |value| completion.complete(value)),
    );
}

pub fn mobile_recording_configure_directory(path: String) -> String {
    ubm_desktop::continuation::envelope(
        ubm_mobile::continuation::process_recording_registry()
            .configure_directory(std::path::Path::new(&path))
            .map_err(ubm_desktop::continuation::recording_failure),
    )
}

pub fn mobile_configure_terminal_evidence_maximum_bytes(
    maximum_bytes: u64,
) -> Result<(), MobileCoreError> {
    ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(maximum_bytes).map_err(
        |detail| MobileCoreError::Failed {
            code: "argument.invalid".to_owned(),
            domain: "core".to_owned(),
            operation: "mobile.terminal-evidence.configure".to_owned(),
            detail: Some(detail.to_owned()),
        },
    )?;
    if let Some(host) = mobile_host_current() {
        host.host
            .configure_terminal_evidence_maximum_bytes(maximum_bytes)
            .map_err(MobileCoreError::from)?;
    }
    Ok(())
}

pub fn mobile_recording_control(
    operation: String,
    id: String,
    token: String,
    max_items: u32,
    max_bytes: u32,
) -> String {
    if let Some(owner) = mobile_host_current() {
        owner
            .host
            .continuation_recording_control(&operation, &id, &token, max_items, max_bytes)
    } else {
        ubm_mobile::continuation::recording_control(
            &ubm_mobile::continuation::process_recording_registry(),
            None,
            &operation,
            &id,
            &token,
            max_items,
            max_bytes,
        )
    }
}

const BINDING_IN_PROGRESS: u8 = 0;
const BINDING_ACTIVE: u8 = 1;
const BINDING_FAILED: u8 = 2;
const BINDING_RELEASED: u8 = 3;

#[derive(Default)]
struct ShutdownGate {
    state: Mutex<ShutdownGateState>,
}

#[derive(Default)]
struct ShutdownGateState {
    running: bool,
    released: Option<String>,
}

impl ShutdownGate {
    fn run(&self, cleanup: impl FnOnce() -> String) -> String {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(record) = &state.released {
                return record.clone();
            }
            if state.running {
                return binding_cleanup_failure(BindingCleanupFailure::ShutdownInProgress);
            }
            state.running = true;
        }
        // The host may synchronously call the foreign adapter or reenter this
        // gate. Never hold the gate across shutdown or callback delivery.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running = false;
        let record = match outcome {
            Ok(record) => record,
            Err(error) => {
                drop(state);
                std::panic::resume_unwind(error);
            }
        };
        if ubm_mobile::binding_release_succeeded(&record) {
            state.released = Some(record.clone());
        }
        record
    }
}

// A single ordered producer may have one unacknowledged receipt. Keeping the
// last accepted receipt makes a lost reply replayable without another enqueue;
// sequence numbers also fence stale retries after a later acceptance.
#[derive(Clone, PartialEq, Eq)]
struct SourceFailureReceipt {
    sequence: u64,
    instance: MobileInstance,
    epoch: u64,
    failure: MobilePlatformFailure,
}

#[derive(Default)]
struct SourceFailureReceipts {
    accepted: Option<SourceFailureReceipt>,
    pending: Option<SourceFailureReceipt>,
}

impl SourceFailureReceipts {
    fn admit(
        &mut self,
        receipt: &SourceFailureReceipt,
        deliver: impl FnOnce() -> ubm_mobile::SourceFailureIngressStatus,
    ) -> &'static str {
        if let Some(accepted) = &self.accepted {
            if receipt.sequence == accepted.sequence {
                return if receipt == accepted {
                    "accepted"
                } else {
                    "invalid"
                };
            }
        }
        let previous = self.accepted.as_ref().map_or(0, |value| value.sequence);
        if previous.checked_add(1) != Some(receipt.sequence)
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending != receipt)
        {
            return "invalid";
        }
        self.pending = Some(receipt.clone());
        match deliver() {
            ubm_mobile::SourceFailureIngressStatus::Accepted => {
                self.accepted = self.pending.take();
                "accepted"
            }
            ubm_mobile::SourceFailureIngressStatus::Full => "full",
            ubm_mobile::SourceFailureIngressStatus::Closed => "closed",
            ubm_mobile::SourceFailureIngressStatus::Invalid => "invalid",
        }
    }
}

/// Mirrors UDL `interface MobileCoreHost`.
pub struct MobileCoreHost {
    host: MobileHost,
    binding_state: AtomicU8,
    shutdown_gate: ShutdownGate,
    source_failure_receipts: Mutex<SourceFailureReceipts>,
    notification_failure_spool:
        Mutex<Option<Arc<ubm_desktop::continuation_journal::JournalAppleNotificationFailureSpool>>>,
    notification_failure_spool_configuration_authority_claimed: AtomicBool,
    scan_failure_recovery_configuration_authority_claimed: AtomicBool,
    terminal_recovery: Mutex<TerminalRecoveryHandles>,
}
struct TerminalRecoveryHandles {
    next: u64,
    lease_id: Option<String>,
    lease: Option<Arc<ubm_mobile::TerminalEvidenceRecovery>>,
    entries: std::collections::HashMap<String, ubm_mobile::TerminalEvidenceRecoveryEntry>,
    // The same bounded duplicate-cleanup acknowledgement window as JNI.
    // It is scoped to one opaque recovery lease and never grants authority to
    // a recreated handle after the lease has closed.
    acknowledged: std::collections::HashSet<String>,
    acknowledged_order: std::collections::VecDeque<String>,
}
const TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY: usize = 256;
impl Default for TerminalRecoveryHandles {
    fn default() -> Self {
        Self {
            next: 0,
            lease_id: None,
            lease: None,
            entries: std::collections::HashMap::new(),
            acknowledged: std::collections::HashSet::new(),
            acknowledged_order: std::collections::VecDeque::new(),
        }
    }
}

impl TerminalRecoveryHandles {
    fn retain_acknowledgement(&mut self, handle: String) {
        if !self.acknowledged.insert(handle.clone()) {
            return;
        }
        self.acknowledged_order.push_back(handle);
        if self.acknowledged_order.len() > TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY {
            if let Some(expired) = self.acknowledged_order.pop_front() {
                self.acknowledged.remove(&expired);
            }
        }
    }

    fn clear_lease(&mut self) {
        self.lease_id = None;
        self.lease = None;
        self.entries.clear();
        self.acknowledged.clear();
        self.acknowledged_order.clear();
    }
}

fn terminal_recovery_error(error: DesktopError) -> String {
    serde_json::json!({
        "ok": false,
        "error": {
            "code": error.code_str(),
            "domain": error.domain().as_str(),
            "operation": error.operation(),
            "detail": error.detail(),
        }
    })
    .to_string()
}

fn terminal_recovery_invalid(operation: &str, detail: &str) -> String {
    serde_json::json!({
        "ok": false,
        "error": {
            "code": "lifecycle.invalid-state",
            "domain": "core",
            "operation": operation,
            "detail": detail,
        }
    })
    .to_string()
}

fn terminal_recovery_item(
    handle: String,
    value: &ubm_mobile::TerminalEvidenceRecoveryEntry,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("handle".into(), handle.into());
    object.insert("producer".into(), value.producer.clone().into());
    object.insert(
        "scope".into(),
        serde_json::json!({
            "managerId": value.scope.manager_id,
            "operationId": value.scope.operation_id,
            "incarnation": value.scope.incarnation,
        }),
    );
    object.insert(
        "state".into(),
        match value.state {
            ubm_mobile::TerminalEvidenceRecoveryState::Staged => "staged".into(),
            ubm_mobile::TerminalEvidenceRecoveryState::Committed => "committed".into(),
        },
    );
    if let Some(reference) = &value.reference {
        object.insert("reference".into(), reference.clone().into());
    }
    if let Some(length) = value.byte_length {
        object.insert("byteLength".into(), length.to_string().into());
    }
    if let Some(digest) = value.digest {
        object.insert(
            "digest".into(),
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                .into(),
        );
    }
    if let Some(version) = value.format_version {
        object.insert("formatVersion".into(), version.into());
    }
    serde_json::Value::Object(object)
}

/// Opaque capability claimed by the production adapter while the host binding
/// is installed. It cannot be reconstructed from a recording identifier.
pub struct MobileNotificationFailureSpoolConfigurationAuthority {
    host: std::sync::Weak<MobileCoreHost>,
}

/// Opaque native-host authority. It can be obtained only while installation
/// is private, preventing renderer/JavaScript code from binding recordings.
pub struct MobileScanFailureRecoveryConfigurationAuthority {
    host: std::sync::Weak<MobileCoreHost>,
}

/// Opaque foreign recovery holder. Its contained Rust handle is issued only
/// by the trusted pre-publication attachment authority.
pub struct MobileScanFailureRecovery {
    recovery: Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>,
}

/// One exact captured owner. The pending capability remains private to this
/// holder and is never represented in a UDL field or a diagnostic string.
pub struct MobileScanFailureRecoveryOwner {
    recovery: Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>,
    pending: PendingScanFailure,
}

/// One exact prepared recipient descriptor. This is an unforgeable foreign
/// handle, rather than a serialized head capability supplied by JavaScript.
pub struct MobileScanFailureRecoveryHead {
    recovery: Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>,
    head: ScanFailureHead,
}

/// Opaque native-holder of a durable cleanup retirement capability.  The
/// capability's bytes deliberately never appear in a UDL dictionary, error,
/// or log record.
pub struct MobileScanFailureCleanupReceipt {
    recovery: Arc<ubm_desktop::scan_failure_recovery::ScanFailureRecoveryLease>,
    receipt: ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement,
}

#[derive(Clone)]
pub struct MobileScanFailureIdentity {
    pub host: Vec<u8>,
    pub scan: Vec<u8>,
    pub start: Vec<u8>,
    pub source_generation: Vec<u8>,
    pub occurrence: Vec<u8>,
}

#[derive(Clone)]
pub struct MobileScanFailureMember {
    pub recipient: Vec<u8>,
    pub delivery: Vec<u8>,
}

pub struct MobileScanFailureRecoveryOwnerInfo {
    pub owner_id: String,
    pub prepared: bool,
    pub producer_finished: bool,
    pub raw_retired: bool,
    pub raw_released: bool,
    pub member_count: u64,
    pub released: bool,
}

pub struct MobileScanFailureRecoveryHeadInfo {
    pub protocol: u32,
    pub representation: String,
    pub identity: MobileScanFailureIdentity,
    pub member: MobileScanFailureMember,
    pub encoded_length: String,
    pub chunk_bytes: u32,
}

pub struct MobileScanFailureRecoveryPage {
    pub offset: String,
    pub next_offset: String,
    pub bytes: Vec<u8>,
    pub done: bool,
}

pub struct MobileScanFailureRawInfo {
    pub identity: MobileScanFailureIdentity,
    pub member_count: u64,
    pub token_count: u64,
}

pub enum MobileScanFailureRawToken {
    Null,
    Bool { value: bool },
    I64 { value: i64 },
    U64 { value: String },
    F64Bits { value: String },
    Text { total: String, bytes: Vec<u8> },
    Bytes { total: String, bytes: Vec<u8> },
    Map { entries: String },
    Array { entries: String },
}

impl MobileCoreHost {
    pub fn terminal_evidence_recovery_open_selected(&self) -> String {
        let recovery = match self.host.terminal_evidence_recovery_selected() {
            Ok(recovery) => recovery,
            Err(error) => return terminal_recovery_error(error),
        };
        let mut handles = self
            .terminal_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        handles.next += 1;
        let id = format!("terminal-evidence-recovery-swift-{}", handles.next);
        handles.clear_lease();
        handles.lease_id = Some(id.clone());
        handles.lease = Some(Arc::new(recovery));
        serde_json::json!({"ok":true,"value":{"leaseId":id}}).to_string()
    }
    pub fn terminal_evidence_recovery_list(
        &self,
        lease_id: String,
        max_items: u32,
        max_bytes: u64,
    ) -> String {
        let mut handles = self
            .terminal_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handles.lease_id.as_deref() != Some(lease_id.as_str()) {
            return terminal_recovery_invalid(
                "terminal-evidence-recovery.list",
                "unknown or revoked recovery lease",
            );
        }
        let Some(lease) = handles.lease.clone() else {
            return terminal_recovery_invalid(
                "terminal-evidence-recovery.list",
                "unknown or revoked recovery lease",
            );
        };
        let page = match lease.list(max_items, max_bytes) {
            Ok(page) => page,
            Err(error) => return terminal_recovery_error(error),
        };
        let mut items = Vec::with_capacity(page.entries.len());
        for value in page.entries {
            let id = handles
                .entries
                .iter()
                .find_map(|(id, current)| (current == &value).then(|| id.clone()))
                .unwrap_or_else(|| {
                    handles.next += 1;
                    let id = format!("terminal-evidence-entry-swift-{}", handles.next);
                    handles.entries.insert(id.clone(), value.clone());
                    id
                });
            items.push(terminal_recovery_item(id, &value));
        }
        serde_json::json!({"ok":true,"value":{"items":items,"more":page.more}}).to_string()
    }
    pub fn terminal_evidence_recovery_list_with_status(
        &self,
        lease_id: String,
        max_items: u32,
        max_bytes: u64,
    ) -> String {
        let mut handles = self
            .terminal_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handles.lease_id.as_deref() != Some(lease_id.as_str()) {
            return terminal_recovery_invalid(
                "terminal-evidence-recovery.list-status",
                "unknown or revoked recovery lease",
            );
        }
        let Some(lease) = handles.lease.clone() else {
            return terminal_recovery_invalid(
                "terminal-evidence-recovery.list-status",
                "unknown or revoked recovery lease",
            );
        };
        let page = match lease.list_with_status(max_items, max_bytes) {
            Ok(page) => page,
            Err(error) => return terminal_recovery_error(error),
        };
        let mut items = Vec::with_capacity(page.entries.len());
        for value in page.entries {
            let id = handles
                .entries
                .iter()
                .find_map(|(id, current)| (current == &value).then(|| id.clone()))
                .unwrap_or_else(|| {
                    handles.next += 1;
                    let id = format!("terminal-evidence-entry-swift-{}", handles.next);
                    handles.entries.insert(id.clone(), value.clone());
                    id
                });
            items.push(terminal_recovery_item(id, &value));
        }
        serde_json::json!({"ok":true,"value":{"items":items,"more":page.more,"busyProducers":page.busy_producers}}).to_string()
    }
    pub fn terminal_evidence_recovery_read(
        &self,
        handle: String,
        offset: String,
        max_bytes: u32,
    ) -> String {
        let offset = match offset.parse() {
            Ok(offset) => offset,
            Err(_) => {
                return terminal_recovery_invalid(
                    "terminal-evidence-recovery.read",
                    "offset must be canonical unsigned decimal",
                );
            }
        };
        let handles = self
            .terminal_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (Some(lease), Some(entry)) = (handles.lease.clone(), handles.entries.get(&handle))
        else {
            return terminal_recovery_invalid(
                "terminal-evidence-recovery.read",
                "unknown recovery entry",
            );
        };
        match lease.read(entry, offset, max_bytes) { Ok(page) => serde_json::json!({"ok":true,"value":{"bytesBase64":base64(&page.bytes),"more":!page.done}}).to_string(), Err(error) => terminal_recovery_error(error) }
    }
    pub fn terminal_evidence_recovery_acknowledge(&self, handle: String) -> String {
        let mut handles = self
            .terminal_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handles.acknowledged.contains(&handle) {
            return r#"{"ok":true,"value":{}}"#.into();
        }
        let (Some(lease), Some(entry)) =
            (handles.lease.clone(), handles.entries.get(&handle).cloned())
        else {
            return terminal_recovery_invalid(
                "terminal-evidence-recovery.acknowledge",
                "unknown recovery entry",
            );
        };
        match lease.acknowledge(&entry) {
            Ok(()) => {
                handles.retain_acknowledgement(handle);
                r#"{"ok":true,"value":{}}"#.into()
            }
            Err(error) => terminal_recovery_error(error),
        }
    }
    fn install_notification_failure_spool(
        &self,
        spool: Arc<ubm_desktop::continuation_journal::JournalAppleNotificationFailureSpool>,
    ) -> std::result::Result<(), String> {
        let mut slot = self
            .notification_failure_spool
            .lock()
            .map_err(|_| "closed".to_owned())?;
        if let Some(current) = slot.as_ref() {
            if current.recording_id() != spool.recording_id()
                && current
                    .has_retained_occurrences()
                    .map_err(|error| error.kind.to_owned())?
            {
                return Err("storage.busy".to_owned());
            }
        }
        *slot = Some(spool);
        Ok(())
    }

    fn release_exact_binding(&self) {
        self.binding_state
            .store(BINDING_RELEASED, Ordering::Release);
        let retired = {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let active = slot
                .host
                .as_ref()
                .is_some_and(|current| std::ptr::eq(current.as_ref(), self));
            let pending = matches!(&slot.pending, Some(PendingInstallation::Host(current)) if std::ptr::eq(current.as_ref(), self));
            (
                if active { slot.host.take() } else { None },
                if pending { slot.pending.take() } else { None },
            )
        };
        drop(retired);
    }

    fn binding_active(&self) -> bool {
        self.binding_state.load(Ordering::Acquire) == BINDING_ACTIVE
    }

    fn inactive_binding_envelope() -> String {
        r#"{"ok":false,"error":{"code":"lifecycle.invalid-state","domain":"core","operation":"mobile.host.binding","detail":"host binding is not active"},"commit":null,"retryability":null}"#.to_owned()
    }

    pub fn continuation_configure_recording_directory(&self, path: String) -> String {
        if !self.binding_active() {
            return Self::inactive_binding_envelope();
        }
        self.host
            .continuation_configure_recording_directory(std::path::Path::new(&path))
    }
    fn configure_notification_failure_spool_v1_from_authority(
        &self,
        recording_id: String,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        match self.host.notification_failure_spool(&recording_id) {
            Ok(spool) => match self.install_notification_failure_spool(spool) {
                Ok(()) => "configured".to_owned(),
                Err(status) => status,
            },
            Err(error) if error.kind == "storage.full" => "full".to_owned(),
            Err(error) => error.kind.to_owned(),
        }
    }
    /// V2 configures a separately bounded spool. The result is JSON so the
    /// host can truthfully report this budget independently of its recording.
    fn configure_notification_failure_spool_v2_from_authority(
        &self,
        recording_id: String,
        max_bytes: String,
        max_records: String,
    ) -> String {
        if !self.binding_active() {
            return r#"{"status":"closed"}"#.to_owned();
        }
        let max_bytes_value = match max_bytes.parse::<u64>() {
            Ok(value) => value,
            Err(_) => {
                return r#"{"status":"error","error":{"domain":"argument","code":"argument.invalid"}}"#.to_owned()
            }
        };
        let max_records_value = match max_records.parse::<u64>() {
            Ok(value) => value,
            Err(_) => {
                return r#"{"status":"error","error":{"domain":"argument","code":"argument.invalid"}}"#.to_owned()
            }
        };
        let quota = ubm_desktop::continuation_journal::JournalQuota {
            max_bytes: max_bytes_value,
            max_records: max_records_value,
        };
        match self
            .host
            .notification_failure_spool_with_quota(&recording_id, quota)
        {
            Ok(spool) => match self.install_notification_failure_spool(spool) {
                Ok(()) => serde_json::json!({"status":"configured","quota":{"maxBytes":max_bytes_value.to_string(),"maxRecords":max_records_value.to_string()}}).to_string(),
                Err(status) => serde_json::json!({"status":"error","error":{"domain":status.split_once('.').map(|(domain,_)| domain).unwrap_or("storage"),"code":status}}).to_string(),
            },
            Err(error) => serde_json::json!({"status":"error","error":{"domain":error.kind.split_once('.').map(|(domain,_)| domain).unwrap_or("storage"),"code":error.kind}}).to_string(),
        }
    }

    /// Raw configuration is deliberately unsupported: it can bypass the
    /// adapter's route/callback/replay lease. Production claims the opaque
    /// authority during binding and configures through that fenced owner.
    pub fn configure_notification_failure_spool_v1(&self, recording_id: String) -> String {
        let _ = recording_id;
        if !self.binding_active() {
            return "closed".to_owned();
        }
        "unsupported".to_owned()
    }

    pub fn configure_notification_failure_spool_v2(
        &self,
        recording_id: String,
        max_bytes: String,
        max_records: String,
    ) -> String {
        let _ = (recording_id, max_bytes, max_records);
        if !self.binding_active() {
            return r#"{"status":"closed"}"#.to_owned();
        }
        r#"{"status":"error","error":{"domain":"capability","code":"capability.unsupported"}}"#
            .to_owned()
    }

    pub fn claim_notification_failure_spool_configuration_authority_v3(
        self: &Arc<Self>,
    ) -> Option<Arc<MobileNotificationFailureSpoolConfigurationAuthority>> {
        // The native binding can claim exactly once while the host remains
        // unpublished. A published UniFFI host handle cannot acquire an
        // authority that the production binding did not keep.
        if self.binding_state.load(Ordering::Acquire) != BINDING_IN_PROGRESS
            || self
                .notification_failure_spool_configuration_authority_claimed
                .swap(true, Ordering::AcqRel)
        {
            return None;
        }
        Some(Arc::new(
            MobileNotificationFailureSpoolConfigurationAuthority {
                host: Arc::downgrade(self),
            },
        ))
    }

    /// The production binding claims this before it publishes the foreign
    /// host. No public handle can mint recovery access afterwards.
    pub fn claim_scan_failure_recovery_configuration_authority_v1(
        self: &Arc<Self>,
    ) -> Option<Arc<MobileScanFailureRecoveryConfigurationAuthority>> {
        if self.binding_state.load(Ordering::Acquire) != BINDING_IN_PROGRESS
            || self
                .scan_failure_recovery_configuration_authority_claimed
                .swap(true, Ordering::AcqRel)
        {
            return None;
        }
        Some(Arc::new(MobileScanFailureRecoveryConfigurationAuthority {
            host: Arc::downgrade(self),
        }))
    }
    pub fn continuation_recording_control(
        &self,
        operation: String,
        id: String,
        token: String,
        max_items: u32,
        max_bytes: u32,
    ) -> String {
        if !self.binding_active() {
            return Self::inactive_binding_envelope();
        }
        self.host
            .continuation_recording_control(&operation, &id, &token, max_items, max_bytes)
    }
    pub fn continuation_reserve_declaration(&self, declaration_json: String) -> String {
        if !self.binding_active() {
            return Self::inactive_binding_envelope();
        }
        self.host
            .continuation_reserve_declaration(&declaration_json)
    }
    pub fn continuation_commit_declaration(&self, reservation_token: String) -> String {
        if !self.binding_active() {
            return Self::inactive_binding_envelope();
        }
        self.host
            .continuation_commit_declaration(&reservation_token)
    }
    pub fn continuation_cancel_declaration(&self, reservation_token: String) -> String {
        if !self.binding_active() {
            return Self::inactive_binding_envelope();
        }
        self.host
            .continuation_cancel_declaration(&reservation_token)
    }
    pub fn continuation_seed_declaration(&self, declaration_json: String) -> String {
        if !self.binding_active() {
            return Self::inactive_binding_envelope();
        }
        self.host.continuation_seed_declaration(&declaration_json)
    }
    pub fn continuation_declaration_replacement_failure(
        &self,
        declaration_json: String,
    ) -> Option<String> {
        if !self.binding_active() {
            return Some("host binding is not active".to_owned());
        }
        self.host
            .continuation_declaration_replacement_failure(&declaration_json)
    }

    pub fn continuation_describe_backlog(&self, completion: Box<dyn MobileInvokeCompletion>) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host
            .continuation_describe_backlog(Box::new(move |value| completion.complete(value)));
    }

    pub fn continuation_execute(
        &self,
        peer_id: String,
        declaration_json: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_execute(
            &peer_id,
            &declaration_json,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_prepare_claim(
        &self,
        max_items: u32,
        max_bytes: u32,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_prepare_claim(
            max_items,
            max_bytes,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_acknowledge_claim(
        &self,
        claim_token: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_acknowledge_claim(
            &claim_token,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_prepare_retained_claim_v1(
        &self,
        max_items: u32,
        max_bytes: u32,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_prepare_retained_claim_v1(
            max_items,
            max_bytes,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_prepare_claim_versioned(
        &self,
        version: u32,
        max_items: u32,
        max_bytes: u32,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_prepare_claim_versioned(
            version,
            max_items,
            max_bytes,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_acknowledge_retained_claim_v1(
        &self,
        claim_token: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_acknowledge_retained_claim_v1(
            &claim_token,
            Box::new(move |value| completion.complete(value)),
        );
    }

    /// V2 is an additive, explicitly selected retained-prefix protocol. It is
    /// never used to acknowledge a V1 claim.
    pub fn continuation_acknowledge_retained_claim_v2(
        &self,
        claim_token: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_acknowledge_retained_claim_v2(
            &claim_token,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_read_peer_failure_recovery(
        &self,
        recovery_record: String,
        offset: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        let offset_text = offset;
        let Ok(offset) = offset_text.parse::<u64>() else {
            return completion.complete(ubm_desktop::continuation::envelope(Err(serde_json::json!({
                "code":"argument.invalid", "domain":"gatt", "operation":"continuation.peer-source-failure.read", "detail":"offset must be canonical UInt64 decimal"
            }))));
        };
        if offset.to_string() != offset_text {
            return completion.complete(ubm_desktop::continuation::envelope(Err(serde_json::json!({
                "code":"argument.invalid", "domain":"gatt", "operation":"continuation.peer-source-failure.read", "detail":"offset must be canonical UInt64 decimal"
            }))));
        }
        self.host.continuation_read_peer_failure_recovery(
            &recovery_record,
            offset,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_acknowledge_peer_failure_recovery(
        &self,
        recovery_record: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_acknowledge_peer_failure_recovery(
            &recovery_record,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_retire_claim_receipt(
        &self,
        claim_token: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_retire_claim_receipt(
            &claim_token,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_legacy_evidence_page(
        &self,
        claim_token: String,
        after_ordinal: u64,
        max_items: u32,
        max_bytes: u32,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_legacy_evidence_page(
            &claim_token,
            after_ordinal,
            max_items,
            max_bytes,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn continuation_legacy_evidence_retire(
        &self,
        claim_token: String,
        ordinal: u64,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        if !self.binding_active() {
            return completion.complete(Self::inactive_binding_envelope());
        }
        self.host.continuation_legacy_evidence_retire(
            &claim_token,
            ordinal,
            Box::new(move |value| completion.complete(value)),
        );
    }

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

    /// Version 1 requires one ordered producer and a monotonically increasing
    /// receipt sequence. A replay of the last exact acceptance is idempotent.
    pub fn notification_failure_ingress_version(&self, offered: u32) -> u32 {
        u32::from(offered == 1)
    }

    pub fn ingest_notification_failure_v1(
        &self,
        sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
    ) -> String {
        let receipt = SourceFailureReceipt {
            sequence,
            instance,
            epoch,
            failure,
        };
        let mut receipts = self
            .source_failure_receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        receipts
            .admit(&receipt, || {
                if FailureKind::parse(&receipt.failure.kind).is_none() {
                    return ubm_mobile::SourceFailureIngressStatus::Invalid;
                }
                let instance: Instance = receipt.instance.clone().into();
                let failure = match mobile_failure(receipt.failure.clone()) {
                    Ok(failure) => failure,
                    Err(_) => return ubm_mobile::SourceFailureIngressStatus::Invalid,
                };
                self.host
                    .ingest_notification_source_failure(&instance, receipt.epoch, &failure)
            })
            .to_owned()
    }

    /// `committed` means the exact callback is on stable storage. All other
    /// outcomes retain ownership with the platform adapter.
    pub fn append_notification_failure_v1(
        &self,
        sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        if sequence == 0 || FailureKind::parse(&failure.kind).is_none() {
            return "invalid".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let occurrence = ubm_desktop::continuation_journal::AppleNotificationFailureOccurrence {
            route,
            epoch,
            sequence,
            failure: serde_json::json!({"instance":{"peerId":instance.peer_id,"serviceUuid":instance.service_uuid,"serviceOccurrence":instance.service_occurrence,"characteristicUuid":instance.characteristic_uuid,"characteristicOccurrence":instance.characteristic_occurrence},"kind":failure.kind,"gattStatus":failure.gatt_status,"nativeDomain":failure.native_domain,"nativeCode":failure.native_code,"detail":failure.detail,"dispatched":failure.dispatched}),
        };
        // Retain the slot lock until append has reached a terminal durable
        // outcome. Configuration takes this same lock, so it cannot report a
        // successful A→B handoff while an adapter callback still owns an
        // in-flight append through A.
        let slot = match self.notification_failure_spool.lock() {
            Ok(slot) => slot,
            Err(_) => return "closed".to_owned(),
        };
        match slot.as_ref() {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.append(&occurrence) {
                Ok(
                    ubm_desktop::continuation_journal::AppleNotificationFailureAppend::Committed,
                ) => "committed".to_owned(),
                Ok(ubm_desktop::continuation_journal::AppleNotificationFailureAppend::Full) => {
                    "full".to_owned()
                }
                Err(error) if error.kind == "storage.full" => "full".to_owned(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    /// Returns a decimal durable capture watermark for a route (including
    /// `0`), or an explicit unavailable state. Swift uses this before
    /// allocating new sequences after restoration.
    pub fn notification_failure_spool_resume_sequence_v1(
        &self,
        instance: MobileInstance,
        epoch: u64,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.highest_sequence(&route, epoch) {
                Ok(sequence) => sequence.to_string(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    /// V2 exposes the stable, cross-route capture stream. Its ordinal is an
    /// acknowledgement identity only: a restored adapter must assign a fresh
    /// contiguous V1 receipt sequence when it redelivers a page to this
    /// process. That prevents a V1 sink, which starts at receipt one after
    /// process death, from being confused with the original source sequence.
    pub fn notification_failure_spool_resume_global_v2(&self) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.highest_capture_ordinal() {
                Ok(highest_capture_ordinal) => serde_json::json!({
                    "version": 2,
                    "highestCaptureOrdinal": highest_capture_ordinal.to_string(),
                })
                .to_string(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    /// Pages the durable global FIFO. `sourceSequence` is retained diagnostic
    /// source identity; it is deliberately not the V1 delivery sequence used
    /// for the current process's ingress receipt stream.
    pub fn notification_failure_spool_page_global_v2(
        &self,
        after_capture_ordinal: u64,
        max_items: u32,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.page_global(after_capture_ordinal, max_items) {
                Ok(records) => serde_json::json!({
                    "version": 2,
                    "records": records.into_iter().map(|record| serde_json::json!({
                        "captureOrdinal": record.capture_ordinal.to_string(),
                        "route": record.occurrence.route,
                        "epoch": record.occurrence.epoch.to_string(),
                        "sourceSequence": record.occurrence.sequence.to_string(),
                        "failure": record.occurrence.failure,
                    })).collect::<Vec<_>>(),
                })
                .to_string(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    /// Compatibility entrypoint retained for already-generated clients.  An
    /// ordinal alone is not proof of the occurrence that was observed, so it
    /// must never be allowed to delete a global FIFO entry. Use V3, which
    /// carries the complete immutable occurrence identity.
    pub fn acknowledge_notification_failure_spool_global_v2(
        &self,
        _capture_ordinal: u64,
    ) -> String {
        "unsupported".to_owned()
    }

    /// Exact global ACK. The caller must echo the source occurrence returned
    /// by the V2 page alongside its immutable capture ordinal. V2 remains for
    /// deployed compatibility only; new adapters use this proof-bearing form.
    pub fn acknowledge_notification_failure_spool_global_v3(
        &self,
        capture_ordinal: u64,
        source_sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let occurrence = ubm_desktop::continuation_journal::AppleNotificationFailureOccurrence {
            route,
            epoch,
            sequence: source_sequence,
            failure: serde_json::json!({"instance":{"peerId":instance.peer_id,"serviceUuid":instance.service_uuid,"serviceOccurrence":instance.service_occurrence,"characteristicUuid":instance.characteristic_uuid,"characteristicOccurrence":instance.characteristic_occurrence},"kind":failure.kind,"gattStatus":failure.gatt_status,"nativeDomain":failure.native_domain,"nativeCode":failure.native_code,"detail":failure.detail,"dispatched":failure.dispatched}),
        };
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.acknowledge_capture_exact(capture_ordinal, &occurrence) {
                Ok(()) => "acknowledged".to_owned(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    /// V4 returns an authenticated retirement capability after the exact ACK
    /// commits. The adapter must persist that capability before requesting the
    /// separate final confirmation/reclaim operation.
    pub fn acknowledge_notification_failure_spool_global_v4(
        &self,
        capture_ordinal: u64,
        source_sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let occurrence = ubm_desktop::continuation_journal::AppleNotificationFailureOccurrence {
            route,
            epoch,
            sequence: source_sequence,
            failure: serde_json::json!({"instance":{"peerId":instance.peer_id,"serviceUuid":instance.service_uuid,"serviceOccurrence":instance.service_occurrence,"characteristicUuid":instance.characteristic_uuid,"characteristicOccurrence":instance.characteristic_occurrence},"kind":failure.kind,"gattStatus":failure.gatt_status,"nativeDomain":failure.native_domain,"nativeCode":failure.native_code,"detail":failure.detail,"dispatched":failure.dispatched}),
        };
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.acknowledge_capture_exact_v4(capture_ordinal, &occurrence) {
                Ok(capability) => serde_json::json!({"version":4,"status":"acknowledged","retirementCapability":capability}).to_string(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    pub fn confirm_notification_failure_spool_acknowledgement_retirement_v4(
        &self,
        capture_ordinal: u64,
        source_sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
        retirement_capability: String,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let occurrence = ubm_desktop::continuation_journal::AppleNotificationFailureOccurrence {
            route,
            epoch,
            sequence: source_sequence,
            failure: serde_json::json!({"instance":{"peerId":instance.peer_id,"serviceUuid":instance.service_uuid,"serviceOccurrence":instance.service_occurrence,"characteristicUuid":instance.characteristic_uuid,"characteristicOccurrence":instance.characteristic_occurrence},"kind":failure.kind,"gattStatus":failure.gatt_status,"nativeDomain":failure.native_domain,"nativeCode":failure.native_code,"detail":failure.detail,"dispatched":failure.dispatched}),
        };
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.confirm_acknowledgement_retirement_v4(
                capture_ordinal,
                &occurrence,
                &retirement_capability,
            ) {
                Ok(()) => "confirmed".to_owned(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    pub fn notification_failure_spool_pending_retirements_v4(&self, max_items: u32) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.pending_acknowledgement_retirements_v4(max_items) {
                Ok(records) => serde_json::json!({"version":4,"records":records.into_iter().map(|(capture, occurrence, capability)| serde_json::json!({"captureOrdinal":capture.to_string(),"route":occurrence.route,"epoch":occurrence.epoch.to_string(),"sourceSequence":occurrence.sequence.to_string(),"failure":occurrence.failure,"retirementCapability":capability})).collect::<Vec<_>>()}).to_string(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    pub fn retire_notification_failure_spool_acknowledgement_v3(
        &self,
        capture_ordinal: u64,
        source_sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let occurrence = ubm_desktop::continuation_journal::AppleNotificationFailureOccurrence {
            route,
            epoch,
            sequence: source_sequence,
            failure: serde_json::json!({"instance":{"peerId":instance.peer_id,"serviceUuid":instance.service_uuid,"serviceOccurrence":instance.service_occurrence,"characteristicUuid":instance.characteristic_uuid,"characteristicOccurrence":instance.characteristic_occurrence},"kind":failure.kind,"gattStatus":failure.gatt_status,"nativeDomain":failure.native_domain,"nativeCode":failure.native_code,"detail":failure.detail,"dispatched":failure.dispatched}),
        };
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.retire_acknowledgement(capture_ordinal, &occurrence) {
                Ok(()) => "retired".to_owned(),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    /// Bounded durable replay/export page. Records remain owned by the spool
    /// until the exact occurrence is acknowledged after downstream acceptance.
    pub fn notification_failure_spool_page_v1(
        &self,
        instance: MobileInstance,
        epoch: u64,
        after_sequence: u64,
        max_items: u32,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.page(&route, epoch, after_sequence, max_items) {
                Ok(records) => serde_json::to_string(&serde_json::json!({"version":1,"records":records.into_iter().map(|record| serde_json::json!({"epoch":record.epoch.to_string(),"sequence":record.sequence.to_string(),"failure":record.failure})).collect::<Vec<_>>() })).unwrap_or_else(|_| "invalid".to_owned()),
                Err(error) => error.kind.to_owned(),
            },
        }
    }

    pub fn acknowledge_notification_failure_spool_v1(
        &self,
        sequence: u64,
        instance: MobileInstance,
        epoch: u64,
        failure: MobilePlatformFailure,
    ) -> String {
        if !self.binding_active() {
            return "closed".to_owned();
        }
        let route = format!(
            "{}|{}|{}|{}|{}",
            instance.peer_id,
            instance.service_uuid,
            instance.service_occurrence,
            instance.characteristic_uuid,
            instance.characteristic_occurrence
        );
        let occurrence = ubm_desktop::continuation_journal::AppleNotificationFailureOccurrence {
            route,
            epoch,
            sequence,
            failure: serde_json::json!({"instance":{"peerId":instance.peer_id,"serviceUuid":instance.service_uuid,"serviceOccurrence":instance.service_occurrence,"characteristicUuid":instance.characteristic_uuid,"characteristicOccurrence":instance.characteristic_occurrence},"kind":failure.kind,"gattStatus":failure.gatt_status,"nativeDomain":failure.native_domain,"nativeCode":failure.native_code,"detail":failure.detail,"dispatched":failure.dispatched}),
        };
        let spool = match self.notification_failure_spool.lock() {
            Ok(slot) => slot.clone(),
            Err(_) => return "closed".to_owned(),
        };
        match spool {
            None => "unsupported".to_owned(),
            Some(spool) => match spool.acknowledge(&occurrence) {
                Ok(()) => "acknowledged".to_owned(),
                Err(error) => error.kind.to_owned(),
            },
        }
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
        if !self.binding_active() {
            return Err(failed(
                "lifecycle.invalid-state",
                "mobile.host.open-session",
                "host binding is not active",
            ));
        }
        let (session, admission) = self.host.admit_session(
            &owner,
            &expected_wire_revision,
            &mobile_build_identity_json(),
        )?;
        Ok(Arc::new(MobileCoreSession { session, admission }))
    }

    pub fn shutdown(&self) -> String {
        if self.binding_state.load(Ordering::Acquire) == BINDING_IN_PROGRESS {
            return binding_cleanup_failure(BindingCleanupFailure::BindingInProgress);
        }
        let record = self.shutdown_gate.run(|| self.host.shutdown_blocking());
        if ubm_mobile::binding_release_succeeded(&record) {
            self.release_exact_binding();
        }
        record
    }
}

impl MobileNotificationFailureSpoolConfigurationAuthority {
    pub fn configure_v1(&self, recording_id: String) -> String {
        match self.host.upgrade() {
            Some(host) if host.binding_active() => {
                host.configure_notification_failure_spool_v1_from_authority(recording_id)
            }
            _ => "closed".to_owned(),
        }
    }

    pub fn configure_v2(
        &self,
        recording_id: String,
        max_bytes: String,
        max_records: String,
    ) -> String {
        match self.host.upgrade() {
            Some(host) if host.binding_active() => host
                .configure_notification_failure_spool_v2_from_authority(
                    recording_id,
                    max_bytes,
                    max_records,
                ),
            _ => r#"{"status":"closed"}"#.to_owned(),
        }
    }
}

impl MobileScanFailureRecoveryConfigurationAuthority {
    /// Resolve a trusted opaque recording id exclusively through the injected
    /// registry. Recovery credentials stay in the native host's secret store.
    pub fn attach_recording_v1(&self, recording_id: String) -> String {
        match self.host.upgrade() {
            Some(host) if host.binding_active() => host
                .host
                .scan_failure_recovery_registry()
                .attach_trusted(&recording_id)
                .map_or("denied".to_owned(), |_| "attached".to_owned()),
            _ => "closed".to_owned(),
        }
    }

    /// Additive typed attachment used exclusively by the native host holder.
    /// The returned object carries opaque Rust state; it does not serialize
    /// recovery credentials, pending-owner capabilities, or cleanup proofs.
    pub fn attach_recording_v2(
        &self,
        recording_id: String,
    ) -> Result<Arc<MobileScanFailureRecovery>, MobileCoreError> {
        let host = self
            .host
            .upgrade()
            .filter(|host| host.binding_active())
            .ok_or_else(|| {
                failed(
                    "lifecycle.destroyed",
                    "scan.failure-recovery.attach",
                    "host is closed",
                )
            })?;
        let recovery = host
            .host
            .scan_failure_recovery_registry()
            .attach_trusted(&recording_id)
            .map_err(MobileCoreError::from)?;
        Ok(Arc::new(MobileScanFailureRecovery { recovery }))
    }
}

fn scan_recovery_failure(operation: &str) -> MobileCoreError {
    failed(
        "lifecycle.invalid-state",
        operation,
        "scan recovery owner is stale, foreign, or no longer retained",
    )
}

fn scan_u64(value: &str, operation: &str) -> Result<u64, MobileCoreError> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return Err(failed(
            "argument.invalid",
            operation,
            "offset must be canonical decimal u64",
        ));
    }
    value.parse::<u64>().map_err(|_| {
        failed(
            "argument.invalid",
            operation,
            "offset must be canonical decimal u64",
        )
    })
}

fn scan_index(value: u64, operation: &str) -> Result<usize, MobileCoreError> {
    usize::try_from(value).map_err(|_| {
        failed(
            "argument.invalid",
            operation,
            "index does not fit this host",
        )
    })
}

fn scan_count(value: usize, operation: &str) -> Result<u64, MobileCoreError> {
    u64::try_from(value).map_err(|_| scan_recovery_failure(operation))
}

fn scan_max_bytes(value: u32, operation: &str) -> Result<usize, MobileCoreError> {
    usize::try_from(value).map_err(|_| {
        failed(
            "argument.invalid",
            operation,
            "maximum byte count does not fit this host",
        )
    })
}

fn scan_identity(
    value: ubm_core::oversized_evidence::ScanFailureIdentity,
) -> MobileScanFailureIdentity {
    MobileScanFailureIdentity {
        host: value.host.to_vec(),
        scan: value.scan.to_vec(),
        start: value.start.to_vec(),
        source_generation: value.source_generation.to_vec(),
        occurrence: value.occurrence.to_vec(),
    }
}

fn scan_member(value: ubm_core::oversized_evidence::ScanFailureMember) -> MobileScanFailureMember {
    MobileScanFailureMember {
        recipient: value.recipient.to_vec(),
        delivery: value.delivery.to_vec(),
    }
}

impl MobileScanFailureRecovery {
    pub fn owners(&self) -> Vec<Arc<MobileScanFailureRecoveryOwner>> {
        self.recovery
            .owners()
            .into_iter()
            .map(|owner| {
                Arc::new(MobileScanFailureRecoveryOwner {
                    recovery: Arc::clone(&self.recovery),
                    pending: owner.key,
                })
            })
            .collect()
    }

    pub fn pending_cleanup_receipts(
        &self,
    ) -> Result<Vec<Arc<MobileScanFailureCleanupReceipt>>, MobileCoreError> {
        self.recovery
            .pending_cleanup_receipts()
            .map(|receipts| {
                receipts
                    .into_iter()
                    .map(|receipt| {
                        Arc::new(MobileScanFailureCleanupReceipt {
                            recovery: Arc::clone(&self.recovery),
                            receipt,
                        })
                    })
                    .collect()
            })
            .map_err(MobileCoreError::from)
    }
}

impl MobileScanFailureRecoveryOwner {
    pub fn info(&self) -> Result<MobileScanFailureRecoveryOwnerInfo, MobileCoreError> {
        // The inventory call authenticates the recovery holder and is the
        // source of truth across a restart; never retain a stale value copy.
        let owner = self
            .recovery
            .owners()
            .into_iter()
            .find(|owner| owner.key == self.pending);
        match owner {
            Some(owner) => Ok(MobileScanFailureRecoveryOwnerInfo {
                owner_id: owner.key.ordinal.to_string(),
                prepared: owner.prepared,
                producer_finished: owner.producer_finished,
                raw_retired: owner.raw_retired,
                raw_released: owner.raw_released,
                member_count: scan_count(owner.member_count, "scan.failure-recovery.owner-info")?,
                released: owner.released.is_some(),
            }),
            None => Err(scan_recovery_failure("scan.failure-recovery.owner-info")),
        }
    }

    pub fn member(&self, index: u64) -> Result<MobileScanFailureMember, MobileCoreError> {
        self.recovery
            .member(
                &self.pending,
                scan_index(index, "scan.failure-recovery.member")?,
            )
            .map(scan_member)
            .map_err(MobileCoreError::from)
    }

    pub fn head(
        &self,
        member_index: u64,
    ) -> Result<Option<Arc<MobileScanFailureRecoveryHead>>, MobileCoreError> {
        let member = self
            .recovery
            .member(
                &self.pending,
                scan_index(member_index, "scan.failure-recovery.head")?,
            )
            .map_err(MobileCoreError::from)?;
        self.recovery
            .head(member)
            .map(|head| {
                head.map(|head| {
                    Arc::new(MobileScanFailureRecoveryHead {
                        recovery: Arc::clone(&self.recovery),
                        head,
                    })
                })
            })
            .map_err(MobileCoreError::from)
    }

    pub fn raw_info(&self) -> Result<MobileScanFailureRawInfo, MobileCoreError> {
        let info = self
            .recovery
            .raw_info(&self.pending)
            .map_err(MobileCoreError::from)?;
        Ok(MobileScanFailureRawInfo {
            identity: scan_identity(info.identity),
            member_count: scan_count(info.member_count, "scan.failure-recovery.raw-info")?,
            token_count: scan_count(info.token_count, "scan.failure-recovery.raw-info")?,
        })
    }

    pub fn raw_member(&self, index: u64) -> Result<MobileScanFailureMember, MobileCoreError> {
        self.recovery
            .raw_member(
                &self.pending,
                scan_index(index, "scan.failure-recovery.raw-member")?,
            )
            .map(scan_member)
            .map_err(MobileCoreError::from)
    }

    pub fn raw_token(
        &self,
        index: u64,
        offset: String,
        max_bytes: u32,
    ) -> Result<MobileScanFailureRawToken, MobileCoreError> {
        let max_bytes = scan_max_bytes(max_bytes, "scan.failure-recovery.raw-token")?;
        self.recovery
            .raw_token(
                &self.pending,
                scan_index(index, "scan.failure-recovery.raw-token")?,
                usize::try_from(scan_u64(&offset, "scan.failure-recovery.raw-token")?).map_err(
                    |_| {
                        failed(
                            "argument.invalid",
                            "scan.failure-recovery.raw-token",
                            "offset does not fit this host",
                        )
                    },
                )?,
                max_bytes,
            )
            .map(|token| match token {
                ScanFailureRawToken::Null => MobileScanFailureRawToken::Null,
                ScanFailureRawToken::Bool(value) => MobileScanFailureRawToken::Bool { value },
                ScanFailureRawToken::I64(value) => MobileScanFailureRawToken::I64 { value },
                ScanFailureRawToken::U64(value) => MobileScanFailureRawToken::U64 {
                    value: value.to_string(),
                },
                ScanFailureRawToken::F64Bits(value) => MobileScanFailureRawToken::F64Bits {
                    value: value.to_string(),
                },
                ScanFailureRawToken::Text { total, bytes } => MobileScanFailureRawToken::Text {
                    total: total.to_string(),
                    bytes,
                },
                ScanFailureRawToken::Bytes { total, bytes } => MobileScanFailureRawToken::Bytes {
                    total: total.to_string(),
                    bytes,
                },
                ScanFailureRawToken::Map(entries) => MobileScanFailureRawToken::Map {
                    entries: entries.to_string(),
                },
                ScanFailureRawToken::Array(entries) => MobileScanFailureRawToken::Array {
                    entries: entries.to_string(),
                },
            })
            .map_err(MobileCoreError::from)
    }

    pub fn raw_retire(&self) -> Result<(), MobileCoreError> {
        self.recovery
            .raw_retire(&self.pending)
            .map_err(MobileCoreError::from)
    }
    pub fn raw_release(&self) -> Result<(), MobileCoreError> {
        self.recovery
            .raw_release(&self.pending)
            .map_err(MobileCoreError::from)
    }
    pub fn raw_finalize(&self) -> Result<Arc<MobileScanFailureCleanupReceipt>, MobileCoreError> {
        self.recovery
            .raw_finalize(&self.pending)
            .map(|receipt| {
                Arc::new(MobileScanFailureCleanupReceipt {
                    recovery: Arc::clone(&self.recovery),
                    receipt,
                })
            })
            .map_err(MobileCoreError::from)
    }
}

impl MobileScanFailureRecoveryHead {
    pub fn info(&self) -> MobileScanFailureRecoveryHeadInfo {
        MobileScanFailureRecoveryHeadInfo {
            protocol: u32::from(self.head.evidence.protocol),
            representation: self.head.evidence.representation.to_owned(),
            identity: scan_identity(self.head.evidence.identity),
            member: scan_member(self.head.evidence.member),
            encoded_length: self.head.evidence.encoded_length.to_string(),
            chunk_bytes: self.head.evidence.chunk_bytes,
        }
    }
    pub fn read(&self, offset: String) -> Result<MobileScanFailureRecoveryPage, MobileCoreError> {
        self.recovery
            .read(&self.head, scan_u64(&offset, "scan.failure-recovery.read")?)
            .map(|page| MobileScanFailureRecoveryPage {
                offset: page.offset.to_string(),
                next_offset: page.next_offset.to_string(),
                bytes: page.bytes,
                done: page.done,
            })
            .map_err(MobileCoreError::from)
    }
    pub fn acknowledge(&self) -> Result<(), MobileCoreError> {
        self.recovery
            .acknowledge(&self.head)
            .map_err(MobileCoreError::from)
    }
    pub fn retire(&self) -> Result<(), MobileCoreError> {
        self.recovery
            .retire(&self.head)
            .map_err(MobileCoreError::from)
    }
    pub fn release(&self) -> Result<(), MobileCoreError> {
        self.recovery
            .release(&self.head)
            .map_err(MobileCoreError::from)
    }
    pub fn finalize(&self) -> Result<Arc<MobileScanFailureCleanupReceipt>, MobileCoreError> {
        self.recovery
            .finalize_release(&self.head)
            .map(|receipt| {
                Arc::new(MobileScanFailureCleanupReceipt {
                    recovery: Arc::clone(&self.recovery),
                    receipt,
                })
            })
            .map_err(MobileCoreError::from)
    }
}

impl MobileScanFailureCleanupReceipt {
    pub fn retire(&self) -> Result<(), MobileCoreError> {
        self.recovery
            .retire_cleanup(&self.receipt)
            .map_err(MobileCoreError::from)
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

    pub fn negotiate_scan_end_version(
        &self,
        offered_json: String,
    ) -> Result<String, MobileCoreError> {
        self.session
            .negotiate_scan_end_version(&offered_json)
            .map_err(Into::into)
    }

    /// React Native remains on v1 until every v2 producer and the terminal
    /// reservation are integrated and qualified.
    pub fn negotiate_scan_end_version_qualified(
        &self,
        offered_json: String,
    ) -> Result<String, MobileCoreError> {
        self.session
            .negotiate_scan_end_version_with_max(&offered_json, 1)
            .map_err(Into::into)
    }

    pub fn ordered_recording_receipt_control(
        &self,
        operation: String,
        id: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        self.session.ordered_recording_receipt_control(
            operation,
            id,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn stop_ordered_recording(&self, id: String, completion: Box<dyn MobileInvokeCompletion>) {
        self.session.ordered_recording_control_wire(
            id,
            false,
            Box::new(move |value| completion.complete(value)),
        );
    }
    pub fn ordered_recording_context(
        &self,
        id: String,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        self.session.ordered_recording_control_wire(
            id,
            true,
            Box::new(move |value| completion.complete(value)),
        );
    }

    pub fn start_ordered_recording(
        &self,
        handle: String,
        recording_id: String,
        max_bytes: String,
        max_records: String,
        declaration_json: String,
        budget_ms: u64,
        completion: Box<dyn MobileInvokeCompletion>,
    ) {
        self.session.start_ordered_recording_wire(
            handle,
            recording_id,
            max_bytes,
            max_records,
            declaration_json,
            budget_ms,
            Box::new(move |value| completion.complete(value)),
        );
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

    #[test]
    fn terminal_evidence_acknowledgement_window_is_bounded_and_lease_local() {
        let mut handles = TerminalRecoveryHandles::default();
        for index in 0..=TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY {
            handles.retain_acknowledgement(format!("terminal-{index}"));
        }
        assert_eq!(
            handles.acknowledged.len(),
            TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY
        );
        assert!(!handles.acknowledged.contains("terminal-0"));
        assert!(handles.acknowledged.contains(&format!(
            "terminal-{TERMINAL_EVIDENCE_ACKNOWLEDGEMENT_RETENTION_CAPACITY}"
        )));
        handles.clear_lease();
        assert!(handles.acknowledged.is_empty());
        assert!(handles.acknowledged_order.is_empty());
    }

    // These are the only tests that install the process-wide foreign mobile
    // host. Serialize that narrow shared fixture rather than changing the
    // production slot or the crate's test-thread policy.
    fn process_host_test_gate() -> std::sync::MutexGuard<'static, ()> {
        static GATE: OnceLock<Mutex<()>> = OnceLock::new();
        GATE.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn discard_intentionally_unreleased_host_for_test(host: &Arc<MobileCoreHost>) {
        let retired = {
            let mut slot = host_slot()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                matches!(&slot.host, Some(current) if Arc::ptr_eq(current, host)),
                "test fixture must remove only its exact intentionally unreleased host"
            );
            assert!(!slot.installing && slot.pending.is_none());
            slot.host.take()
        };
        drop(retired);
    }

    #[test]
    fn scan_recovery_error_preserves_desktop_identity_and_detail() {
        let error = DesktopError::new(
            ubm_core::contracts::BleErrorCode::ArgumentInvalid,
            ubm_core::contracts::BleErrorDomain::Gatt,
            "scan.failure-recovery.raw-token",
        )
        .with_detail("offset exceeds retained bytes");

        assert_eq!(
            MobileCoreError::from(error),
            MobileCoreError::Failed {
                code: "argument.invalid".to_owned(),
                domain: "gatt".to_owned(),
                operation: "scan.failure-recovery.raw-token".to_owned(),
                detail: Some("offset exceeds retained bytes".to_owned()),
            }
        );
    }

    #[test]
    fn retired_handle_shutdown_replays_receipt_without_touching_replacement() {
        let gate = ShutdownGate::default();
        let failed = r#"{"failures":[{}],"state":"release-failed"}"#;
        let released = r#"{"failures":[],"state":"released"}"#;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.run(|| panic!("scripted cleanup exception"));
        }))
        .is_err());
        assert_eq!(
            gate.run(|| {
                assert!(gate
                    .run(|| panic!("reentrant cleanup ran"))
                    .contains("cleanup in progress"));
                failed.to_owned()
            }),
            failed
        );
        assert_eq!(gate.run(|| released.to_owned()), released);
        let replacement = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(
            gate.run(|| {
                replacement.store(false, Ordering::SeqCst);
                released.to_owned()
            }),
            released
        );
        assert!(
            replacement.load(Ordering::SeqCst),
            "stale shutdown touched the replacement"
        );
    }

    #[test]
    fn acknowledged_failure_receipt_replays_and_fences_changed_or_stale_requests() {
        let instance = MobileInstance {
            peer_id: "old".into(),
            service_uuid: "180d".into(),
            service_occurrence: 1,
            characteristic_uuid: "2a37".into(),
            characteristic_occurrence: 2,
        };
        let failure = MobilePlatformFailure {
            kind: "platform".into(),
            gatt_status: None,
            native_domain: Some("native".into()),
            native_code: Some(42),
            detail: "original".into(),
            dispatched: true,
        };
        let original = SourceFailureReceipt {
            sequence: 1,
            instance,
            epoch: 7,
            failure,
        };
        let mut ledger = SourceFailureReceipts::default();
        let mut attempts = 0;
        let mut admit = || {
            attempts += 1;
            ubm_mobile::SourceFailureIngressStatus::Full
        };
        assert_eq!(ledger.admit(&original, &mut admit), "full");
        let mut changed = original.clone();
        changed.failure.native_code = Some(43);
        assert_eq!(
            ledger.admit(&changed, || panic!("changed receipt admitted")),
            "invalid"
        );
        assert_eq!(
            ledger.admit(&original, || {
                ubm_mobile::SourceFailureIngressStatus::Accepted
            }),
            "accepted"
        );
        assert_eq!(
            ledger.admit(&original, || panic!("accepted receipt duplicated")),
            "accepted"
        );
        let mut next = original.clone();
        next.sequence = 2;
        assert_eq!(
            ledger.admit(&next, || ubm_mobile::SourceFailureIngressStatus::Closed),
            "closed"
        );
        assert_eq!(
            ledger.admit(&original, || panic!("old replay duplicated")),
            "accepted"
        );
        assert_eq!(
            ledger.admit(&next, || ubm_mobile::SourceFailureIngressStatus::Accepted),
            "accepted"
        );
        assert_eq!(
            ledger.admit(&original, || panic!("stale replay duplicated")),
            "invalid"
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn refused_failure_keeps_exact_pending_receipt_across_retries() {
        let instance = MobileInstance {
            peer_id: "old".into(),
            service_uuid: "180d".into(),
            service_occurrence: 1,
            characteristic_uuid: "2a37".into(),
            characteristic_occurrence: 2,
        };
        let failure = MobilePlatformFailure {
            kind: "platform".into(),
            gatt_status: Some(133),
            native_domain: Some("original-domain".into()),
            native_code: Some(42),
            detail: "original-detail".into(),
            dispatched: false,
        };
        let original = SourceFailureReceipt {
            sequence: 1,
            instance,
            epoch: 7,
            failure,
        };
        let mut ledger = SourceFailureReceipts::default();
        assert_eq!(
            ledger.admit(&original, || ubm_mobile::SourceFailureIngressStatus::Closed),
            "closed"
        );
        assert!(ledger.accepted.is_none());
        assert!(ledger.pending.as_ref() == Some(&original));
        assert_eq!(
            ledger.admit(&original, || ubm_mobile::SourceFailureIngressStatus::Closed),
            "closed"
        );
        let mut changed = original.clone();
        changed.epoch += 1;
        assert_eq!(
            ledger.admit(&changed, || panic!("changed retry reached ingress")),
            "invalid"
        );
        assert!(ledger.pending.as_ref() == Some(&original));
    }

    #[test]
    fn notification_source_failure_preserves_identity_and_native_cause() {
        let instance = MobileInstance {
            peer_id: "peer".into(),
            service_uuid: "180d".into(),
            service_occurrence: 2,
            characteristic_uuid: "2a37".into(),
            characteristic_occurrence: 3,
        };
        let failure = MobilePlatformFailure {
            kind: "platform".into(),
            gatt_status: None,
            native_domain: Some("CBErrorDomain".into()),
            native_code: Some(17),
            detail: "source stopped".into(),
            dispatched: true,
        };
        let result = ingress(MobileRadioIngress::NotificationSourceFailed {
            version: 1,
            instance: instance.clone(),
            epoch: 42,
            failure: failure.clone(),
        })
        .unwrap();
        assert!(
            matches!(result, RadioIngress::NotificationSourceFailed { instance, epoch: 42, failure }
            if instance.peer_id == "peer" && instance.service_occurrence == 2
                && instance.characteristic_occurrence == 3
                && failure.native_domain.as_deref() == Some("CBErrorDomain")
                && failure.native_code == Some(17) && failure.detail == "source stopped")
        );
        assert!(ingress(MobileRadioIngress::NotificationSourceFailed {
            version: 2,
            instance,
            epoch: 42,
            failure,
        })
        .is_err());
    }

    #[test]
    fn marker_status_preserves_admission_and_write_outcome() {
        let status = MobileOrderedMarkerWriteStatus {
            admission: MobileOrderedMarkerAdmission::Admitted { ordinal: u64::MAX },
            write: MobileOrderedMarkerWriteState::Settled { failure: None },
        };
        let result =
            completion(MobileRadioCompletion::OrderedMarkerWriteStatus { status }).unwrap();
        assert!(matches!(
            result,
            RadioCompletion::OrderedMarkerWriteStatus(ubm_mobile::OrderedMarkerWriteStatus {
                admission: ubm_mobile::OrderedMarkerAdmission::Admitted { ordinal: u64::MAX },
                write: ubm_mobile::OrderedMarkerWriteState::Settled { failure: None }
            })
        ));
    }

    #[test]
    fn pending_and_indeterminate_write_survive_uniffi_codec() {
        let failure = MobilePlatformFailure {
            kind: "platform".into(),
            gatt_status: None,
            native_domain: Some("native".into()),
            native_code: Some(9),
            detail: "dispatch uncertain".into(),
            dispatched: true,
        };
        for status in [
            MobileOrderedMarkerWriteStatus {
                admission: MobileOrderedMarkerAdmission::Pending,
                write: MobileOrderedMarkerWriteState::NotStarted { failure: None },
            },
            MobileOrderedMarkerWriteStatus {
                admission: MobileOrderedMarkerAdmission::Admitted { ordinal: u64::MAX },
                write: MobileOrderedMarkerWriteState::Initiating,
            },
            MobileOrderedMarkerWriteStatus {
                admission: MobileOrderedMarkerAdmission::Admitted { ordinal: u64::MAX },
                write: MobileOrderedMarkerWriteState::Started {
                    failure: Some(failure),
                },
            },
        ] {
            let value = MobileRadioCompletion::OrderedMarkerWriteStatus { status };
            let mut bytes = Vec::new();
            <MobileRadioCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::write(
                value.clone(),
                &mut bytes,
            );
            let decoded =
                <MobileRadioCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
                    &mut bytes.as_slice(),
                )
                .unwrap();
            assert_eq!(decoded, value);
            let converted = completion(decoded).unwrap();
            assert!(converted.answers(ubm_mobile::RequestKind::OrderedMarkerWriteSettlement));
        }
    }

    #[test]
    fn ordered_versioned_late_failure_roundtrips_and_refuses_legacy_worker() {
        let record = MobileOrderedRecord::SubsequentSourceFailureV2 {
            failure: MobilePlatformFailure {
                kind: "platform".into(),
                gatt_status: None,
                native_domain: Some("CB".into()),
                native_code: Some(-42),
                detail: "original failure".into(),
                dispatched: false,
            },
            instance: MobileInstance {
                peer_id: "peer".into(),
                service_uuid: "service".into(),
                service_occurrence: u64::MAX,
                characteristic_uuid: "characteristic".into(),
                characteristic_occurrence: 8,
            },
            source_generation: "original-source".into(),
            source_member: u64::MAX,
            epoch: u64::MAX - 1,
            batch_id: u64::MAX,
            record_index: u32::MAX,
        };
        let mut bytes = Vec::new();
        <MobileOrderedRecord as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            record.clone(),
            &mut bytes,
        );
        let mut remaining = bytes.as_slice();
        let decoded = <MobileOrderedRecord as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
            &mut remaining,
        )
        .unwrap();
        assert_eq!(decoded, record);
        assert!(remaining.is_empty());
        assert!(ordered_record(decoded)
            .unwrap_err()
            .contains("requires mobile worker support"));
    }

    #[test]
    fn ordered_uniffi_retains_typed_late_failures_and_unknown_omissions() {
        let failure = MobilePlatformFailure {
            kind: "gatt-status".into(),
            gatt_status: Some(257),
            native_domain: Some("native".into()),
            native_code: Some(257),
            detail: "late source failure".into(),
            dispatched: true,
        };
        let input = MobileOrderedRecord::SubsequentSourceFailure {
            failure: failure.clone(),
        };
        let mut bytes = Vec::new();
        <MobileOrderedRecord as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            input.clone(),
            &mut bytes,
        );
        let mut remaining = bytes.as_slice();
        let roundtrip = <MobileOrderedRecord as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
            &mut remaining,
        )
        .unwrap();
        assert_eq!(roundtrip, input);
        assert!(remaining.is_empty());
        let decoded = ordered_record(roundtrip).unwrap();
        assert!(
            matches!(decoded, OrderedRecord::SubsequentSourceFailure { failure } if failure.gatt_status == Some(257) && failure.dispatched)
        );
        assert!(matches!(
            ordered_record(MobileOrderedRecord::FailureObservationsOmitted).unwrap(),
            OrderedRecord::FailureObservationsOmitted
        ));
        let decoded = ordered_record(MobileOrderedRecord::SourceFailed {
            uncertain_tail: true,
            failure,
            native_domain: Some("native".into()),
            native_code: Some("257".into()),
            failure_member: None,
            rejected: None,
        })
        .unwrap();
        assert!(
            matches!(decoded, OrderedRecord::SourceFailed { failure, .. } if failure.gatt_status == Some(257))
        );
    }

    #[test]
    fn ordered_uniffi_codec_preserves_u64_and_scoped_failure() {
        let member = MobileOrderedRegistration {
            member_id: 3,
            source_member: u64::MAX,
            epoch: u64::MAX - 1,
        };
        let value = MobileRadioCompletion::OrderedRecords {
            batch_id: Some(u64::MAX),
            records: vec![
                MobileOrderedRecord::Value {
                    member: member.clone(),
                    ordinal: u64::MAX,
                    value: vec![0, 255],
                    delivery: "unknown".into(),
                    provenance: "read-or-notification".into(),
                },
                MobileOrderedRecord::Marker {
                    marker_id: u64::MAX,
                    ordinal: u64::MAX - 1,
                },
                MobileOrderedRecord::Terminal {
                    reason: "source-failed".into(),
                    uncertain_tail: true,
                    native_domain: Some("CBErrorDomain".into()),
                    native_code: Some("7".into()),
                    failure_member: Some(member),
                    rejected: Some(MobileOrderedRejected::Marker {
                        marker_id: u64::MAX,
                    }),
                },
            ],
        };
        let mut bytes = Vec::new();
        <MobileRadioCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            value.clone(),
            &mut bytes,
        );
        let decoded = <MobileRadioCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
            &mut bytes.as_slice(),
        )
        .unwrap();
        assert_eq!(decoded, value);
        assert!(
            matches!(completion(decoded), Ok(RadioCompletion::OrderedRecords { batch_id: Some(u64::MAX), records }) if matches!(&records[0], OrderedRecord::Value { ordinal: u64::MAX, .. }) && matches!(&records[1], OrderedRecord::Marker { marker_id: u64::MAX, ordinal } if *ordinal == u64::MAX - 1) && matches!(&records[2], OrderedRecord::Terminal { failure_member: Some(member), rejected: Some(OrderedRejected::Marker { marker_id: u64::MAX }), .. } if member.source_member == u64::MAX))
        );
    }

    #[test]
    fn ordered_open_receipt_identity_roundtrips() {
        for request in [
            RadioRequest::OrderedRecoverOpen {
                id: 7,
                group_id: u64::MAX,
                open_request_id: 1 << 63,
                peer_id: "peer".into(),
            },
            RadioRequest::OrderedAcknowledgeOpen {
                id: 8,
                version: 1,
                group_id: u64::MAX,
                open_request_id: 1 << 63,
                peer_id: "peer".into(),
            },
        ] {
            let mapped = MobileRadioRequest::from(&request);
            let mut bytes = Vec::new();
            <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::write(
                mapped.clone(),
                &mut bytes,
            );
            let decoded = <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
                &mut bytes.as_slice(),
            )
            .unwrap();
            assert_eq!(decoded, mapped);
            assert!(matches!(decoded,
                MobileRadioRequest::OrderedRecoverOpen { group_id: u64::MAX, open_request_id, ref peer_id, .. }
                | MobileRadioRequest::OrderedAcknowledgeOpen { group_id: u64::MAX, open_request_id, ref peer_id, .. }
                if open_request_id == 1 << 63 && peer_id == "peer"));
        }
    }

    #[test]
    fn ordered_ack_preserves_generation_and_batch_identity() {
        let request = RadioRequest::OrderedAcknowledgeBatch {
            id: u64::MAX,
            generation: OrderedSourceGeneration::Opaque("source".into()),
            group_id: u64::MAX - 1,
            batch_id: u64::MAX,
        };
        let mapped = MobileRadioRequest::from(&request);
        let mut bytes = Vec::new();
        <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            mapped.clone(),
            &mut bytes,
        );
        let decoded = <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
            &mut bytes.as_slice(),
        )
        .unwrap();
        assert_eq!(decoded, mapped);
        assert!(matches!(
            ingress(MobileRadioIngress::OrderedWake {
                generation: MobileOrderedGeneration::Counter { value: u64::MAX },
                group_id: u64::MAX
            }),
            Ok(RadioIngress::OrderedWake {
                generation: OrderedSourceGeneration::Counter(u64::MAX),
                group_id: u64::MAX
            })
        ));
        assert!(matches!(
            ingress(MobileRadioIngress::OrderedWakeFailure {
                generation: MobileOrderedGeneration::Counter { value: u64::MAX },
                group_id: u64::MAX
            }),
            Ok(RadioIngress::OrderedWakeFailure {
                generation: OrderedSourceGeneration::Counter(u64::MAX),
                group_id: u64::MAX
            })
        ));
        assert!(ingress(MobileRadioIngress::OrderedWake {
            generation: MobileOrderedGeneration::Opaque {
                value: String::new()
            },
            group_id: 1
        })
        .is_err());
    }

    #[test]
    fn malformed_ordered_batches_and_loss_scopes_fail_closed() {
        assert!(completion(MobileRadioCompletion::OrderedRecords {
            batch_id: None,
            records: vec![MobileOrderedRecord::Loss {
                scope: "member".into(),
                member_id: None,
                unit: "source-records".into(),
                count: None,
                byte_count: None
            }]
        })
        .is_err());
        assert!(completion(MobileRadioCompletion::OrderedRecords {
            batch_id: Some(1),
            records: vec![]
        })
        .is_err());
        assert!(ordered_loss("member", None, "values").is_err());
        let member = MobileOrderedRegistration {
            member_id: 1,
            source_member: 2,
            epoch: 3,
        };
        for (delivery, expected) in [
            ("unknown", ObservedDelivery::Unknown),
            ("indication", ObservedDelivery::Indication),
        ] {
            let decoded = ordered_record(MobileOrderedRecord::Value {
                member: member.clone(),
                ordinal: 1,
                value: vec![1],
                delivery: delivery.into(),
                provenance: "notification".into(),
            })
            .unwrap();
            assert!(
                matches!(decoded, OrderedRecord::Value { delivery, .. } if delivery == expected)
            );
        }
        assert!(ordered_record(MobileOrderedRecord::Value {
            member,
            ordinal: 1,
            value: vec![],
            delivery: "guessed".into(),
            provenance: "notification".into()
        })
        .is_err());
    }

    #[test]
    fn companion_filters_survive_the_actual_uniffi_request_codec() {
        for filters_json in [
            None,
            Some(
                r#"[{"namePrefix":"SIM","companyIdentifier":107,"manufacturerPrefix":[0,255]}]"#
                    .to_owned(),
            ),
        ] {
            let radio = RadioRequest::AssociateCompanion {
                id: u64::MAX - 1,
                name: None,
                service_uuid: None,
                filters_json: filters_json.clone(),
            };
            let mapped = MobileRadioRequest::from(&radio);
            assert_eq!(
                mapped,
                MobileRadioRequest::AssociateCompanion {
                    id: u64::MAX - 1,
                    name: None,
                    service_uuid: None,
                    filters_json
                }
            );
            let mut bytes = Vec::new();
            <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::write(
                mapped.clone(),
                &mut bytes,
            );
            let mut remaining = bytes.as_slice();
            let decoded = <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
                &mut remaining,
            )
            .expect("real UniFFI codec decodes companion selector");
            assert_eq!(decoded, mapped);
            assert!(remaining.is_empty());
        }
    }

    struct Recorder {
        tx: Mutex<mpsc::Sender<MobileRadioRequest>>,
    }

    impl MobilePlatformRadio for Recorder {
        fn ordered_admission_owner(&self, version: u32) -> String {
            if version == ubm_mobile::ORDERED_FAILURE_SUMMARY_ADMISSION_VERSION {
                "12345678-1234-4abc-8def-123456789abc".to_owned()
            } else {
                String::new()
            }
        }
        fn submit(&self, request: MobileRadioRequest) {
            if let Ok(tx) = self.tx.lock() {
                let _ = tx.send(request);
            }
        }
        fn cancel(&self, _request_id: u64) {}
    }

    #[test]
    fn uniffi_host_negotiates_retirement_independently_and_dispatches_fence() {
        let _test_gate = process_host_test_gate();
        struct V4Radio {
            tx: Mutex<mpsc::Sender<MobileRadioRequest>>,
        }
        impl MobilePlatformRadio for V4Radio {
            fn ordered_admission_owner(&self, version: u32) -> String {
                if version == ubm_mobile::ORDERED_FAILURE_SUMMARY_ADMISSION_VERSION
                    || version == ubm_mobile::ORDERED_OWNER_RETIREMENT_VERSION
                {
                    "12345678-1234-4abc-8def-123456789abf".to_owned()
                } else {
                    String::new()
                }
            }
            fn submit(&self, request: MobileRadioRequest) {
                self.tx.lock().unwrap().send(request).unwrap();
            }
            fn cancel(&self, _: u64) {}
        }
        let (tx, rx) = mpsc::channel();
        let host = mobile_host_install(
            Box::new(V4Radio { tx: Mutex::new(tx) }),
            Box::new(NoWake),
            "apple".into(),
            "uniffi-v4".into(),
            "test".into(),
        )
        .unwrap();
        let shutdown = std::thread::spawn({
            let host = Arc::clone(&host);
            move || host.shutdown()
        });
        loop {
            let request = rx
                .recv_timeout(std::time::Duration::from_secs(8))
                .expect("host must dispatch the negotiated fence");
            match request {
                MobileRadioRequest::Close { id } => assert_eq!(
                    host.complete(id, MobileRadioCompletion::Closed { failures: vec![] }),
                    "delivered"
                ),
                MobileRadioRequest::OwnerFence {
                    id,
                    owner_incarnation,
                    retirement_id,
                } => {
                    assert_eq!(
                        host.complete(
                            id,
                            MobileRadioCompletion::OwnerRetirement {
                                owner_incarnation,
                                retirement_id,
                                state: MobileOwnerRetirementState::Quiescent
                            }
                        ),
                        "delivered"
                    );
                    break;
                }
                other => panic!("unexpected request: {other:?}"),
            }
        }
        assert!(shutdown.join().unwrap().contains("\"state\":\"released\""));
    }

    #[test]
    fn uniffi_radio_owner_reaches_platform_radio() {
        let (tx, _rx) = mpsc::channel();
        let radio = Box::new(Recorder { tx: Mutex::new(tx) });
        let id =
            radio.ordered_admission_owner(ubm_mobile::ORDERED_FAILURE_SUMMARY_ADMISSION_VERSION);
        let admission = ubm_mobile::OrderedAdmissionNegotiation::negotiate(
            ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION,
            &id,
        )
        .unwrap();
        let platform = ForeignPlatformRadio {
            radio,
            admission,
            retirement_version: 0,
            retirement_owner: None,
            owner: OnceLock::new(),
        };
        assert_eq!(platform.ordered_admission_owner(), Some(admission.owner));
        assert!(ubm_mobile::OrderedAdmissionOwner::negotiate(2, &id).is_err());
        assert!(ubm_mobile::OrderedAdmissionOwner::negotiate(1, "invalid").is_err());
    }

    #[test]
    fn uniffi_platform_reports_the_negotiated_slot_version() {
        let (tx, _rx) = mpsc::channel();
        let admission = ubm_mobile::OrderedAdmissionNegotiation::negotiate(
            2,
            "12345678-1234-4abc-8def-123456789abc",
        )
        .unwrap();
        let platform = ForeignPlatformRadio {
            radio: Box::new(Recorder { tx: Mutex::new(tx) }),
            admission,
            retirement_version: 0,
            retirement_owner: None,
            owner: OnceLock::new(),
        };
        assert_eq!(
            platform.ordered_admission_negotiation().unwrap().version,
            ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION
        );
    }

    #[test]
    fn owner_retirement_rejects_old_versions_and_wrong_owners_before_callback() {
        let (tx, rx) = mpsc::channel();
        let owner = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
            2,
            "12345678-1234-4abc-8def-123456789abc",
        )
        .unwrap();
        let other = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
            2,
            "12345678-1234-4abc-8def-123456789abd",
        )
        .unwrap();
        let retirement_id = ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
            2,
            "22345678-1234-4abc-8def-123456789abc",
        )
        .unwrap();
        let request = RadioRequest::OwnerFence {
            id: 10,
            owner_incarnation: owner,
            retirement_id,
        };
        for (retirement_version, admission) in [
            (
                3,
                ubm_mobile::OrderedAdmissionNegotiation { version: 2, owner },
            ),
            (
                4,
                ubm_mobile::OrderedAdmissionNegotiation {
                    version: 2,
                    owner: other,
                },
            ),
        ] {
            let platform = ForeignPlatformRadio {
                radio: Box::new(Recorder {
                    tx: Mutex::new(tx.clone()),
                }),
                admission,
                retirement_version,
                retirement_owner: None,
                owner: OnceLock::new(),
            };
            platform.submit(request.clone());
            assert!(rx.try_recv().is_err());
        }
        let platform = ForeignPlatformRadio {
            radio: Box::new(Recorder { tx: Mutex::new(tx) }),
            admission: ubm_mobile::OrderedAdmissionNegotiation { version: 2, owner },
            retirement_version: 4,
            retirement_owner: Some(owner),
            owner: OnceLock::new(),
        };
        platform.submit(request.clone());
        assert_eq!(rx.try_recv().unwrap(), MobileRadioRequest::from(&request));
        let reply = completion(MobileRadioCompletion::OwnerRetirement {
            owner_incarnation: owner.to_string(),
            retirement_id: retirement_id.to_string(),
            state: MobileOwnerRetirementState::Quiescent,
        })
        .unwrap();
        assert!(reply.answers_request(&request));
        assert!(reply.answers_request(&RadioRequest::OwnerStatus {
            id: 11,
            owner_incarnation: owner,
            retirement_id
        }));
        assert!(!reply.answers_request(&RadioRequest::OwnerStatus {
            id: 12,
            owner_incarnation: other,
            retirement_id
        }));
        assert!(completion(MobileRadioCompletion::OwnerRetirement {
            owner_incarnation: "bad".into(),
            retirement_id: retirement_id.to_string(),
            state: MobileOwnerRetirementState::Quiescent
        })
        .is_err());
    }

    #[test]
    fn owner_retirement_preserves_all_native_failures_in_order() {
        let owner = "12345678-1234-4abc-8def-123456789abc".to_owned();
        let retirement = "22345678-1234-4abc-8def-123456789abc".to_owned();
        let first = MobileOrderedFailureV2 {
            kind: "permission-denied".to_owned(),
            gatt_status: Some(7),
            native_domain: Some("CBErrorDomain".to_owned()),
            native_code: Some(101),
            native_name: Some("first-native".to_owned()),
            detail: "first failure".to_owned(),
            dispatched: false,
        };
        let second = MobileOrderedFailureV2 {
            kind: "unsupported".to_owned(),
            gatt_status: Some(19),
            native_domain: Some("CBATTErrorDomain".to_owned()),
            native_code: Some(202),
            native_name: Some("second-native".to_owned()),
            detail: "second failure".to_owned(),
            dispatched: true,
        };
        let result = completion(MobileRadioCompletion::OwnerRetirement {
            owner_incarnation: owner,
            retirement_id: retirement,
            state: MobileOwnerRetirementState::FencedPending {
                outstanding_classes: vec![MobileOwnerOutstandingClass::Cleanup],
                failures: vec![first, second],
            },
        })
        .expect("two native failures must decode");
        let RadioCompletion::OwnerRetirement(receipt) = result else {
            panic!("unexpected completion")
        };
        let ubm_mobile::OwnerRetirementState::FencedPending { failures, .. } = receipt.state else {
            panic!("expected retained native failures")
        };
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].kind, FailureKind::PermissionDenied);
        assert_eq!(failures[0].gatt_status, Some(7));
        assert_eq!(failures[0].native_domain.as_deref(), Some("CBErrorDomain"));
        assert_eq!(failures[0].native_code, Some(101));
        assert_eq!(failures[0].native_name.as_deref(), Some("first-native"));
        assert!(!failures[0].dispatched);
        assert_eq!(failures[1].kind, FailureKind::Unsupported);
        assert_eq!(failures[1].gatt_status, Some(19));
        assert_eq!(
            failures[1].native_domain.as_deref(),
            Some("CBATTErrorDomain")
        );
        assert_eq!(failures[1].native_code, Some(202));
        assert_eq!(failures[1].native_name.as_deref(), Some("second-native"));
        assert!(failures[1].dispatched);
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
        let _test_gate = process_host_test_gate();
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
        // ASK authorization returns a UUID before UBM has ever scanned or
        // remembered it. That UUID must reach the actual native radio route;
        // CoreBluetooth's retrievePeripherals resolves OS-authorized peers.
        let chosen = "12345678-1234-1234-1234-123456789ABC";
        for (ordinal, verb) in ["connect", "disconnect"].iter().enumerate() {
            let (result_tx, result_rx) = mpsc::channel();
            session.invoke(
                format!("connection.{verb}"),
                format!(r#"{{"peerId":"{chosen}","lease":"ask-lease","operationId":"ask-{verb}","budgetMs":5000,"admission":{}}}"#, ordinal + 1),
                Box::new(Capture { tx: Mutex::new(result_tx) }),
            );
            let request = request_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|error| {
                    panic!(
                        "chosen peer route failed: {error}; result={:?}",
                        result_rx.try_recv()
                    )
                });
            let id = match request {
                MobileRadioRequest::Connect { id, peer_id, .. } if *verb == "connect" => {
                    assert_eq!(peer_id, chosen);
                    id
                }
                MobileRadioRequest::Disconnect { id, peer_id } if *verb == "disconnect" => {
                    assert_eq!(peer_id, chosen);
                    id
                }
                other => panic!("unexpected chosen-peer radio request: {other:?}"),
            };
            assert_eq!(host.complete(id, MobileRadioCompletion::Unit), "delivered");
            let result = result_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("real host completes chosen-peer operation");
            assert!(result.contains("\"ok\":true"), "{result}");
        }
        assert_eq!(
            host.ingest(MobileRadioIngress::Dropped {
                ingress_class: "bogus".to_owned(),
                detail: String::new(),
            }),
            "dropped-control"
        );
        // Slot retirement is not native-owner quiescence. The actual foreign
        // host remains reachable for callbacks and a later shutdown retry.
        let record = host.shutdown();
        assert!(record.contains("\"state\":\"release-failed\""), "{record}");
        assert!(mobile_host_current().is_some_and(|current| Arc::ptr_eq(&current, &host)));
        let retry = host.shutdown();
        assert!(retry.contains("\"state\":\"release-failed\""), "{retry}");
        assert!(mobile_host_current().is_some_and(|current| Arc::ptr_eq(&current, &host)));
        // This test deliberately proves failed shutdown retains the exact
        // production host. Test-only cleanup happens only after that proof,
        // and removes that exact fixture so it cannot contaminate another
        // independently scheduled test.
        discard_intentionally_unreleased_host_for_test(&host);
        assert!(mobile_host_current().is_none());
    }

    #[test]
    fn scan_recovery_attachment_authority_is_claimed_before_host_publication() {
        struct Binder {
            authority: Arc<Mutex<Option<Arc<MobileScanFailureRecoveryConfigurationAuthority>>>>,
        }
        impl MobileHostBinding for Binder {
            fn bind(&self, host: Arc<MobileCoreHost>) -> bool {
                let authority = host
                    .claim_scan_failure_recovery_configuration_authority_v1()
                    .expect("binding may claim recovery authority once");
                assert!(host
                    .claim_scan_failure_recovery_configuration_authority_v1()
                    .is_none());
                *self.authority.lock().expect("authority storage") = Some(authority);
                true
            }
        }

        let _test_gate = process_host_test_gate();
        let (request_tx, _request_rx) = mpsc::channel();
        let authority = Arc::new(Mutex::new(None));
        let host = mobile_host_install_bound(
            Box::new(Recorder {
                tx: Mutex::new(request_tx),
            }),
            Box::new(NoWake),
            Box::new(Binder {
                authority: Arc::clone(&authority),
            }),
            "apple".to_owned(),
            "uniffi-scan-recovery-authority".to_owned(),
            "corebluetooth".to_owned(),
        )
        .expect("bound host installs");
        assert!(authority.lock().expect("authority storage").is_some());
        assert!(host
            .claim_scan_failure_recovery_configuration_authority_v1()
            .is_none());
        discard_intentionally_unreleased_host_for_test(&host);
    }

    #[test]
    fn connect_carries_the_preferred_phys() {
        let request = MobileRadioRequest::from(&RadioRequest::Connect {
            id: 4,
            peer_id: "p".to_owned(),
            auto_connect: false,
            preferred_phy: vec![ubm_mobile::Phy::Le2m, ubm_mobile::Phy::LeCoded],
            expected_generation: "cg-1".to_owned(),
        });
        let MobileRadioRequest::Connect {
            preferred_phy,
            expected_generation,
            ..
        } = request
        else {
            unreachable!("connect maps to connect: {request:?}");
        };
        assert_eq!(preferred_phy, ["le-2m", "le-coded"]);
        assert_eq!(expected_generation, "cg-1");
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

#[cfg(test)]
#[path = "ordered_admission_tests.rs"]
mod ordered_admission_tests;

#[path = "ordered_admission.rs"]
mod ordered_admission;
pub use ordered_admission::*;
