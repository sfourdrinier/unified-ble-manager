//! F01 Tauri scheduling authority: BLE op scheduling executes `ubm-desktop`.
//!
//! [`DesktopCore`] owns one [`DesktopCentral`] over a caller-supplied radio
//! boundary: the production [`BtleplugRadio`] or a scripted boundary in
//! tests. Every BLE data-path op — scan, connect, discover, read, write,
//! subscribe, notifications, cancel, shutdown — schedules through
//! `ubm-desktop` / `ubm-core`. This module holds no scan policy, no
//! subscription state, no retry logic, and no timeout timers: deadlines
//! cross as `timeout_ms` args so the core owns every caller outcome.
//!
//! Contract error identities pass through verbatim: methods return
//! [`DesktopError`] unchanged, and the IPC layer renders its
//! `code`/`domain`/`operation` without substitution (see
//! [`error_identity`]). A missing radio (no adapter on headless CI) fails
//! loudly with `adapter.unavailable`, never silently.
//!
//! Status in the F01 factory-routing migration (PARTIAL, honest scope):
//! this authority is complete and proven (tests below drive the full op
//! slice through the real `DesktopCentral` over a scripted boundary), and
//! the plugin owns `ubm-desktop` as a real dependency (the executor seam in
//! `btleplug_dispatcher.rs` already resolves through it). The remaining
//! follow-up — cutting the IPC op methods in `btleplug_dispatcher.rs`
//! (scan start/stop, connect, disconnect, discover, read, write,
//! descriptor read/write, subscribe, unsubscribe, cancel, dispose) from raw
//! btleplug `Adapter`/`Peripheral` calls to this authority, then deleting
//! the parallel policy (`SCAN_POLL_INTERVAL` poll loop, `scan_plan.rs`
//! duplicate/merge derivation, per-op btleplug timeout handling) and
//! remodelling the radio-holding resources (`ScanResource.task` to core op
//! id, `ConnectionResource.peripheral` to peer key/lease, `DatabaseResource`
//! to core discovery paths, `SubscriptionBody::Native` to core consumer) —
//! is a state-machine remodel of that 6890-line file, deliberately left as
//! one coherent follow-up rather than churned halfway here. Routing half the
//! ops would create the very second scheduling authority F01 forbids, so the
//! dispatcher keeps single (if legacy) radio ownership until that cutover.

use std::future::Future;
use std::pin::Pin;

use ubm_core::contracts::OperationId;
use ubm_desktop::{
    ConnectionHandle, DesktopCentral, DesktopError, DiscoveredPath, DiscoveryReport, PeerSnapshot,
    RadioBoundary,
};

/// GATT path selector parts (UUIDs plus optional duplicate occurrences).
#[derive(Clone, Debug)]
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

/// Tauri scheduling authority over one shared-core central.
///
/// The central opens lazily on the first BLE op so dispatcher construction
/// never touches the radio; every method below is one thin delegation into
/// [`DesktopCentral`].
pub struct DesktopCore<B: RadioBoundary> {
    central: Option<DesktopCentral<B>>,
    pending_boundary: Option<B>,
    owner: String,
}

impl<B: RadioBoundary> DesktopCore<B> {
    /// Stage a radio boundary; the central opens on the first BLE op.
    /// `owner` labels the core attachment (host identity, e.g. `"tauri"`).
    pub fn new(boundary: B, owner: &str) -> Self {
        Self {
            central: None,
            pending_boundary: Some(boundary),
            owner: owner.to_owned(),
        }
    }

    /// The open central, opening it on first use. Radio failures (no
    /// adapter, withheld readout) surface verbatim — never synthesized.
    pub async fn ensure_open(&mut self) -> Result<&DesktopCentral<B>, DesktopError> {
        if self.central.is_none() {
            let boundary = self.pending_boundary.take().ok_or_else(|| {
                DesktopError::adapter_unavailable("desktop.open")
                    .with_detail("boundary already consumed")
            })?;
            let central = DesktopCentral::open(boundary, &self.owner).await?;
            self.central = Some(central);
        }
        self.central.as_ref().ok_or_else(|| {
            DesktopError::adapter_unavailable("desktop.open")
                .with_detail("central missing after open")
        })
    }

    /// True once the central is open (no radio effect: reports admission only).
    pub fn is_open(&self) -> bool {
        self.central.is_some()
    }

    /// Start a scan through the core: duplicate/merge/timeout policy is the
    /// core's, not the caller's. Returns the core scan operation id.
    pub async fn start_scan(
        &mut self,
        owner: &str,
        service_uuids: &[String],
        timeout_ms: u64,
    ) -> Result<String, DesktopError> {
        let refs: Vec<&str> = service_uuids.iter().map(String::as_str).collect();
        let central = self.ensure_open().await?;
        let session = central.start_scan(owner, &refs, timeout_ms).await?;
        Ok(session.operation_id().to_string())
    }

    /// Stop the owned scan (idempotent in the core).
    pub async fn stop_scan(&mut self) -> Result<(), DesktopError> {
        let central = self.ensure_open().await?;
        central.stop_scan().await
    }

    /// Take one queued advertisement from the core observation queue
    /// (`None` = none queued now; the caller paces delivery, the core owns
    /// admission/overflow).
    pub async fn take_advertisement(&self) -> Result<Option<PeerSnapshot>, DesktopError> {
        let central = self.central.as_ref().ok_or_else(|| {
            DesktopError::adapter_unavailable("desktop.scan").with_detail("scan before open")
        })?;
        Ok(central.take_advertisement().await)
    }

    /// Connect through the core (deadline-owned by the core).
    pub async fn connect(
        &mut self,
        peer_id: &str,
        lease: &str,
        timeout_ms: u64,
    ) -> Result<ConnectionHandle, DesktopError> {
        let central = self.ensure_open().await?;
        central.connect(peer_id, lease, timeout_ms).await
    }

    /// Disconnect through the core.
    pub async fn disconnect(&mut self, peer_id: &str, lease: &str) -> Result<(), DesktopError> {
        let central = self.ensure_open().await?;
        central.disconnect(peer_id, lease).await
    }

    /// Run discovery through the core (partial-failure report, never a
    /// silent short snapshot).
    pub async fn discover(
        &mut self,
        peer_id: &str,
        lease: &str,
    ) -> Result<DiscoveryReport, DesktopError> {
        let central = self.ensure_open().await?;
        central.discover(peer_id, lease).await
    }

    /// Read the whole current discovery tree for one peer.
    pub async fn discovered_paths(
        &self,
        peer_id: &str,
    ) -> Result<Vec<DiscoveredPath>, DesktopError> {
        let central = self.central.as_ref().ok_or_else(|| {
            DesktopError::adapter_unavailable("desktop.discover")
                .with_detail("discover before open")
        })?;
        central.discovered_paths(peer_id).await
    }

    fn selector(selector: &CoreSelector) -> Result<ubm_desktop::PathSelector, DesktopError> {
        DesktopCentral::<B>::selector(
            &selector.service_uuid,
            selector.service_occurrence,
            selector.characteristic_uuid.as_deref(),
            selector.characteristic_occurrence,
            selector.descriptor_uuid.as_deref(),
            selector.descriptor_occurrence,
        )
    }

    /// Read through the core (deadline-owned by the core).
    pub async fn read(
        &mut self,
        peer_id: &str,
        selector: &CoreSelector,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.ensure_open().await?;
        central.read(peer_id, &path, timeout_ms).await
    }

    /// Write through the core (`mode`: `with-response` / `without-response`;
    /// the core validates MTU and properties inside the op deadline).
    pub async fn write(
        &mut self,
        peer_id: &str,
        selector: &CoreSelector,
        value: Vec<u8>,
        mode: &str,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.ensure_open().await?;
        central.write(peer_id, &path, value, mode, timeout_ms).await
    }

    /// Descriptor read through the core.
    pub async fn read_descriptor(
        &mut self,
        peer_id: &str,
        selector: &CoreSelector,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.ensure_open().await?;
        central.read_descriptor(peer_id, &path, timeout_ms).await
    }

    /// Descriptor write through the core.
    pub async fn write_descriptor(
        &mut self,
        peer_id: &str,
        selector: &CoreSelector,
        value: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.ensure_open().await?;
        central
            .write_descriptor(peer_id, &path, value, timeout_ms)
            .await
    }

    /// Subscribe through the core (enablement is core-arbitrated: concurrent
    /// subscribers share one physical enable).
    pub async fn subscribe(
        &mut self,
        peer_id: &str,
        selector: &CoreSelector,
        consumer: &str,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.ensure_open().await?;
        central
            .subscribe(peer_id, &path, consumer, timeout_ms)
            .await
    }

    /// Take one queued notification for a subscribed consumer.
    pub async fn take_notification(
        &self,
        peer_id: &str,
        selector: &CoreSelector,
        consumer: &str,
    ) -> Result<Option<Vec<u8>>, DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.central.as_ref().ok_or_else(|| {
            DesktopError::adapter_unavailable("desktop.subscribe")
                .with_detail("subscribe before open")
        })?;
        central.take_notification(peer_id, &path, consumer).await
    }

    /// Unsubscribe through the core (last consumer disables the physical CCCD).
    pub async fn unsubscribe(
        &mut self,
        peer_id: &str,
        selector: &CoreSelector,
        consumer: &str,
    ) -> Result<bool, DesktopError> {
        let path = Self::selector(selector)?;
        let central = self.ensure_open().await?;
        central.unsubscribe(peer_id, &path, consumer).await
    }

    /// Cancel one core operation by id (caller completion stays core-owned:
    /// cancellation requests best-effort physical halt, the settled core
    /// outcome is still the caller result).
    pub async fn cancel_operation(
        &mut self,
        operation_id: &str,
    ) -> Result<ubm_desktop::CompletionOutcome, DesktopError> {
        let operation = OperationId::new(operation_id).map_err(DesktopError::from)?;
        let central = self.ensure_open().await?;
        central.cancel_operation(&operation).await
    }

    /// Shut the central down (idempotent; other centrals are unaffected —
    /// executor shutdown stays an explicit process-owner step).
    pub async fn shutdown(&mut self) {
        if let Some(central) = self.central.take() {
            central.shutdown().await;
        }
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
/// Every BLE verdict the dispatcher serves comes through this trait: the
/// production implementation is [`DesktopCore`] over the btleplug radio, and
/// tests inject [`DesktopCore`] over a scripted boundary through the same
/// object. The dispatcher holds `Arc<dyn CoreAuthority>` so the radio type
/// never leaks into IPC code, and it performs no BLE scheduling of its own —
/// no scan policy, no retry, no timeout timers, no ownership generations.
pub trait CoreAuthority: Send + Sync {
    /// Start a scan through the core; returns the core scan operation id.
    fn start_scan<'a>(
        &'a self,
        owner: &'a str,
        service_uuids: &'a [String],
        timeout_ms: u64,
    ) -> CoreFuture<'a, String>;
    /// Stop the owned scan (idempotent in the core).
    fn stop_scan(&self) -> CoreFuture<'_, ()>;
    /// Take one queued advertisement (`None` = none queued now).
    fn take_advertisement(&self) -> CoreFuture<'_, Option<PeerSnapshot>>;
    /// Connect through the core (deadline-owned by the core).
    fn connect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ConnectionHandle>;
    /// Disconnect through the core.
    fn disconnect<'a>(&'a self, peer_id: &'a str, lease: &'a str) -> CoreFuture<'a, ()>;
    /// Run discovery through the core (partial-failure report).
    fn discover<'a>(&'a self, peer_id: &'a str, lease: &'a str) -> CoreFuture<'a, DiscoveryReport>;
    /// Read the whole current discovery tree for one peer.
    fn discovered_paths<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Vec<DiscoveredPath>>;
    /// Read through the core (deadline-owned by the core).
    fn read<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        timeout_ms: u64,
    ) -> CoreFuture<'a, Vec<u8>>;
    /// Write through the core (`mode`: `with-response` / `without-response`).
    fn write<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        mode: &'a str,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ()>;
    /// Descriptor read through the core.
    fn read_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        timeout_ms: u64,
    ) -> CoreFuture<'a, Vec<u8>>;
    /// Descriptor write through the core.
    fn write_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ()>;
    /// Subscribe through the core (enablement is core-arbitrated).
    fn subscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ()>;
    /// Take one queued notification for a subscribed consumer.
    fn take_notification<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
    ) -> CoreFuture<'a, Option<Vec<u8>>>;
    /// Unsubscribe through the core (last consumer disables the CCCD).
    fn unsubscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
    ) -> CoreFuture<'a, bool>;
    /// Cancel one core operation by id (the settled core outcome is still
    /// the caller result).
    fn cancel_operation<'a>(
        &'a self,
        operation_id: &'a str,
    ) -> CoreFuture<'a, ubm_desktop::CompletionOutcome>;
    /// Shut the central down (idempotent).
    fn shutdown(&self) -> CoreFuture<'_, ()>;
    /// Radio adapter label through the core boundary (attachment identity).
    fn adapter_name(&self) -> CoreFuture<'_, String>;
    /// Live ATT MTU for one connected peer through the core boundary
    /// (`None` = withheld by the OS; never synthesized).
    fn mtu<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Option<u16>>;
    /// Currently visible radio peers through the core boundary (heard facts).
    fn peers(&self) -> CoreFuture<'_, Vec<PeerSnapshot>>;
}

/// Shared dispatch over one [`DesktopCore`]: lock, open, delegate. The mutex
/// is the only dispatcher-side serialization; every verdict stays core-made.
impl<B: RadioBoundary> CoreAuthority for tokio::sync::Mutex<DesktopCore<B>> {
    fn start_scan<'a>(
        &'a self,
        owner: &'a str,
        service_uuids: &'a [String],
        timeout_ms: u64,
    ) -> CoreFuture<'a, String> {
        Box::pin(async move {
            self.lock()
                .await
                .start_scan(owner, service_uuids, timeout_ms)
                .await
        })
    }

    fn stop_scan(&self) -> CoreFuture<'_, ()> {
        Box::pin(async move { self.lock().await.stop_scan().await })
    }

    fn take_advertisement(&self) -> CoreFuture<'_, Option<PeerSnapshot>> {
        Box::pin(async move {
            let mut core = self.lock().await;
            core.ensure_open().await?;
            core.take_advertisement().await
        })
    }

    fn connect<'a>(
        &'a self,
        peer_id: &'a str,
        lease: &'a str,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ConnectionHandle> {
        Box::pin(async move { self.lock().await.connect(peer_id, lease, timeout_ms).await })
    }

    fn disconnect<'a>(&'a self, peer_id: &'a str, lease: &'a str) -> CoreFuture<'a, ()> {
        Box::pin(async move { self.lock().await.disconnect(peer_id, lease).await })
    }

    fn discover<'a>(&'a self, peer_id: &'a str, lease: &'a str) -> CoreFuture<'a, DiscoveryReport> {
        Box::pin(async move { self.lock().await.discover(peer_id, lease).await })
    }

    fn discovered_paths<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Vec<DiscoveredPath>> {
        Box::pin(async move { self.lock().await.discovered_paths(peer_id).await })
    }

    fn read<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        timeout_ms: u64,
    ) -> CoreFuture<'a, Vec<u8>> {
        let owned = selector.clone();
        Box::pin(async move { self.lock().await.read(peer_id, &owned, timeout_ms).await })
    }

    fn write<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        mode: &'a str,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ()> {
        let owned = selector.clone();
        Box::pin(async move {
            self.lock()
                .await
                .write(peer_id, &owned, value, mode, timeout_ms)
                .await
        })
    }

    fn read_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        timeout_ms: u64,
    ) -> CoreFuture<'a, Vec<u8>> {
        let owned = selector.clone();
        Box::pin(async move {
            self.lock()
                .await
                .read_descriptor(peer_id, &owned, timeout_ms)
                .await
        })
    }

    fn write_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        value: Vec<u8>,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ()> {
        let owned = selector.clone();
        Box::pin(async move {
            self.lock()
                .await
                .write_descriptor(peer_id, &owned, value, timeout_ms)
                .await
        })
    }

    fn subscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
        timeout_ms: u64,
    ) -> CoreFuture<'a, ()> {
        let owned = selector.clone();
        Box::pin(async move {
            self.lock()
                .await
                .subscribe(peer_id, &owned, consumer, timeout_ms)
                .await
        })
    }

    fn take_notification<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
    ) -> CoreFuture<'a, Option<Vec<u8>>> {
        let owned = selector.clone();
        Box::pin(async move {
            self.lock()
                .await
                .take_notification(peer_id, &owned, consumer)
                .await
        })
    }

    fn unsubscribe<'a>(
        &'a self,
        peer_id: &'a str,
        selector: &'a CoreSelector,
        consumer: &'a str,
    ) -> CoreFuture<'a, bool> {
        let owned = selector.clone();
        Box::pin(async move {
            self.lock()
                .await
                .unsubscribe(peer_id, &owned, consumer)
                .await
        })
    }

    fn cancel_operation<'a>(
        &'a self,
        operation_id: &'a str,
    ) -> CoreFuture<'a, ubm_desktop::CompletionOutcome> {
        Box::pin(async move { self.lock().await.cancel_operation(operation_id).await })
    }

    fn shutdown(&self) -> CoreFuture<'_, ()> {
        Box::pin(async move {
            self.lock().await.shutdown().await;
            Ok(())
        })
    }

    fn adapter_name(&self) -> CoreFuture<'_, String> {
        Box::pin(async move {
            let mut core = self.lock().await;
            let central = core.ensure_open().await?;
            central.boundary().adapter_name().await
        })
    }

    fn mtu<'a>(&'a self, peer_id: &'a str) -> CoreFuture<'a, Option<u16>> {
        Box::pin(async move {
            let mut core = self.lock().await;
            let central = core.ensure_open().await?;
            // The boundary reports an unmeasured MTU as `None` directly
            // (never an error): withhold, never synthesize.
            Ok(central.boundary().mtu(peer_id).await)
        })
    }

    fn peers(&self) -> CoreFuture<'_, Vec<PeerSnapshot>> {
        Box::pin(async move {
            let mut core = self.lock().await;
            let central = core.ensure_open().await?;
            central.boundary().peers().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_desktop::FakeRadio;

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";

    async fn open_core() -> DesktopCore<FakeRadio> {
        let executor = ubm_desktop::executor::desktop_runtime();
        let _guard = executor.enter();
        let mut core = DesktopCore::new(FakeRadio::new(), "tauri-test");
        core.ensure_open()
            .await
            .expect("fake radio opens without hardware");
        core
    }

    #[tokio::test]
    async fn scan_schedules_through_the_shared_core() {
        let mut core = open_core().await;
        let operation = core
            .start_scan("owner-a", &[HRM_SERVICE.to_owned()], 5000)
            .await
            .expect("core admits scan");
        assert!(!operation.is_empty(), "scan carries a core operation id");
        core.stop_scan().await.expect("core stops scan");
    }

    #[tokio::test]
    async fn full_op_slice_executes_ubm_desktop() {
        let mut core = open_core().await;
        core.start_scan("owner-a", &[HRM_SERVICE.to_owned()], 5000)
            .await
            .expect("scan");
        core.stop_scan().await.expect("stop");
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
            .read("peer-unknown", &selector, 500)
            .await
            .expect_err("unknown peer must fail");
        let (code, domain, _, _) = error_identity(&error);
        assert_eq!(code, "peer.not-found");
        assert_eq!(domain, "connection");
        // Shutdown is idempotent; later ops refuse loudly.
        core.shutdown().await;
        core.shutdown().await;
        let error = core
            .start_scan("owner-a", &[HRM_SERVICE.to_owned()], 100)
            .await
            .expect_err("post-shutdown scan must fail");
        let (code, _, _, _) = error_identity(&error);
        assert_eq!(code, "adapter.unavailable");
    }

    #[tokio::test]
    async fn malformed_selectors_fail_before_the_radio() {
        let mut core = open_core().await;
        let bad = CoreSelector {
            service_uuid: "not-a-uuid".to_owned(),
            service_occurrence: None,
            characteristic_uuid: None,
            characteristic_occurrence: None,
            descriptor_uuid: None,
            descriptor_occurrence: None,
        };
        let error = core
            .read("peer-1", &bad, 500)
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
