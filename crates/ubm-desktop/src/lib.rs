//! `ubm-desktop`: host-neutral desktop central radio/executor (HOST-DESKTOP).
//!
//! Portable desktop BLE authority without a Tauri dependency: one shared
//! executor lifetime with explicit shutdown ([`executor`]), contract
//! error identities ([`errors`]), a mockable btleplug radio boundary
//! ([`boundary`]), the production btleplug backend ([`btleplug_backend`]),
//! the real-core central adapter ([`central`]), and the frozen-matrix
//! parity table ([`capabilities`]).
//!
//! No Tauri dependency: the Tauri shell includes `executor.rs` by path as
//! the single-sourced seam. No BLE hardware on the qualification host:
//! radio proof is boundary-fault receipts plus the open physical slice in
//! `PARITY_GAPS.md`, never a claimed radio qualification.

pub mod boundary;
pub mod btleplug_backend;
pub mod capabilities;
pub mod central;
pub mod errors;
pub mod executor;

pub use boundary::{
    CharacteristicSnapshot, DescriptorKey, DescriptorSnapshot, FakeRadio, FaultOp, InstanceKey,
    ManufacturerData, PeerSnapshot, PropertyFlags, RadioBoundary, RadioCloseFailure, RadioEvent,
    ScanFilterSpec, ServiceData, ServiceSnapshot,
};
pub use btleplug_backend::BtleplugRadio;
pub use capabilities::{
    CapabilityVerdict, DESKTOP_CAPABILITIES, DesktopCapability, register_desktop_capabilities,
};
pub use central::{
    ConnectionHandle, DesktopCentral, DiscoveredPath, DiscoveryReport, NotificationPoll,
    ScanSession, ShutdownReport,
};
pub use errors::DesktopError;
pub use ubm_core::central::{CompletionOutcome, PathSelector};
pub use ubm_core::contracts::OperationId;
