//! Tauri scheduling authority: every BLE op schedules through one shared
//! `ubm-desktop` [`DesktopCentral`].
//!
//! The dispatcher holds `Arc<dyn CoreAuthority>`; production implements it
//! with a [`DesktopCentral`] over the btleplug radio, tests with a
//! [`DesktopCentral`] over a scripted boundary. [`DesktopCentral`] is an
//! `Arc` over internally locked state whose methods take `&self`, so the
//! dispatcher clones the handle and never serializes BLE work behind a lock
//! of its own (PR210-04): one peer's slow connect or discovery cannot stall
//! another peer's notifications, a cancel, or shutdown.
//!
//! Every operation carries an [`OpControl`]: the caller's budget, admitted
//! on the plugin's own clock from the relative `budgetMs` the webview sent
//! (PR210-06), and the ticket that receives the core operation id at
//! admission so a cancel targets exactly that operation, even one that
//! arrives before the id exists (PR210-05). The core owns every outcome:
//! this module holds no scan policy, no subscription state, no retry logic
//! and no timers.
//!
//! Contract error identities pass through verbatim: methods return
//! [`DesktopError`] unchanged (code, domain, operation, detail, commit state
//! and retryability), and the IPC layer renders them without substitution.
//! A missing radio fails loudly with `adapter.unavailable`, never silently.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;
use ubm_core::contracts::{AttachmentTuple, OperationId};
use ubm_desktop::{
    AdapterAuthorization, AdapterPowerState, AdapterResetEvent, AdapterStatus, CancelAck,
    CharacteristicRead, ConnectionHandle, ConnectionParametersEvent, DeliveryMode, DesktopCentral,
    DesktopError, DiscoveredPath, DiscoveryReport, LifecycleEvent, NotificationPoll,
    ObservedConnectionParameters, ObservedDelivery, OpControl, OpTicket, PathSelector,
    PeerSnapshot, RadioBoundary, ScanStop, ScanTerminalEvent, ShutdownReport,
};

/// GATT path selector parts (UUIDs plus optional duplicate occurrences).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreSelector {
    /// Canonical service UUID.
    pub service_uuid: String,
    /// Service occurrence among duplicate UUIDs.
    pub service_occurrence: Option<u64>,
    /// Characteristic UUID (`None` = service-level path).
    pub characteristic_uuid: Option<String>,
    /// Characteristic occurrence among duplicate UUIDs.
    pub characteristic_occurrence: Option<u64>,
    /// Descriptor UUID (`None` = characteristic-level path).
    pub descriptor_uuid: Option<String>,
    /// Descriptor occurrence among duplicate UUIDs.
    pub descriptor_occurrence: Option<u64>,
}

impl CoreSelector {
    fn path<B: RadioBoundary>(&self) -> Result<PathSelector, DesktopError> {
        DesktopCentral::<B>::selector(
            &self.service_uuid,
            self.service_occurrence,
            self.characteristic_uuid.as_deref(),
            self.characteristic_occurrence,
            self.descriptor_uuid.as_deref(),
            self.descriptor_occurrence,
        )
    }
}

/// Render a [`DesktopError`] identity for IPC failures without substitution:
/// `code`, `domain`, and `operation` cross verbatim.
pub fn error_identity(error: &DesktopError) -> (&'static str, &'static str, String, String) {
    (
        error.code_str(),
        error.domain().as_str(),
        error.operation().to_owned(),
        error.detail().unwrap_or("").to_owned(),
    )
}

/// One boxed core future behind the authority trait.
pub type CoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DesktopError>> + Send + 'a>>;

/// Shared-core scheduling authority behind the Tauri IPC dispatcher.
///
/// Every BLE verdict the dispatcher serves comes through this trait. The
/// dispatcher holds `Arc<dyn CoreAuthority>` so the radio type never leaks
/// into IPC code, and it performs no BLE scheduling of its own. Every
/// method is a direct delegation to the shared [`DesktopCentral`]; none of
/// them takes a lock across the radio call.
pub trait CoreAuthority: Send + Sync {
    fn directory_os(&self) -> ubm_desktop::DesktopOs;
    fn known_peers(&self, ctl: OpControl) -> CoreFuture<'_, Vec<ubm_desktop::DirectoryPeer>>;
    /// Reserve before an IPC worker starts. Scripted authorities may have
    /// their own admission; the production DesktopCentral owns the FIFO.
    fn bind_gatt_admission(
        &self,
        _peer_id: &str,
        ctl: OpControl,
    ) -> Result<OpControl, DesktopError> {
        Ok(ctl)
    }
    fn connection_parameter_source_failure<'a>(
        &'a self,
        _peer: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<DesktopError>> + Send + 'a>> {
        Box::pin(async { None })
    }

    fn request_priority<'a>(
        &'a self,
        peer: &'a str,
        lease: &'a str,
        priority: ubm_desktop::boundary::ConnectionPriority,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool> {
        let _ = (peer, lease, priority, ctl);
        Box::pin(async {
            Err(ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::CapabilityUnsupported,
                ubm_core::contracts::BleErrorDomain::Capability,
                "connection.request-priority",
            ))
        })
    }

    fn start_scan_platform<'a>(
        &'a self,
        owner: &'a str,
        services: &'a [String],
        windows: Option<ubm_desktop::boundary::WindowsScanOptions>,
        ctl: OpControl,
    ) -> CoreFuture<'a, OperationId> {
        if windows.is_none() {
            return self.start_scan(owner, services, ctl);
        }
        Box::pin(async {
            Err(ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::CapabilityUnsupported,
                ubm_core::contracts::BleErrorDomain::Capability,
                "scan.platform-options",
            ))
        })
    }
    /// Security facts and ceremonies share this exact core/cancellation authority.
    fn security_state<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::SecurityState>;
    fn pair<'a>(
        &'a self,
        peer: &'a str,
        request: ubm_desktop::PairRequest,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::PairOutcome>;
    fn cancel_pairing<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::CancelPairingOutcome>;
    fn unpair<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::UnpairOutcome>;
    fn security_events(&self) -> broadcast::Receiver<ubm_desktop::SecurityEvent>;
    /// The actual OS address type, never inferred from the address bytes.
    fn address_type<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, Option<ubm_desktop::AddressType>>;
    fn resolve_address<'a>(
        &'a self,
        address: &'a str,
        address_type: ubm_desktop::AddressType,
        ctl: OpControl,
    ) -> CoreFuture<'a, String>;
    /// Capability descriptors from this instantiated central, including refusal reasons.
    fn capability_descriptors(
        &self,
    ) -> CoreFuture<'_, Vec<ubm_core::central::CapabilityDescriptor>>;
    /// Read the OS directory without acquiring a connection lease.
    fn connected_peers<'a>(
        &'a self,
        services: &'a [String],
        ctl: OpControl,
    ) -> CoreFuture<'a, Vec<ubm_desktop::DirectoryPeer>>;
    /// Resolve an app-held OS identifier without connecting.
    fn bonded_peers(&self, ctl: OpControl) -> CoreFuture<'_, Vec<ubm_desktop::DirectoryPeer>>;
    fn resolve_peer<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, Option<ubm_desktop::DirectoryPeer>>;
    /// Trusted-host native continuation, over this exact central/lease authority.
    fn native_continuation(
        &self,
        runtime: tokio::runtime::Handle,
        recordings: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    ) -> ubm_desktop::continuation_adapter::DesktopContinuation;
    /// Start a scan; returns the core scan operation id.
    fn start_scan<'a>(
        &'a self,
        owner: &'a str,
        service_uuids: &'a [String],
        ctl: OpControl,
    ) -> CoreFuture<'a, OperationId>;
    /// Stop exactly the scan `scan` names ([`ScanStop::NotActive`] for any
    /// other id, with no radio call). A failed stop keeps the scan owned.
    fn stop_scan<'a>(&'a self, scan: &'a OperationId, ctl: OpControl) -> CoreFuture<'a, ScanStop>;
    /// Take one queued advertisement (`None` = none queued now).
    fn take_advertisement(&self) -> CoreFuture<'_, Option<PeerSnapshot>>;
    /// Connect; the lease is the exact string later ops echo back.
    fn connect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ConnectionHandle>;
    fn connect_when_available<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ConnectionHandle>;
    /// Release this caller's lease, preserving other link owners. False means
    /// the shared physical link remains; true means it ended/already ended.
    /// A failed release keeps its cleanup ownership.
    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool>;
    /// Run discovery (partial-failure report).
    fn discover<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, DiscoveryReport>;
    /// Read the whole current discovery tree for one peer.
    fn discovered_paths<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Vec<DiscoveredPath>>;
    /// Characteristic read: the value and the radio's own provenance.
    fn read<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        ctl: OpControl,
    ) -> CoreFuture<'a, CharacteristicRead>;
    /// Characteristic write (`mode`: `with-response` / `without-response`).
    fn write<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        mode: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()>;
    fn write_when_ready<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()> {
        let _ = (peer_id, selector, value, ctl);
        Box::pin(async {
            Err(ubm_desktop::DesktopError::new(
                ubm_core::contracts::BleErrorCode::CapabilityUnsupported,
                ubm_core::contracts::BleErrorDomain::Capability,
                "gatt.write-when-ready",
            ))
        })
    }
    fn acquire_gatt<'a>(
        &'a self,
        peer: &'a str,
        selector: &'a CoreSelector,
        kind: ubm_desktop::acquired_gatt::AcquisitionKind,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::AcquiredGattHandle> {
        let _ = (peer, selector, kind, ctl);
        Box::pin(async { Err(acquired_unsupported()) })
    }
    fn acquired_write<'a>(
        &'a self,
        handle: &'a str,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()> {
        let _ = (handle, value, ctl);
        Box::pin(async { Err(acquired_unsupported()) })
    }
    fn acquired_receive<'a>(&'a self, handle: &'a str, ctl: OpControl) -> CoreFuture<'a, Vec<u8>> {
        let _ = (handle, ctl);
        Box::pin(async { Err(acquired_unsupported()) })
    }
    fn close_acquired<'a>(&'a self, handle: &'a str, ctl: OpControl) -> CoreFuture<'a, ()> {
        let _ = (handle, ctl);
        Box::pin(async { Err(acquired_unsupported()) })
    }
    /// Descriptor read.
    fn read_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        ctl: OpControl,
    ) -> CoreFuture<'a, Vec<u8>>;
    /// Descriptor write.
    fn write_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()>;
    /// Subscribe one consumer, carrying a hard delivery requirement to the
    /// radio; answers the delivery the radio reported.
    fn subscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
        delivery: Option<DeliveryMode>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ObservedDelivery>;
    /// Poll one consumer's notification stream with a typed outcome.
    fn poll_notification<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
    ) -> CoreFuture<'a, NotificationPoll>;
    /// Remove one consumer (the last one disables the CCCD).
    fn unsubscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool>;
    /// Connected RSSI of the link held under `lease`.
    fn read_rssi<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, i16>;
    /// Effective ATT MTU of the link held under `lease`, when the OS
    /// observed one. `None` is unobserved, not a link failure.
    fn read_effective_mtu<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, Option<u16>>;
    /// The largest single write the OS accepts on the link held under
    /// `lease`, for one write mode (the same limit a write of that mode is
    /// admitted against).
    fn connection_maximum_write_length<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        with_response: bool,
        ctl: OpControl,
    ) -> CoreFuture<'a, u64>;
    /// Whether the lease's link can take a write without response now.
    fn write_readiness<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool>;
    fn write_readiness_events(&self) -> broadcast::Receiver<ubm_desktop::WriteReadinessEvent>;
    /// Observed connection parameters for the lease holding the link.
    /// Interval and supervision timeout are microseconds.
    fn connection_parameters<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ObservedConnectionParameters>;
    fn connection_parameter_events(&self) -> broadcast::Receiver<ConnectionParametersEvent>;
    /// Cancel the operation behind `ticket` (before or after admission).
    fn cancel<'a>(&'a self, ticket: &'a OpTicket) -> CoreFuture<'a, CancelAck>;
    /// Subscribe to connection-lifecycle events.
    fn lifecycle_events(&self) -> broadcast::Receiver<LifecycleEvent>;
    /// Subscribe to scans the core ended without a stop request (the OS
    /// stopped it, or an adapter loss took it).
    fn scan_terminal_events(&self) -> broadcast::Receiver<ScanTerminalEvent>;
    /// Subscribe to adapter resets: each one replaced the attachment the
    /// dispatcher's callers are bound to (IPC protocol 4 rebind).
    fn adapter_reset_events(&self) -> broadcast::Receiver<AdapterResetEvent>;
    /// Shut the central down (idempotent) and return its authoritative
    /// report.
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ShutdownReport> + Send + '_>>;
    /// The central's current attachment scope: minted at open, replaced by
    /// every adapter reset (finding 57). The one attachment identity the
    /// plugin reports; it opens no radio of its own for it (finding 43).
    fn attachment(&self) -> AttachmentTuple;
    /// The adapter facts admission reads, as the radio last reported them.
    fn adapter_status(&self) -> AdapterStatus;
    /// Adapter power state read from the radio under the budget.
    fn adapter_state(&self, ctl: OpControl) -> CoreFuture<'_, AdapterPowerState>;
    /// Whether the OS lets this process use the adapter, under the budget.
    fn adapter_authorization(&self, ctl: OpControl) -> CoreFuture<'_, AdapterAuthorization>;
    /// Radio adapter label through the core boundary (attachment identity).
    fn adapter_name(&self) -> CoreFuture<'_, String>;
    /// Currently visible radio peers through the core boundary (heard facts).
    fn peers(&self) -> CoreFuture<'_, Vec<PeerSnapshot>>;
}

impl<B: RadioBoundary> CoreAuthority for DesktopCentral<B> {
    fn request_priority<'a>(
        &'a self,
        peer: &'a str,
        lease: &'a str,
        priority: ubm_desktop::boundary::ConnectionPriority,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool> {
        Box::pin(DesktopCentral::request_priority(
            self, peer, lease, priority, ctl,
        ))
    }

    fn start_scan_platform<'a>(
        &'a self,
        owner: &'a str,
        services: &'a [String],
        windows: Option<ubm_desktop::boundary::WindowsScanOptions>,
        ctl: OpControl,
    ) -> CoreFuture<'a, OperationId> {
        Box::pin(async move {
            let refs: Vec<&str> = services.iter().map(String::as_str).collect();
            let session = DesktopCentral::start_scan_platform(
                self,
                owner,
                &refs,
                ubm_core::central::ScanDuplicatePolicy::All,
                None,
                windows,
                ctl,
            )
            .await?;
            Ok(session.operation_id().clone())
        })
    }
    fn security_state<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::SecurityState> {
        Box::pin(DesktopCentral::security_state(self, peer, ctl))
    }
    fn pair<'a>(
        &'a self,
        peer: &'a str,
        request: ubm_desktop::PairRequest,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::PairOutcome> {
        Box::pin(DesktopCentral::pair(self, peer, request, ctl))
    }
    fn cancel_pairing<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::CancelPairingOutcome> {
        Box::pin(DesktopCentral::cancel_pairing(self, peer, ctl))
    }
    fn unpair<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::UnpairOutcome> {
        Box::pin(DesktopCentral::unpair(self, peer, ctl))
    }
    fn security_events(&self) -> broadcast::Receiver<ubm_desktop::SecurityEvent> {
        DesktopCentral::security_events(self)
    }
    fn address_type<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, Option<ubm_desktop::AddressType>> {
        Box::pin(DesktopCentral::address_type(self, peer, ctl))
    }
    fn resolve_address<'a>(
        &'a self,
        address: &'a str,
        address_type: ubm_desktop::AddressType,
        ctl: OpControl,
    ) -> CoreFuture<'a, String> {
        Box::pin(DesktopCentral::resolve_address(
            self,
            address,
            address_type,
            ctl,
        ))
    }
    fn capability_descriptors(
        &self,
    ) -> CoreFuture<'_, Vec<ubm_core::central::CapabilityDescriptor>> {
        Box::pin(async move { Ok(DesktopCentral::capability_descriptors(self).await) })
    }
    fn directory_os(&self) -> ubm_desktop::DesktopOs {
        DesktopCentral::directory_os(self)
    }
    fn known_peers(&self, ctl: OpControl) -> CoreFuture<'_, Vec<ubm_desktop::DirectoryPeer>> {
        Box::pin(DesktopCentral::known_directory_peers(self, ctl))
    }
    fn connected_peers<'a>(
        &'a self,
        services: &'a [String],
        ctl: OpControl,
    ) -> CoreFuture<'a, Vec<ubm_desktop::DirectoryPeer>> {
        Box::pin(DesktopCentral::connected_peers(self, services, ctl))
    }
    fn bonded_peers(&self, ctl: OpControl) -> CoreFuture<'_, Vec<ubm_desktop::DirectoryPeer>> {
        Box::pin(DesktopCentral::bonded_peers(self, ctl))
    }
    fn resolve_peer<'a>(
        &'a self,
        peer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, Option<ubm_desktop::DirectoryPeer>> {
        Box::pin(DesktopCentral::resolve_peer(self, peer, ctl))
    }
    fn native_continuation(
        &self,
        runtime: tokio::runtime::Handle,
        recordings: Arc<ubm_desktop::continuation_journal::JournalRegistry>,
    ) -> ubm_desktop::continuation_adapter::DesktopContinuation {
        ubm_desktop::continuation_adapter::DesktopContinuation::new_with_recording_registry(
            self.clone(),
            runtime,
            recordings,
        )
    }
    fn start_scan<'a>(
        &'a self,
        owner: &'a str,
        service_uuids: &'a [String],
        ctl: OpControl,
    ) -> CoreFuture<'a, OperationId> {
        Box::pin(async move {
            let refs: Vec<&str> = service_uuids.iter().map(String::as_str).collect();
            let session = DesktopCentral::start_scan(self, owner, &refs, ctl).await?;
            Ok(session.operation_id().clone())
        })
    }

    fn stop_scan<'a>(&'a self, scan: &'a OperationId, ctl: OpControl) -> CoreFuture<'a, ScanStop> {
        Box::pin(DesktopCentral::stop_scan(self, scan, ctl))
    }

    fn take_advertisement(&self) -> CoreFuture<'_, Option<PeerSnapshot>> {
        Box::pin(async move { Ok(DesktopCentral::take_advertisement(self).await) })
    }

    fn connect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ConnectionHandle> {
        Box::pin(DesktopCentral::connect(self, peer_id, lease, ctl))
    }

    fn connect_when_available<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ConnectionHandle> {
        Box::pin(DesktopCentral::connect_when_available(
            self, peer_id, lease, ctl,
        ))
    }

    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool> {
        Box::pin(DesktopCentral::release_connection_lease(
            self, peer_id, lease, ctl,
        ))
    }

    fn discover<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, DiscoveryReport> {
        Box::pin(DesktopCentral::discover(self, peer_id, lease, ctl))
    }

    fn discovered_paths<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Vec<DiscoveredPath>> {
        Box::pin(DesktopCentral::discovered_paths(self, peer_id))
    }

    fn read<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        ctl: OpControl,
    ) -> CoreFuture<'a, CharacteristicRead> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::read(self, peer_id, &path, ctl).await
        })
    }

    fn write<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        mode: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::write(self, peer_id, &path, value, mode, ctl).await
        })
    }

    fn bind_gatt_admission(
        &self,
        peer_id: &str,
        ctl: OpControl,
    ) -> Result<OpControl, DesktopError> {
        DesktopCentral::bind_gatt_admission(self, peer_id, ctl)
    }
    fn connection_parameter_source_failure<'a>(
        &'a self,
        peer: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<DesktopError>> + Send + 'a>> {
        Box::pin(DesktopCentral::connection_parameter_source_failure(
            self, peer,
        ))
    }

    fn write_when_ready<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::write_when_ready(self, peer_id, &path, value, ctl).await
        })
    }

    fn read_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        ctl: OpControl,
    ) -> CoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::read_descriptor(self, peer_id, &path, ctl).await
        })
    }

    fn acquire_gatt<'a>(
        &'a self,
        peer: &'a str,
        selector: &'a CoreSelector,
        kind: ubm_desktop::acquired_gatt::AcquisitionKind,
        ctl: OpControl,
    ) -> CoreFuture<'a, ubm_desktop::AcquiredGattHandle> {
        Box::pin(async move {
            DesktopCentral::acquire_gatt(self, peer, &selector.path::<B>()?, kind, ctl).await
        })
    }
    fn acquired_write<'a>(
        &'a self,
        handle: &'a str,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()> {
        Box::pin(DesktopCentral::acquired_write(self, handle, value, ctl))
    }
    fn acquired_receive<'a>(&'a self, handle: &'a str, ctl: OpControl) -> CoreFuture<'a, Vec<u8>> {
        Box::pin(DesktopCentral::acquired_receive(self, handle, ctl))
    }
    fn close_acquired<'a>(&'a self, handle: &'a str, ctl: OpControl) -> CoreFuture<'a, ()> {
        Box::pin(DesktopCentral::close_acquired(self, handle, ctl))
    }

    fn write_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::write_descriptor(self, peer_id, &path, value, ctl).await
        })
    }

    fn subscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
        delivery: Option<DeliveryMode>,
        ctl: OpControl,
    ) -> CoreFuture<'a, ObservedDelivery> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::subscribe(self, peer_id, &path, consumer, delivery, ctl).await
        })
    }

    fn poll_notification<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
    ) -> CoreFuture<'a, NotificationPoll> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::poll_notification(self, peer_id, &path, consumer).await
        })
    }

    fn unsubscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool> {
        Box::pin(async move {
            let path = selector.path::<B>()?;
            DesktopCentral::unsubscribe(self, peer_id, &path, consumer, ctl).await
        })
    }

    fn read_rssi<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, i16> {
        Box::pin(DesktopCentral::read_rssi(self, peer_id, lease, ctl))
    }

    fn read_effective_mtu<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, Option<u16>> {
        Box::pin(DesktopCentral::read_effective_mtu(
            self, peer_id, lease, ctl,
        ))
    }

    fn connection_maximum_write_length<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        with_response: bool,
        ctl: OpControl,
    ) -> CoreFuture<'a, u64> {
        Box::pin(DesktopCentral::connection_maximum_write_length(
            self,
            peer_id,
            lease,
            with_response,
            ctl,
        ))
    }

    fn write_readiness<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, bool> {
        Box::pin(DesktopCentral::write_readiness(self, peer_id, lease, ctl))
    }

    fn write_readiness_events(&self) -> broadcast::Receiver<ubm_desktop::WriteReadinessEvent> {
        DesktopCentral::write_readiness_events(self)
    }

    fn connection_parameters<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ObservedConnectionParameters> {
        Box::pin(DesktopCentral::connection_parameters(
            self, peer_id, lease, ctl,
        ))
    }

    fn connection_parameter_events(&self) -> broadcast::Receiver<ConnectionParametersEvent> {
        DesktopCentral::connection_parameter_events(self)
    }

    fn cancel<'a>(&'a self, ticket: &'a OpTicket) -> CoreFuture<'a, CancelAck> {
        Box::pin(DesktopCentral::cancel(self, ticket))
    }

    fn lifecycle_events(&self) -> broadcast::Receiver<LifecycleEvent> {
        DesktopCentral::lifecycle_events(self)
    }

    fn scan_terminal_events(&self) -> broadcast::Receiver<ScanTerminalEvent> {
        DesktopCentral::scan_terminal_events(self)
    }

    fn adapter_reset_events(&self) -> broadcast::Receiver<AdapterResetEvent> {
        DesktopCentral::adapter_reset_events(self)
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ShutdownReport> + Send + '_>> {
        Box::pin(DesktopCentral::shutdown(self))
    }

    fn attachment(&self) -> AttachmentTuple {
        DesktopCentral::attachment(self)
    }

    fn adapter_status(&self) -> AdapterStatus {
        DesktopCentral::adapter_status(self)
    }

    fn adapter_state(&self, ctl: OpControl) -> CoreFuture<'_, AdapterPowerState> {
        Box::pin(DesktopCentral::adapter_state(self, ctl))
    }

    fn adapter_authorization(&self, ctl: OpControl) -> CoreFuture<'_, AdapterAuthorization> {
        Box::pin(DesktopCentral::adapter_authorization(self, ctl))
    }

    fn adapter_name(&self) -> CoreFuture<'_, String> {
        Box::pin(self.boundary().adapter_name())
    }

    fn peers(&self) -> CoreFuture<'_, Vec<PeerSnapshot>> {
        Box::pin(self.boundary().peers())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ubm_desktop::FakeRadio;

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";

    /// Opens the central on the shared desktop executor, as production does.
    async fn open_authority() -> Arc<dyn CoreAuthority> {
        let central = ubm_desktop::executor::desktop_runtime()
            .spawn(DesktopCentral::open(FakeRadio::new(), "tauri-test"))
            .await
            .expect("open task joins")
            .expect("fake radio opens without hardware");
        Arc::new(central)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_schedules_through_the_shared_core() {
        let core = open_authority().await;
        let operation = core
            .start_scan(
                "owner-a",
                &[HRM_SERVICE.to_owned()],
                OpControl::budget_ms(5000),
            )
            .await
            .expect("core admits scan");
        assert_eq!(
            core.stop_scan(&operation, OpControl::budget_ms(5000))
                .await
                .expect("core stops scan"),
            ScanStop::Stopped
        );
        assert_eq!(
            core.stop_scan(&operation, OpControl::budget_ms(5000))
                .await
                .expect("second stop"),
            ScanStop::NotActive,
            "a released scan id answers not-active without a radio call"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_op_slice_executes_ubm_desktop() {
        let core = open_authority().await;
        let operation = core
            .start_scan(
                "owner-a",
                &[HRM_SERVICE.to_owned()],
                OpControl::budget_ms(5000),
            )
            .await
            .expect("scan");
        core.stop_scan(&operation, OpControl::budget_ms(5000))
            .await
            .expect("stop");
        // A never-connected peer fails reads with the frozen core identity,
        // never an empty surprise.
        let selector = CoreSelector {
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: Some(0),
            characteristic_uuid: Some("00002a37-0000-1000-8000-00805f9b34fb".to_owned()),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        };
        let error = core
            .read("peer-unknown", &selector, OpControl::budget_ms(500))
            .await
            .expect_err("unknown peer must fail");
        let (code, domain, _, _) = error_identity(&error);
        assert_eq!(code, "peer.not-found");
        assert_eq!(domain, "connection");
        // Shutdown is idempotent; later ops refuse loudly.
        core.shutdown().await;
        core.shutdown().await;
        let error = core
            .start_scan(
                "owner-a",
                &[HRM_SERVICE.to_owned()],
                OpControl::budget_ms(100),
            )
            .await
            .expect_err("post-shutdown scan must fail");
        let (code, _, _, _) = error_identity(&error);
        assert_eq!(code, "adapter.unavailable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_selectors_fail_before_the_radio() {
        let core = open_authority().await;
        let bad = CoreSelector {
            service_uuid: "not-a-uuid".to_owned(),
            service_occurrence: None,
            characteristic_uuid: None,
            characteristic_occurrence: None,
            descriptor_uuid: None,
            descriptor_occurrence: None,
        };
        let error = core
            .read("peer-1", &bad, OpControl::budget_ms(500))
            .await
            .expect_err("malformed UUID must fail");
        let (code, domain, _, _) = error_identity(&error);
        assert_eq!(domain, "core");
        assert!(
            code == "bytes.invalid" || code == "argument.invalid",
            "unexpected code {code}"
        );
    }

    #[test]
    fn error_identities_cross_verbatim() {
        let error = DesktopError::adapter_unavailable("desktop.open").with_detail("no adapter");
        let (code, domain, operation, detail) = error_identity(&error);
        assert_eq!(code, "adapter.unavailable");
        assert_eq!(domain, "adapter");
        assert_eq!(operation, "desktop.open");
        assert_eq!(detail, "no adapter");
    }
}

fn acquired_unsupported() -> DesktopError {
    DesktopError::new(
        ubm_core::contracts::BleErrorCode::CapabilityUnsupported,
        ubm_core::contracts::BleErrorDomain::Capability,
        "gatt.acquire",
    )
}
