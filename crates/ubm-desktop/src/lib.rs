//! `ubm-desktop`: host-neutral desktop central radio/executor (HOST-DESKTOP).
//!
//! Portable desktop BLE authority without a Tauri dependency: one shared
//! executor lifetime with explicit shutdown ([`executor`]), contract
//! error identities ([`errors`]), a mockable btleplug radio boundary
//! ([`boundary`]), the production btleplug backend (`btleplug_backend`),
//! the real-core central adapter ([`central`]), and the frozen-matrix
//! parity table ([`capabilities`]).
//!
//! Operation control ([`op_control`]): every central operation takes an
//! [`OpControl`] — the caller budget plus a cancellation ticket.
//!
//! Cargo feature `btleplug` (default on) builds the production
//! `btleplug_backend`. Hosts that bring their own [`RadioBoundary`] build
//! with `default-features = false` and link no btleplug code.
//!
//! No Tauri dependency: the Tauri shell includes `executor.rs` by path as
//! the single-sourced seam. No BLE hardware on the qualification host:
//! radio proof is boundary-fault receipts plus the open physical slice in
//! `PARITY_GAPS.md`, never a claimed radio qualification.

pub mod boundary;
#[cfg(feature = "btleplug")]
pub mod btleplug_backend;
pub mod capabilities;
pub mod central;
pub mod delivery;
pub mod errors;
pub mod executor;
pub mod identity;
pub mod op_control;
pub mod os;

pub use boundary::bluez_bus_supported;
pub use boundary::{
    ATT_DEFAULT_LE_MTU, ATT_MAX_ATTRIBUTE_VALUE, AdapterAuthorization, AdapterAvailability,
    AdapterLossCause, AdapterPowerState, AddressType, AdmissionPolicy, AdvertisementExtras,
    BluezBus, BondState, CharacteristicAccess, CharacteristicRead, CharacteristicSnapshot,
    DeliveryMode, DescriptorKey, DescriptorSnapshot, FakeRadio, FaultOp, HostDeployment,
    InstanceKey, ManufacturerData, ObservationSource, ObservedDelivery, PairOutcome, PeerSnapshot,
    PropertyFlags, RadioBoundary, RadioCloseFailure, RadioEvent, ReadProvenance, ScanFilterSpec,
    SecurityState, ServiceData, ServiceSnapshot, UnpairOutcome, WriteLimits,
};
#[cfg(feature = "btleplug")]
pub use btleplug_backend::BtleplugRadio;
pub use capabilities::{
    CapabilityVerdict, DESKTOP_CAPABILITIES, DesktopCapability, DesktopOs, OsOverride,
    desktop_capability_states, register_desktop_capabilities, register_desktop_capabilities_for,
    register_desktop_capabilities_with_pairing_generation,
};
pub use central::{
    ADAPTER_INITIALIZATION_TIMED_OUT, ADAPTER_INITIALIZATION_TIMEOUT, AdapterEvent,
    AdapterResetEvent, AdapterStatus, COMPLETED_SCAN_TICKET_CAPACITY, CentralObserver,
    CentralProfile, CentralSignal, ConnectionHandle, DesktopCentral, DiscoveredPath,
    DiscoveryReport, InvalidationCause, LIFECYCLE_EVENT_CAPACITY, LifecycleEvent, LifecycleKind,
    LinkRelease, NotificationPoll, PeerRecord, ResourceCounters, ScanObservation, ScanSession,
    ScanStop, ShutdownReport,
};
pub use central::{
    CancelPairingOutcome, PairRequest, PairingGeneration, PairingGenerationController,
    ScanTerminalEvent, SecureConnections, SecurityEvent, WriteReadinessEvent,
};
pub use delivery::{BothPropertiesRule, DeliveryPlan, plan_delivery, platform_rule};
pub use errors::{DesktopError, PlatformDetail, PlatformValue, Retryability};
pub use identity::{AttachmentEpoch, DesktopIdentity, HostIdentity};
pub use op_control::{
    Budget, COMPENSATION_TIMEOUT, CancelAck, CancelRequest, LIVENESS_BACKSTOP_DETAIL,
    LIVENESS_CLEANUP, LIVENESS_OP, LIVENESS_SCAN_START, OpControl, OpTicket,
};
pub use ubm_core::central::{
    CentralResourceCounters, CompletionOutcome, ConnectionState, DatabaseState, PathSelector,
    ScanDuplicatePolicy,
};
pub use ubm_core::contracts::OperationId;
pub use ubm_core::ownership::{CleanupRecord, CleanupState};
