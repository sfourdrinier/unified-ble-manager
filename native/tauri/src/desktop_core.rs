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

use tokio::sync::broadcast;
use ubm_core::contracts::{AttachmentTuple, OperationId};
use ubm_desktop::{
    AdapterAuthorization, AdapterPowerState, AdapterStatus, CancelAck, ConnectionHandle,
    DeliveryMode, DesktopCentral, DesktopError, DiscoveredPath, DiscoveryReport, LifecycleEvent,
    LinkRelease, NotificationPoll, ObservedDelivery, OpControl, OpTicket, PathSelector,
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
    /// Release the link held under `lease`. A failed release keeps it.
    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, LinkRelease>;
    /// Run discovery (partial-failure report).
    fn discover<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, DiscoveryReport>;
    /// Read the whole current discovery tree for one peer.
    fn discovered_paths<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Vec<DiscoveredPath>>;
    /// Characteristic read.
    fn read<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        ctl: OpControl,
    ) -> CoreFuture<'a, Vec<u8>>;
    /// Characteristic write (`mode`: `with-response` / `without-response`).
    fn write<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        mode: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, ()>;
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
    /// Cancel the operation behind `ticket` (before or after admission).
    fn cancel<'a>(&'a self, ticket: &'a OpTicket) -> CoreFuture<'a, CancelAck>;
    /// Subscribe to connection-lifecycle events.
    fn lifecycle_events(&self) -> broadcast::Receiver<LifecycleEvent>;
    /// Subscribe to scans the core ended without a stop request (the OS
    /// stopped it, or an adapter loss took it).
    fn scan_terminal_events(&self) -> broadcast::Receiver<ScanTerminalEvent>;
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

    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        ctl: OpControl,
    ) -> CoreFuture<'a, LinkRelease> {
        Box::pin(DesktopCentral::disconnect(self, peer_id, lease, ctl))
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
    ) -> CoreFuture<'a, Vec<u8>> {
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

    fn cancel<'a>(&'a self, ticket: &'a OpTicket) -> CoreFuture<'a, CancelAck> {
        Box::pin(DesktopCentral::cancel(self, ticket))
    }

    fn lifecycle_events(&self) -> broadcast::Receiver<LifecycleEvent> {
        DesktopCentral::lifecycle_events(self)
    }

    fn scan_terminal_events(&self) -> broadcast::Receiver<ScanTerminalEvent> {
        DesktopCentral::scan_terminal_events(self)
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
