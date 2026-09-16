//! Host-neutral desktop central adapter (HOST-DESKTOP).
//!
//! [`DesktopCentral`] drives the real [`ubm_core::central::Central`] over a
//! [`RadioBoundary`](crate::boundary::RadioBoundary): validation, ownership,
//! generations, and subscription sharing stay in the core; this layer
//! translates radio outcomes into core settlements with contract error
//! identities. Scan cleanup, partial discovery failures, descriptor paths,
//! and cancellation are handled here, never in the radio backend.
//!
//! Execution: one shared desktop executor for the process
//! ([`crate::executor`]); this type never builds a runtime. All async work
//! must run on the shared handle. There is no BLE hardware on the
//! qualification host: physical proof stays queued (see `PARITY_GAPS.md`).

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, watch};
use ubm_core::central::{Central, PathSelector, canonical_uuid, validate_scan_request};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, BleErrorCode, BleErrorDomain, ContenderKind, Generation, OperationId,
};
use ubm_core::ownership::EffectBatch;

use crate::boundary::{InstanceKey, PeerSnapshot, RadioBoundary, RadioEvent, ScanFilterSpec};
use crate::errors::DesktopError;

/// Effect batch capacity per core call (matches the core's own default).
const EFFECT_BATCH_CAP: usize = 64;
/// Safety bound, not host policy: a btleplug disconnect that never resolves
/// (peripheral already dropped from the OS map) becomes a bounded,
/// classified outcome instead of hanging the caller.
const DISCONNECT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);
/// Default subscription stream bounds (items, bytes).
const DEFAULT_SUB_ITEM_CAP: u64 = 64;
const DEFAULT_SUB_BYTE_CAP: u64 = 8192;
/// ATT protocol ceiling for one write (ATT_MTU max 512 minus 3 bytes of
/// opcode/handle). A protocol constant, not a measurement: the OS-measured
/// MTU still gates every write through [`Central::maximum_write_length`].
const ATT_MAX_WRITE: u64 = 509;

static EPOCH: OnceLock<Instant> = OnceLock::new();
static OPEN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Monotonic milliseconds for core calls.
fn now_ms() -> u64 {
    let start = EPOCH.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn batch() -> EffectBatch {
    EffectBatch::new(EFFECT_BATCH_CAP)
}

fn contract_error(code: BleErrorCode, domain: BleErrorDomain, operation: &str) -> DesktopError {
    DesktopError::new(code, domain, operation)
}

/// Frozen wire string for a core error code (skipped-path reporting keeps
/// identities, not debug strings).
fn core_code_str(error: &ubm_core::contracts::CoreError) -> &'static str {
    error.code().as_str()
}

/// One live scan owned by this central (the core arbitrates one physical
/// scan; a second start fails with `scan.already-active`).
#[derive(Debug, Clone)]
pub struct ScanSession {
    id: OperationId,
}

impl ScanSession {
    /// Core operation id backing this scan.
    #[must_use]
    pub fn operation_id(&self) -> &OperationId {
        &self.id
    }
}

/// Handle for one connected peer.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    /// Session peer key in the core.
    pub peer_key: String,
    /// Connection generation minted by the core.
    pub connection_generation: Option<String>,
}

/// Partial-failure report for discovery: usable paths register, unusable
/// entries are skipped with their identities instead of failing the whole
/// snapshot.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    /// Paths registered (service, characteristic, and descriptor levels).
    pub paths_registered: usize,
    /// `(uuid, code)` for every skipped entry, in discovery order.
    pub skipped: Vec<(String, String)>,
}

struct ActiveScan {
    id: OperationId,
}

/// Build the per-instance routing key for a resolved selector. Levels the
/// selector leaves unspecified matched exactly one candidate during
/// `resolve_path` (otherwise resolution fails `gatt.ambiguous-path`), so
/// they are occurrence 0; specified levels carry the instance.
fn instance_key(peer_id: &str, selector: &PathSelector, characteristic: &str) -> InstanceKey {
    (
        peer_id.to_owned(),
        selector.service_uuid.clone(),
        selector.service_occurrence.unwrap_or(0),
        characteristic.to_owned(),
        selector.characteristic_occurrence.unwrap_or(0),
    )
}

struct Inner<B> {
    core: Mutex<Central>,
    boundary: B,
    scan: Mutex<Option<ActiveScan>>,
    /// Radio peripheral id -> core session peer key.
    peers: Mutex<HashMap<String, String>>,
    /// Per-instance subscription routing: (peer, service uuid, service
    /// occurrence, characteristic uuid, characteristic occurrence) ->
    /// core path. Duplicate UUIDs never share routing.
    subscriptions: Mutex<HashMap<InstanceKey, usize>>,
    /// Per-instance keys whose physical disable failed and is pending
    /// retry through `unsubscribe`. A pending key fails new subscribes
    /// closed until the disable completes.
    failed_disables: Mutex<HashSet<InstanceKey>>,
    shut_down: AtomicBool,
    /// Stop signal for the central-lifetime event loop.
    loop_stop: watch::Sender<bool>,
    /// Event-loop worker, joined at shutdown so no advertisement can race
    /// cleanup after the central is gone.
    loop_done: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Host-neutral desktop central: the real core over a mockable radio.
pub struct DesktopCentral<B> {
    inner: Arc<Inner<B>>,
}

/// Clone shares one central (one attachment scope, one scan owner).
impl<B> Clone for DesktopCentral<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<B: RadioBoundary> DesktopCentral<B> {
    /// Open a central over `boundary` with a fresh attachment scope.
    /// `owner` labels the attachment (host identity, e.g. `"node"`).
    ///
    /// Must be called on the shared desktop executor: the central-lifetime
    /// event loop spawns on the ambient runtime, and per-manager runtimes
    /// are forbidden.
    pub async fn open(boundary: B, owner: &str) -> Result<Self, DesktopError> {
        // L4: a shut-down executor admits no new centrals — a post-shutdown
        // open fails closed instead of building a zombie central whose ops
        // refuse admission.
        if crate::executor::is_desktop_runtime_shut_down() {
            return Err(contract_error(
                BleErrorCode::AdapterUnavailable,
                BleErrorDomain::Adapter,
                "desktop.open",
            ));
        }
        if owner.is_empty() {
            return Err(contract_error(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "desktop.owner",
            ));
        }
        // L6: never synthesize an adapter identity — a withheld readout
        // fails the open instead of labelling the adapter "unknown".
        let adapter_label = boundary.adapter_name().await?;
        let ordinal = OPEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let attachment = AttachmentTuple::new(
            AttachmentId::new(format!("desktop-attachment-{ordinal}"))
                .map_err(DesktopError::from)?,
            BackendInstanceId::new(format!("ubm-desktop-btleplug-{owner}"))
                .map_err(DesktopError::from)?,
            BackendGeneration::new(format!("desktop-backend-gen-{ordinal}"))
                .map_err(DesktopError::from)?,
            AdapterId::new(adapter_label).map_err(DesktopError::from)?,
            AdapterGeneration::new(format!("desktop-adapter-gen-{ordinal}"))
                .map_err(DesktopError::from)?,
        );
        let generation =
            Generation::new(format!("desktop-kernel-gen-{ordinal}")).map_err(DesktopError::from)?;
        let mut core = Central::new(
            attachment,
            generation,
            ubm_core::central::CentralConfig::default(),
        )
        .map_err(DesktopError::from)?;
        // M4: project desktop capability truth into the live core so
        // runtime gates match the parity report row for row.
        crate::capabilities::register_desktop_capabilities(&mut core)
            .map_err(DesktopError::from)?;
        let (loop_stop, loop_stop_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            core: Mutex::new(core),
            boundary,
            scan: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            failed_disables: Mutex::new(HashSet::new()),
            shut_down: AtomicBool::new(false),
            loop_stop,
            loop_done: Mutex::new(None),
        });
        let worker = tokio::spawn(scan_loop(Arc::clone(&inner), loop_stop_rx));
        *inner.loop_done.lock().await = Some(worker);
        Ok(Self { inner })
    }

    /// Borrow the radio boundary (event injection in tests runs through the
    /// boundary handle, not the central).
    #[must_use]
    pub fn boundary(&self) -> &B {
        &self.inner.boundary
    }

    fn admit(&self, operation: &str) -> Result<(), DesktopError> {
        if self.inner.shut_down.load(Ordering::SeqCst)
            || crate::executor::is_desktop_runtime_shut_down()
        {
            return Err(DesktopError::adapter_unavailable(operation));
        }
        Ok(())
    }

    /// Whether explicit shutdown has been recorded.
    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.inner.shut_down.load(Ordering::SeqCst)
    }

    /// Whether a scan session is currently owned.
    pub async fn has_active_scan(&self) -> bool {
        self.inner.scan.lock().await.is_some()
    }

    /// Core session peer key for a radio peripheral id, if resolved.
    pub async fn peer_key_for(&self, peer_id: &str) -> Option<String> {
        self.inner.peers.lock().await.get(peer_id).cloned()
    }

    /// Build a validated path selector with canonical UUIDs. Occurrence
    /// disambiguates duplicate UUIDs; UUID alone never identifies a path.
    pub fn selector(
        service_uuid: &str,
        service_occurrence: Option<u64>,
        characteristic_uuid: Option<&str>,
        characteristic_occurrence: Option<u64>,
        descriptor_uuid: Option<&str>,
        descriptor_occurrence: Option<u64>,
    ) -> Result<PathSelector, DesktopError> {
        let characteristic_uuid = characteristic_uuid
            .map(canonical_uuid)
            .transpose()
            .map_err(DesktopError::from)?;
        let descriptor_uuid = descriptor_uuid
            .map(canonical_uuid)
            .transpose()
            .map_err(DesktopError::from)?;
        Ok(PathSelector {
            service_uuid: canonical_uuid(service_uuid).map_err(DesktopError::from)?,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
        })
    }

    /// Start a scan: validate first (no radio effect on rejection), admit in
    /// the core, then start the OS scan. A radio failure settles the core
    /// session as failed and releases the scan owner — a failed start never
    /// wedges later scans.
    pub async fn start_scan(
        &self,
        owner: &str,
        service_uuids: &[&str],
        timeout_ms: u64,
    ) -> Result<ScanSession, DesktopError> {
        self.admit("scan.start")?;
        let request = validate_scan_request(service_uuids, "all", "none", timeout_ms, true, &[])
            .map_err(DesktopError::from)?;
        let filter = ScanFilterSpec {
            service_uuids: request.service_uuids().to_vec(),
        };
        let id = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            core.start_scan(&request, None, owner, now_ms(), &mut out)
                .map_err(DesktopError::from)?
        };
        if let Err(error) = self.inner.boundary.start_scan(filter).await {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let _ = core.note_scan_platform(
                &id,
                ubm_core::central::ScanPlatformEvent::StartFailed,
                now_ms(),
                &mut out,
            );
            let _ = core.settle_op(&id, ContenderKind::Failure, false, 0, now_ms(), &mut out);
            return Err(error);
        }
        {
            let mut core = self.inner.core.lock().await;
            // `start_scan` returning `Ok` is the OS acknowledgement.
            let _ = core.platform_scan_started(&id);
        }
        {
            let mut scan = self.inner.scan.lock().await;
            *scan = Some(ActiveScan { id: id.clone() });
        }
        Ok(ScanSession { id })
    }

    /// Stop the owned scan (idempotent): stop the OS scan, then settle the
    /// core session. An OS stop failure still settles the core session as
    /// failed and reports `scan.stop-failed` instead of swallowing cleanup.
    /// The central-lifetime event loop keeps running for connection events;
    /// it is joined only at [`DesktopCentral::shutdown`].
    pub async fn stop_scan(&self) -> Result<(), DesktopError> {
        let active = self.inner.scan.lock().await.take();
        let Some(active) = active else {
            return Ok(());
        };
        let stop_outcome = self.inner.boundary.stop_scan().await;
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        match stop_outcome {
            Ok(()) => {
                let _ = core.note_scan_platform(
                    &active.id,
                    ubm_core::central::ScanPlatformEvent::PlatformStopped,
                    now_ms(),
                    &mut out,
                );
                let _ = core.settle_op(
                    &active.id,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                Ok(())
            }
            Err(error) => {
                let _ = core.note_scan_platform(
                    &active.id,
                    ubm_core::central::ScanPlatformEvent::StopFailed,
                    now_ms(),
                    &mut out,
                );
                let _ = core.settle_op(
                    &active.id,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                Err(error)
            }
        }
    }

    /// Connect to a radio peer id (btleplug peripheral identity): resolve
    /// the peer, admit the connection, dispatch, then drive the radio. A
    /// radio failure marks peer loss (Connecting -> Lost, no resurrection)
    /// and settles the op as failed; the radio error takes precedence over
    /// compensation bookkeeping.
    pub async fn connect(
        &self,
        peer_id: &str,
        lease: &str,
        timeout_ms: u64,
    ) -> Result<ConnectionHandle, DesktopError> {
        self.admit("connection.connect")?;
        let peer_key = {
            let mut core = self.inner.core.lock().await;
            core.resolve_peer("platform-guid", peer_id)
                .map_err(DesktopError::from)?
        };
        // The peer is known from here regardless of the link outcome, so a
        // failed connect still leaves a peer loss the host can observe.
        self.inner
            .peers
            .lock()
            .await
            .insert(peer_id.to_owned(), peer_key.clone());
        let operation = {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let id = core
                .connect(&peer_key, lease, timeout_ms, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            id
        };
        match self.inner.boundary.connect(peer_id).await {
            Ok(()) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                // Best-effort: the event loop may have recorded the
                // DeviceConnected event first.
                let _ = core.note_link_established(&peer_key);
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                let connection_generation = core.connection_generation(&peer_key);
                Ok(ConnectionHandle {
                    peer_key,
                    connection_generation,
                })
            }
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                // Partial-failure cleanup: a half-opened OS link must not
                // linger without an owner. Bounded by the same 1 s
                // discipline as explicit disconnect (L5): a stuck radio
                // wait never hangs the failing connect.
                let _ = tokio::time::timeout(
                    DISCONNECT_COMPLETION_TIMEOUT,
                    self.inner.boundary.disconnect(peer_id),
                )
                .await;
                Err(error)
            }
        }
    }

    /// Explicit disconnect with a bounded radio wait: the link releases on
    /// OS confirmation, and a radio failure is retained as a cleanup failure
    /// (`report_disconnect_failure`) rather than reported as a good release.
    pub async fn disconnect(&self, peer_id: &str, lease: &str) -> Result<(), DesktopError> {
        self.admit("connection.disconnect")?;
        let peer_key = self
            .inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })?;
        {
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            core.disconnect(&peer_key, lease, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
        }
        let outcome = tokio::time::timeout(
            DISCONNECT_COMPLETION_TIMEOUT,
            self.inner.boundary.disconnect(peer_id),
        )
        .await;
        // Late radio completions must not resurrect the link: drop local
        // subscription routing for this peer now; the core already
        // invalidated its hubs at disconnect.
        self.drop_peer_subscriptions(peer_id).await;
        let mut core = self.inner.core.lock().await;
        match outcome {
            Ok(Ok(())) => {
                core.note_link_released(&peer_key)
                    .map_err(DesktopError::from)?;
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = core.report_disconnect_failure(&peer_key, error.code());
                Err(error)
            }
            Err(_) => {
                let _ = core.report_disconnect_failure(&peer_key, BleErrorCode::OperationTimedOut);
                Err(contract_error(
                    BleErrorCode::OperationTimedOut,
                    BleErrorDomain::Connection,
                    "connection.disconnect",
                )
                .with_detail("disconnect completion deadline exceeded"))
            }
        }
    }

    /// Radio-observed link loss (event loop or host): exactly one terminal,
    /// no double release (CLN-02).
    pub async fn remote_peer_loss(&self, peer_id: &str) -> Result<(), DesktopError> {
        let peer_key = self
            .inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })?;
        self.drop_peer_subscriptions(peer_id).await;
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        core.note_peer_loss(&peer_key, now_ms(), &mut out)
            .map_err(DesktopError::from)?;
        Ok(())
    }

    /// Discover services and register service/characteristic/descriptor
    /// paths with per-UUID occurrence identity. Unusable entries are
    /// skipped with their identities (partial failure); an empty usable
    /// snapshot fails discovery instead of completing an empty database.
    pub async fn discover(
        &self,
        peer_id: &str,
        lease: &str,
    ) -> Result<DiscoveryReport, DesktopError> {
        self.admit("discovery.complete")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        {
            let mut core = self.inner.core.lock().await;
            core.begin_discovery(&peer_key)
                .map_err(DesktopError::from)?;
        }
        let services = match self.inner.boundary.discover(peer_id).await {
            Ok(services) => services,
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let _ = core.fail_discovery(&peer_key);
                return Err(error);
            }
        };
        let mut report = DiscoveryReport::default();
        {
            let mut core = self.inner.core.lock().await;
            // The core registers paths against a Current database: the
            // radio snapshot completing is what advances Discovering to
            // Current; entries then register one by one underneath it.
            core.complete_discovery(&peer_key)
                .map_err(DesktopError::from)?;
            let mut service_counts: HashMap<String, u64> = HashMap::new();
            for service in &services {
                let service_occurrence = service_counts.entry(service.uuid.clone()).or_insert(0);
                let occurrence = *service_occurrence;
                *service_occurrence += 1;
                match core.register_path(
                    &peer_key,
                    &service.uuid,
                    occurrence,
                    None,
                    None,
                    None,
                    None,
                    0,
                    lease,
                ) {
                    Ok(_) => report.paths_registered += 1,
                    Err(error) => {
                        report
                            .skipped
                            .push((service.uuid.clone(), core_code_str(&error).to_owned()));
                        continue;
                    }
                }
                let mut char_counts: HashMap<String, u64> = HashMap::new();
                for characteristic in &service.characteristics {
                    let char_occurrence =
                        char_counts.entry(characteristic.uuid.clone()).or_insert(0);
                    let char_occ = *char_occurrence;
                    *char_occurrence += 1;
                    let bits =
                        crate::btleplug_backend::core_property_bits(characteristic.properties);
                    match core.register_path(
                        &peer_key,
                        &service.uuid,
                        occurrence,
                        Some(&characteristic.uuid),
                        Some(char_occ),
                        None,
                        None,
                        bits,
                        lease,
                    ) {
                        Ok(_) => report.paths_registered += 1,
                        Err(error) => {
                            report.skipped.push((
                                characteristic.uuid.clone(),
                                core_code_str(&error).to_owned(),
                            ));
                            continue;
                        }
                    }
                    let mut desc_counts: HashMap<String, u64> = HashMap::new();
                    for descriptor in &characteristic.descriptors {
                        let desc_occurrence =
                            desc_counts.entry(descriptor.uuid.clone()).or_insert(0);
                        let desc_occ = *desc_occurrence;
                        *desc_occurrence += 1;
                        // Descriptor values travel explicit descriptor
                        // operations; the CCCD stays managed by
                        // subscribe/unsubscribe (core rejects direct CCCD
                        // writes with `gatt.cccd-managed`).
                        let descriptor_bits =
                            ubm_core::central::GATT_PROP_READ | ubm_core::central::GATT_PROP_WRITE;
                        match core.register_path(
                            &peer_key,
                            &service.uuid,
                            occurrence,
                            Some(&characteristic.uuid),
                            Some(char_occ),
                            Some(&descriptor.uuid),
                            Some(desc_occ),
                            descriptor_bits,
                            lease,
                        ) {
                            Ok(_) => report.paths_registered += 1,
                            Err(error) => report
                                .skipped
                                .push((descriptor.uuid.clone(), core_code_str(&error).to_owned())),
                        }
                    }
                }
            }
            if report.paths_registered == 0 {
                // Unwind the empty completion through valid transitions:
                // a Current database with no usable paths is not a usable
                // outcome, so mark changed and require rediscovery rather
                // than leaving a hollow Current behind.
                let first = report
                    .skipped
                    .first()
                    .map(|(uuid, code)| format!("{uuid}:{code}"))
                    .unwrap_or_else(|| "empty-snapshot".to_owned());
                let _ = core.services_changed(&peer_key);
                let _ = core.require_rediscovery(&peer_key);
                return Err(contract_error(
                    BleErrorCode::GattNotFound,
                    BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail(format!("no usable GATT paths in radio snapshot ({first})")));
            }
        }
        Ok(report)
    }

    /// GATT read through a validated path: freshness, discovery, lease, and
    /// property checks run before kernel admission, so a stale path never
    /// dispatches to the radio.
    pub async fn read(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.admit("gatt.read")?;
        let (operation, key) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let characteristic = core
                .stored_path(index)
                .and_then(|path| path.characteristic_uuid().map(str::to_owned))
                .ok_or_else(|| {
                    contract_error(
                        BleErrorCode::GattPropertyNotSupported,
                        BleErrorDomain::Gatt,
                        "gatt.read",
                    )
                })?;
            let id = core
                .start_read(index, timeout_ms, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, instance_key(peer_id, selector, &characteristic))
        };
        match self
            .inner
            .boundary
            .read_characteristic(peer_id, &key.1, key.2, &key.3, key.4)
            .await
        {
            Ok(bytes) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                Ok(bytes)
            }
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                Err(error)
            }
        }
    }

    /// GATT write. `"long-write"` is rejected up front: prepared-write
    /// transactions have no btleplug radio path (see `PARITY_GAPS.md`),
    /// and a long value must never silently degrade to a single ATT write.
    pub async fn write(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        mode: &str,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        self.admit("gatt.write")?;
        if mode == "long-write" {
            return Err(contract_error(
                BleErrorCode::CapabilityLimited,
                BleErrorDomain::Capability,
                "gatt.write",
            )
            .with_detail("long-write needs a prepared-write radio path"));
        }
        let with_response = mode == "with-response";
        let value_len = value.len() as u64;
        // M1: the unbounded OS MTU lookup runs before the core lock, so a
        // stuck D-Bus round trip never stalls the event loop, cancel, or
        // disconnect behind this write.
        let measured_mtu = self.inner.boundary.mtu(peer_id).await;
        let (operation, key) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let maximum = Self::write_maximum(&core, measured_mtu, "gatt.write")?;
            let characteristic = core
                .stored_path(index)
                .and_then(|path| path.characteristic_uuid().map(str::to_owned))
                .ok_or_else(|| {
                    contract_error(
                        BleErrorCode::GattPropertyNotSupported,
                        BleErrorDomain::Gatt,
                        "gatt.write",
                    )
                })?;
            let id = core
                .start_write(
                    index,
                    mode,
                    value_len,
                    Some(maximum),
                    true,
                    timeout_ms,
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            (id, instance_key(peer_id, selector, &characteristic))
        };
        match self
            .inner
            .boundary
            .write_characteristic(peer_id, &key.1, key.2, &key.3, key.4, value, with_response)
            .await
        {
            Ok(()) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                Ok(())
            }
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                Err(error)
            }
        }
    }

    /// Descriptor read through a validated descriptor path.
    pub async fn read_descriptor(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.admit("gatt.read-descriptor")?;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).ok_or_else(|| {
                contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "path.index",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.read-descriptor",
                        )
                    })?;
            let descriptor = stored.descriptor_uuid().map(str::to_owned).ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.read-descriptor",
                )
            })?;
            let id = core
                .start_read_descriptor(index, timeout_ms, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            let key = instance_key(peer_id, selector, &characteristic);
            let descriptor_occurrence = selector.descriptor_occurrence.unwrap_or(0);
            (id, key, descriptor, descriptor_occurrence)
        };
        match self
            .inner
            .boundary
            .read_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
            )
            .await
        {
            Ok(bytes) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                Ok(bytes)
            }
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                Err(error)
            }
        }
    }

    /// Descriptor write through a validated descriptor path. Direct CCCD
    /// writes fail closed in the core with `gatt.cccd-managed`: sharing
    /// rules stay with subscribe/unsubscribe.
    pub async fn write_descriptor(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        value: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        self.admit("gatt.write-descriptor")?;
        let value_len = value.len() as u64;
        // M1: the unbounded OS MTU lookup runs before the core lock (see
        // `write`).
        let measured_mtu = self.inner.boundary.mtu(peer_id).await;
        let (operation, key, descriptor, descriptor_occurrence) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let stored = core.stored_path(index).ok_or_else(|| {
                contract_error(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "path.index",
                )
            })?;
            let characteristic =
                stored
                    .characteristic_uuid()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        contract_error(
                            BleErrorCode::GattPropertyNotSupported,
                            BleErrorDomain::Gatt,
                            "gatt.write-descriptor",
                        )
                    })?;
            let descriptor = stored.descriptor_uuid().map(str::to_owned).ok_or_else(|| {
                contract_error(
                    BleErrorCode::GattPropertyNotSupported,
                    BleErrorDomain::Gatt,
                    "gatt.write-descriptor",
                )
            })?;
            let maximum = Self::write_maximum(&core, measured_mtu, "gatt.write-descriptor")?;
            let id = core
                .start_write_descriptor(
                    index,
                    value_len,
                    Some(maximum),
                    timeout_ms,
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            core.dispatch_op(&id, &mut out)
                .map_err(DesktopError::from)?;
            let key = instance_key(peer_id, selector, &characteristic);
            let descriptor_occurrence = selector.descriptor_occurrence.unwrap_or(0);
            (id, key, descriptor, descriptor_occurrence)
        };
        match self
            .inner
            .boundary
            .write_descriptor(
                peer_id,
                &key.1,
                key.2,
                &key.3,
                key.4,
                &descriptor,
                descriptor_occurrence,
                value,
            )
            .await
        {
            Ok(()) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                Ok(())
            }
            Err(error) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                Err(error)
            }
        }
    }

    /// Subscribe one consumer: admit in the core, route early values through
    /// the hub (pre-ready values quarantine per GATT-04), then enable the
    /// physical CCCD. A radio failure settles the enablement as failed and
    /// removes routing — a failed subscribe never leaves a live CCCD.
    pub async fn subscribe(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
        timeout_ms: u64,
    ) -> Result<(), DesktopError> {
        self.admit("gatt.subscribe")?;
        // Resolve the instance first (pure read, no side effects) so the
        // pending-disable check below never nests locks: no path here ever
        // holds two mutexes at once.
        let key = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let core = self.inner.core.lock().await;
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let characteristic = core
                .stored_path(index)
                .and_then(|path| path.characteristic_uuid().map(str::to_owned))
                .ok_or_else(|| {
                    contract_error(
                        BleErrorCode::GattPropertyNotSupported,
                        BleErrorDomain::Gatt,
                        "gatt.subscribe",
                    )
                })?;
            instance_key(peer_id, selector, &characteristic)
        };
        // L7 resubscribe semantics: a pending failed disable fails the
        // resubscribe closed — complete the disable with `unsubscribe`
        // first instead of racing it with an enable.
        if self.inner.failed_disables.lock().await.contains(&key) {
            return Err(contract_error(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "gatt.subscribe",
            )
            .with_detail("physical disable pending; complete it with unsubscribe"));
        }
        let (operation, path_index, joined) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let characteristic = core
                .stored_path(index)
                .and_then(|path| path.characteristic_uuid().map(str::to_owned))
                .ok_or_else(|| {
                    contract_error(
                        BleErrorCode::GattPropertyNotSupported,
                        BleErrorDomain::Gatt,
                        "gatt.subscribe",
                    )
                })?;
            debug_assert_eq!(key, instance_key(peer_id, selector, &characteristic));
            // A live CCCD is shared, not re-enabled: joining admits a
            // consumer the core completes immediately, with no radio toggle.
            let joined = core.physical_cccd_enabled(index);
            let id = core
                .subscribe(
                    index,
                    "error",
                    DEFAULT_SUB_ITEM_CAP,
                    DEFAULT_SUB_BYTE_CAP,
                    consumer,
                    timeout_ms,
                    now_ms(),
                    &mut out,
                )
                .map_err(DesktopError::from)?;
            // Dispatch only a freshly queued op: a joined consumer's op is
            // already complete, and a re-subscribed in-flight op is already
            // dispatched. Dispatching either again would fail closed.
            let dispatchable = matches!(
                core.operation_state(&id),
                Some(ubm_core::ownership::OpStateView::Queued)
            );
            if dispatchable {
                core.dispatch_op(&id, &mut out)
                    .map_err(DesktopError::from)?;
            }
            (id, index, joined)
        };
        // Route before the physical enable so values arriving mid-enable
        // quarantine in the hub instead of dropping on the floor.
        self.inner
            .subscriptions
            .lock()
            .await
            .insert(key.clone(), path_index);
        if joined {
            return Ok(());
        }
        match self
            .inner
            .boundary
            .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, true)
            .await
        {
            Ok(()) => {
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_enable(path_index, true, now_ms(), &mut out);
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Success,
                    true,
                    0,
                    now_ms(),
                    &mut out,
                );
                Ok(())
            }
            Err(error) => {
                self.inner.subscriptions.lock().await.remove(&key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_enable(path_index, false, now_ms(), &mut out);
                let _ = core.settle_op(
                    &operation,
                    ContenderKind::Failure,
                    false,
                    0,
                    now_ms(),
                    &mut out,
                );
                Err(error)
            }
        }
    }

    /// Remove one consumer. Removing one consumer never disables another
    /// consumer's live CCCD: the physical disable fires only when the core
    /// reports the last removal issuing it. Returns whether the physical
    /// CCCD was disabled.
    ///
    /// Disable-failure semantics (L7): when the radio refuses the physical
    /// disable, routing stays in place so values keep flowing (no silent
    /// drops), the hub truthfully stays `Disabling`, and the key parks in
    /// the pending-disable set. A later `unsubscribe` retries the disable;
    /// a `subscribe` on the same instance fails closed until the disable
    /// completes.
    pub async fn unsubscribe(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<bool, DesktopError> {
        self.admit("gatt.unsubscribe")?;
        let (disable_physical, path_index, key) = {
            let peer_key = self.known_peer_key(peer_id).await?;
            let mut core = self.inner.core.lock().await;
            let mut out = batch();
            let index = core
                .resolve_path(&peer_key, selector)
                .map_err(DesktopError::from)?;
            let characteristic = core
                .stored_path(index)
                .and_then(|path| path.characteristic_uuid().map(str::to_owned))
                .ok_or_else(|| {
                    contract_error(
                        BleErrorCode::GattPropertyNotSupported,
                        BleErrorDomain::Gatt,
                        "gatt.unsubscribe",
                    )
                })?;
            let disable = core
                .unsubscribe(index, consumer, now_ms(), &mut out)
                .map_err(DesktopError::from)?;
            (
                disable,
                index,
                instance_key(peer_id, selector, &characteristic),
            )
        };
        if !disable_physical {
            return self.retry_failed_disable(peer_id, &key, path_index).await;
        }
        match self
            .inner
            .boundary
            .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false)
            .await
        {
            Ok(()) => {
                self.inner.subscriptions.lock().await.remove(&key);
                self.inner.failed_disables.lock().await.remove(&key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                Ok(true)
            }
            Err(error) => {
                // Routing stays: the CCCD is still live, so values must
                // still reach the hub. The pending-disable set routes the
                // next `unsubscribe` into a retry.
                self.inner.failed_disables.lock().await.insert(key);
                Err(error)
            }
        }
    }

    /// Retry a previously failed physical disable. Returns `Ok(true)` when
    /// the retry completes the disable, `Ok(false)` when no disable is
    /// pending, and the radio error when the retry fails again.
    async fn retry_failed_disable(
        &self,
        peer_id: &str,
        key: &InstanceKey,
        path_index: usize,
    ) -> Result<bool, DesktopError> {
        if !self.inner.failed_disables.lock().await.contains(key) {
            return Ok(false);
        }
        match self
            .inner
            .boundary
            .set_notifications(peer_id, &key.1, key.2, &key.3, key.4, false)
            .await
        {
            Ok(()) => {
                self.inner.subscriptions.lock().await.remove(key);
                self.inner.failed_disables.lock().await.remove(key);
                let mut core = self.inner.core.lock().await;
                let mut out = batch();
                let _ = core.settle_subscribe_disable(path_index, now_ms(), &mut out);
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    /// Take one buffered notification value for a consumer (M2, FIFO
    /// arrival order). Values buffer from subscription onward and stay
    /// observable after the radio delivers them; `None` means no value is
    /// waiting. Only the hub's admitted bytes cross here: values the
    /// radio never delivered are never synthesized.
    pub async fn take_notification(
        &self,
        peer_id: &str,
        selector: &PathSelector,
        consumer: &str,
    ) -> Result<Option<Vec<u8>>, DesktopError> {
        self.admit("gatt.take-notification")?;
        let peer_key = self.known_peer_key(peer_id).await?;
        let mut core = self.inner.core.lock().await;
        let index = core
            .resolve_path(&peer_key, selector)
            .map_err(DesktopError::from)?;
        Ok(core.take_notification_value(index, consumer))
    }

    /// Cancel one admitted operation (`operation.aborted` discipline in the
    /// core). Mid-flight radio abort is an OS gap — btleplug exposes no
    /// abort — so cancellation settles core-side while an in-flight radio
    /// call runs to its own (bounded) completion; see `PARITY_GAPS.md`.
    pub async fn cancel_operation(
        &self,
        operation: &OperationId,
    ) -> Result<ubm_core::central::CompletionOutcome, DesktopError> {
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        core.cancel_op(operation, now_ms(), &mut out)
            .map_err(DesktopError::from)
    }

    /// Explicit shutdown: stop the owned scan (scan cleanup), release live
    /// radio subscriptions through the boundary teardown hook, join the
    /// event loop so nothing races teardown, refuse new admission, record
    /// executor shutdown, and destroy the core owner. Idempotent.
    pub async fn shutdown(&self) {
        let _ = self.stop_scan().await;
        // M3: abort forwarders and best-effort release OS-side CCCDs so no
        // live subscription outlives the central.
        self.inner.boundary.close().await;
        self.inner.shut_down.store(true, Ordering::SeqCst);
        let worker = self.inner.loop_done.lock().await.take();
        let _ = self.inner.loop_stop.send(true);
        if let Some(worker) = worker {
            let _ = worker.await;
        }
        crate::executor::shutdown_desktop_runtime();
        let mut core = self.inner.core.lock().await;
        let mut out = batch();
        let _ = core.destroy(&mut out);
    }

    /// Compose the effective single-write maximum for one peer: the ATT
    /// protocol ceiling plus the OS-measured MTU as both the negotiated
    /// and the backend limit (btleplug submits one ATT operation per
    /// write; the OS enforces the negotiated MTU). An unmeasured MTU fails
    /// closed with `capability.unavailable`, never a guessed 23.
    /// Pure computation over a pre-fetched MTU: callers fetch
    /// `boundary.mtu()` before locking the core (M1), so this never
    /// awaits under the lock.
    fn write_maximum(
        core: &Central,
        measured_mtu: Option<u16>,
        operation: &'static str,
    ) -> Result<u64, DesktopError> {
        let directional = measured_mtu
            .map(|mtu| u64::from(mtu).saturating_sub(3))
            .filter(|limit| *limit > 0);
        core.maximum_write_length(Some(ATT_MAX_WRITE), directional, directional, operation)
            .map_err(DesktopError::from)
    }

    async fn known_peer_key(&self, peer_id: &str) -> Result<String, DesktopError> {
        self.inner
            .peers
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| {
                contract_error(
                    BleErrorCode::PeerNotFound,
                    BleErrorDomain::Connection,
                    "peer.known",
                )
            })
    }

    async fn drop_peer_subscriptions(&self, peer_id: &str) {
        clear_peer_routing(&self.inner, peer_id).await;
    }
}

/// Drop subscription routing and pending-disable retries for one peer.
/// Late radio completions must not resurrect the link: the core already
/// invalidated its hubs, and the adapter drops its routing alongside.
async fn clear_peer_routing<B>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let mut subscriptions = inner.subscriptions.lock().await;
    subscriptions.retain(|key, _| key.0 != peer_id);
    drop(subscriptions);
    let mut failed = inner.failed_disables.lock().await;
    failed.retain(|key| key.0 != peer_id);
}

/// Drive one central lifetime: resolve advertisements to platform-guid
/// peers, reconcile connection events with the core, and route
/// notifications to subscribed hubs. Every core settlement here is
/// best-effort — the explicit op paths own authoritative transitions, and a
/// racing explicit op must not fail because the loop saw the event first.
/// The loop owns core settlement only on the event-source-closed path;
/// [`DesktopCentral::stop_scan`] owns the stop path and
/// [`DesktopCentral::shutdown`] joins this worker, so cleanup cannot race.
async fn scan_loop<B: RadioBoundary>(inner: Arc<Inner<B>>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                let _ = changed;
                break;
            }
            event = inner.boundary.next_event() => {
                match event {
                    None => {
                        // Event source closed: settle an owned scan session
                        // as source-closed so the owner is released even
                        // when the OS never confirms a stop.
                        let id = inner.scan.lock().await.as_ref().map(|active| active.id.clone());
                        if let Some(id) = id {
                            let mut core = inner.core.lock().await;
                            let mut out = batch();
                            let _ = core.note_scan_platform(
                                &id,
                                ubm_core::central::ScanPlatformEvent::SourceClosed,
                                now_ms(),
                                &mut out,
                            );
                            let _ = core.settle_op(
                                &id,
                                ContenderKind::Success,
                                true,
                                0,
                                now_ms(),
                                &mut out,
                            );
                        }
                        break;
                    }
                    Some(RadioEvent::Advertisement(snapshot)) => {
                        ingest_advertisement(&inner, &snapshot).await;
                    }
                    Some(RadioEvent::Connected(peer_id)) => {
                        reconcile_connected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::Disconnected(peer_id)) => {
                        reconcile_disconnected(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::ServicesChanged(peer_id)) => {
                        services_changed_invalidated(&inner, &peer_id).await;
                    }
                    Some(RadioEvent::Notification {
                        peer_id,
                        service_uuid,
                        service_occurrence,
                        characteristic_uuid,
                        characteristic_occurrence,
                        value,
                    }) => {
                        deliver(
                            &inner,
                            &peer_id,
                            &service_uuid,
                            service_occurrence,
                            &characteristic_uuid,
                            characteristic_occurrence,
                            value,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

async fn ingest_advertisement<B: RadioBoundary>(inner: &Arc<Inner<B>>, snapshot: &PeerSnapshot) {
    // L8: resolve under the core lock, then drop the guard before the map
    // insert — the central lock is never held across the peers await, and
    // the insert stays out of the core critical section.
    let peer_key = {
        let mut core = inner.core.lock().await;
        // Platform-guid is the desktop peer identity: btleplug exposes the
        // address type opaquely per platform, so address targeting stays a
        // narrow-OS-adapter gap rather than a guessed domain.
        core.resolve_peer("platform-guid", &snapshot.id).ok()
    };
    if let Some(peer_key) = peer_key {
        inner
            .peers
            .lock()
            .await
            .insert(snapshot.id.clone(), peer_key);
    }
}

async fn reconcile_connected<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    if let Some(peer_key) = peer_key {
        let mut core = inner.core.lock().await;
        let _ = core.note_link_established(&peer_key);
    }
}

async fn reconcile_disconnected<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    if let Some(peer_key) = peer_key {
        clear_peer_routing(inner, peer_id).await;
        let mut core = inner.core.lock().await;
        let mut out = batch();
        let _ = core.note_peer_loss(&peer_key, now_ms(), &mut out);
    }
}

/// Route one radio notification into its per-instance hub with the full
/// value bytes (M2). Events without routing (unknown or unsubscribed
/// instance) drop on the floor: the hub never receives unattributable
/// bytes.
async fn deliver<B: RadioBoundary>(
    inner: &Arc<Inner<B>>,
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
    value: Vec<u8>,
) {
    let key = (
        peer_id.to_owned(),
        service_uuid.to_owned(),
        service_occurrence,
        characteristic_uuid.to_owned(),
        characteristic_occurrence,
    );
    let path_index = inner.subscriptions.lock().await.get(&key).copied();
    if let Some(path_index) = path_index {
        let mut core = inner.core.lock().await;
        let _ = core.deliver_notification_value(path_index, &value);
    }
}

/// Invalidate generations when the OS reports a changed GATT database
/// (L6): stale paths must fail closed and require rediscovery instead of
/// serving re-reads through dead handles. Routing drops alongside the
/// core hubs so late values cannot reach invalidated consumers.
async fn services_changed_invalidated<B: RadioBoundary>(inner: &Arc<Inner<B>>, peer_id: &str) {
    let peer_key = inner.peers.lock().await.get(peer_id).cloned();
    let Some(peer_key) = peer_key else {
        return;
    };
    clear_peer_routing(inner, peer_id).await;
    let mut core = inner.core.lock().await;
    let _ = core.services_changed(&peer_key);
    let _ = core.require_rediscovery(&peer_key);
}

/// Adapter behavior over the mocked boundary: scan ownership and cleanup,
/// peer/connection/GATT mapping with contract identities, partial
/// discovery failures, descriptor paths, subscription sharing, and
/// cancellation. No radio is touched; the fake boundary is the only
/// evidence source on this host.
#[cfg(test)]
mod adapter_tests {
    use std::time::Duration;

    use ubm_core::central::{ConnectionState, ConsumerState, ScanSessionState};

    use crate::boundary::{
        CharacteristicSnapshot, DescriptorSnapshot, FakeRadio, FaultOp, PeerSnapshot,
        PropertyFlags, RadioEvent, ServiceSnapshot,
    };

    use super::DesktopCentral;

    const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
    const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
    const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
    const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";

    fn notify_props() -> PropertyFlags {
        PropertyFlags {
            read: true,
            write: false,
            write_without_response: false,
            notify: true,
            indicate: false,
        }
    }

    fn rw_props() -> PropertyFlags {
        PropertyFlags {
            read: true,
            write: true,
            write_without_response: true,
            notify: false,
            indicate: false,
        }
    }

    fn hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: HRM_MEASUREMENT.to_owned(),
                occurrence: 0,
                properties: notify_props(),
                descriptors: vec![DescriptorSnapshot {
                    uuid: USER_DESCRIPTION.to_owned(),
                    occurrence: 0,
                }],
            }],
        }
    }

    /// One service carrying two same-UUID notify characteristics (wrist +
    /// chest strap): occurrence is the only instance identity.
    fn duplicate_hrm_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: notify_props(),
                    descriptors: Vec::new(),
                },
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 1,
                    properties: notify_props(),
                    descriptors: Vec::new(),
                },
            ],
        }
    }

    fn battery_service() -> ServiceSnapshot {
        ServiceSnapshot {
            uuid: BATTERY_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![CharacteristicSnapshot {
                uuid: BATTERY_LEVEL.to_owned(),
                occurrence: 0,
                properties: rw_props(),
                descriptors: Vec::new(),
            }],
        }
    }

    fn advertisement(peer_id: &str) -> RadioEvent {
        RadioEvent::Advertisement(PeerSnapshot {
            id: peer_id.to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
        })
    }

    async fn open() -> DesktopCentral<FakeRadio> {
        DesktopCentral::open(FakeRadio::new(), "test-host")
            .await
            .expect("open central")
    }

    async fn wait_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
        for _ in 0..200 {
            if central.peer_key_for(peer_id).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for peer {peer_id}");
    }

    fn hrm_selector(service_occurrence: u64) -> crate::central::PathSelector {
        hrm_instance_selector(service_occurrence, 0)
    }

    fn hrm_instance_selector(
        service_occurrence: u64,
        characteristic_occurrence: u64,
    ) -> crate::central::PathSelector {
        DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(service_occurrence),
            Some(HRM_MEASUREMENT),
            Some(characteristic_occurrence),
            None,
            None,
        )
        .expect("selector")
    }

    fn notification(peer_id: &str, characteristic_occurrence: u64, value: Vec<u8>) -> RadioEvent {
        RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HRM_MEASUREMENT.to_owned(),
            characteristic_occurrence,
            value,
        }
    }

    #[tokio::test]
    async fn scan_ingests_advertisements_and_cleans_up() {
        let central = open().await;
        central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("start scan");
        assert!(central.has_active_scan().await);
        central.boundary().push_event(advertisement("peer-1"));
        wait_peer(&central, "peer-1").await;
        central.stop_scan().await.expect("stop scan");
        assert!(!central.has_active_scan().await);
        assert!(
            !central.boundary().scan_active(),
            "OS scan stopped on cleanup"
        );
        let calls = central.boundary().calls();
        let start = calls
            .iter()
            .position(|call| call == "start_scan")
            .expect("start recorded");
        let stop = calls
            .iter()
            .position(|call| call == "stop_scan")
            .expect("stop recorded");
        assert!(start < stop, "stop follows start");
        assert_eq!(
            calls.iter().filter(|call| *call == "stop_scan").count(),
            1,
            "cleanup stops the OS scan exactly once"
        );
    }

    #[tokio::test]
    async fn failed_scan_start_releases_the_owner() {
        let central = open().await;
        central
            .boundary()
            .fail_next(FaultOp::StartScan, "os denied");
        let error = central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect_err("scripted start failure");
        assert_eq!(error.code_str(), "scan.start-failed");
        assert!(!central.has_active_scan().await);
        assert!(
            !central.boundary().calls().contains(&"stop_scan".to_owned()),
            "no stop without a start"
        );
        // The owner is released: a second start succeeds.
        central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("retry after failed start");
        central.stop_scan().await.expect("stop");
    }

    #[tokio::test]
    async fn second_scan_owner_is_rejected_without_radio_effect() {
        let central = open().await;
        central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("first");
        let error = central
            .start_scan("owner-b", &[], 5000)
            .await
            .expect_err("second owner rejected");
        assert_eq!(error.code_str(), "scan.already-active");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "start_scan")
                .count(),
            1,
            "rejected arbitration never reaches the radio"
        );
        central.stop_scan().await.expect("stop");
    }

    #[tokio::test]
    async fn event_source_close_settles_the_owned_scan() {
        let central = open().await;
        let session = central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("start");
        central.boundary().close_events();
        for _ in 0..200 {
            let state = central
                .with_core(|core| core.scan_session_state(session.operation_id()))
                .await;
            if state == Some(ScanSessionState::Stopped) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            central
                .with_core(|core| core.scan_session_state(session.operation_id()))
                .await,
            Some(ScanSessionState::Stopped),
            "source close releases the owner"
        );
        // Late stop stays a safe no-op cleanup, not a second settlement.
        central.stop_scan().await.expect("late stop");
    }

    #[tokio::test]
    async fn connect_does_not_share_without_rule() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-1"));
        let handle = central
            .connect("peer-1", "lease-a", 5000)
            .await
            .expect("connect");
        assert!(!handle.peer_key.is_empty());
        assert!(handle.connection_generation.is_some());
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(state, Some(ConnectionState::Connected));
        // No sharing support: a second lease is rejected before any radio call.
        let before = central.boundary().calls().len();
        let error = central
            .connect("peer-1", "lease-b", 5000)
            .await
            .expect_err("second lease rejected");
        assert_eq!(error.code_str(), "connection.already-owned");
        assert_eq!(
            central.boundary().calls().len(),
            before,
            "no radio on rejection"
        );
        central
            .disconnect("peer-1", "lease-a")
            .await
            .expect("disconnect");
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(state, Some(ConnectionState::Disconnected));
    }

    #[tokio::test]
    async fn connect_failure_marks_loss_and_cleans_half_open_link() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-9"));
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let error = central
            .connect("peer-9", "lease-a", 5000)
            .await
            .expect_err("scripted connect failure");
        assert_eq!(error.code_str(), "connection.failed");
        let peer_key = central.peer_key_for("peer-9").await.expect("peer known");
        let state = central
            .with_core(|core| core.connection_state(&peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Lost),
            "Connecting -> Lost, no resurrection"
        );
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "half-open OS link cleaned up"
        );
    }

    #[tokio::test]
    async fn disconnect_radio_failure_stays_disconnecting() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-2"));
        let handle = central
            .connect("peer-2", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .fail_next(FaultOp::Disconnect, "os stuck");
        let error = central
            .disconnect("peer-2", "lease-a")
            .await
            .expect_err("scripted disconnect failure");
        assert_eq!(error.operation(), "connection.disconnect");
        // Not reported clean: the link never reaches Disconnected.
        let state = central
            .with_core(|core| core.connection_state(&handle.peer_key))
            .await;
        assert_eq!(
            state,
            Some(ConnectionState::Disconnecting),
            "failed cleanup retains ownership"
        );
    }

    #[tokio::test]
    async fn discovery_registers_duplicate_uuids_by_occurrence() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-3"));
        central
            .connect("peer-3", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-3", vec![hrm_service(), hrm_service()]);
        let report = central
            .discover("peer-3", "lease-a")
            .await
            .expect("discover");
        // Two services x (service + characteristic + descriptor).
        assert_eq!(report.paths_registered, 6);
        assert!(report.skipped.is_empty());
        // Same UUID twice: occurrence selects each instance.
        let peer_key = central.peer_key_for("peer-3").await.expect("peer");
        for occurrence in [0u64, 1u64] {
            let selector = hrm_selector(occurrence);
            central
                .with_core(|core| core.resolve_path(&peer_key, &selector))
                .await
                .expect("occurrence resolves");
        }
        // Descriptor path reads through the validated descriptor level.
        let descriptor_selector = DesktopCentral::<FakeRadio>::selector(
            HRM_SERVICE,
            Some(0),
            Some(HRM_MEASUREMENT),
            Some(0),
            Some(USER_DESCRIPTION),
            Some(0),
        )
        .expect("descriptor selector");
        let value = central
            .read_descriptor("peer-3", &descriptor_selector, 5000)
            .await
            .expect("descriptor read");
        assert_eq!(value, vec![0x01]);
        // Characteristic read returns the fake payload.
        let value = central
            .read("peer-3", &hrm_selector(1), 5000)
            .await
            .expect("read");
        assert_eq!(value, vec![0x42]);
    }

    #[tokio::test]
    async fn read_after_peer_loss_fails_without_radio() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-4"));
        central
            .connect("peer-4", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-4", vec![battery_service()]);
        central
            .discover("peer-4", "lease-a")
            .await
            .expect("discover");
        central.remote_peer_loss("peer-4").await.expect("loss");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let error = central
            .read("peer-4", &selector, 5000)
            .await
            .expect_err("stale path never dispatches");
        // The link gate fails the read closed (`lifecycle.invalid-state`:
        // the Invalid database never reaches property validation). What the
        // adapter pins is the fail-closed discipline, not the core's code
        // choice: no radio dispatch, attributed error.
        assert_eq!(error.code_str(), "lifecycle.invalid-state");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"read_characteristic".to_owned()),
            "no radio call for a stale path"
        );
    }

    #[tokio::test]
    async fn write_without_measured_mtu_fails_closed() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-8"));
        central
            .connect("peer-8", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-8", vec![battery_service()]);
        central
            .discover("peer-8", "lease-a")
            .await
            .expect("discover");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        // No MTU scripted: the maximum is unmeasured, never guessed.
        let error = central
            .write("peer-8", &selector, vec![1], "with-response", 5000)
            .await
            .expect_err("unmeasured maximum fails closed");
        assert_eq!(error.code_str(), "capability.unavailable");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "no radio call without a measured maximum"
        );
    }

    #[tokio::test]
    async fn long_write_is_rejected_up_front() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-5"));
        central
            .connect("peer-5", "lease-a", 5000)
            .await
            .expect("connect");
        central.boundary().set_mtu("peer-5", 23);
        central
            .boundary()
            .set_services("peer-5", vec![battery_service()]);
        central
            .discover("peer-5", "lease-a")
            .await
            .expect("discover");
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let error = central
            .write("peer-5", &selector, vec![1, 2, 3], "long-write", 5000)
            .await
            .expect_err("long-write has no radio path");
        assert_eq!(error.code_str(), "capability.limited");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "never silently single-written"
        );
        // Ordinary write modes still flow.
        central
            .write("peer-5", &selector, vec![1, 2, 3], "with-response", 5000)
            .await
            .expect("plain write");
    }

    #[tokio::test]
    async fn subscription_sharing_keeps_one_cccd() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-6"));
        central
            .connect("peer-6", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-6", vec![hrm_service()]);
        central
            .discover("peer-6", "lease-a")
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .subscribe("peer-6", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe a");
        central
            .subscribe("peer-6", &selector, "consumer-b", 5000)
            .await
            .expect("subscribe b");
        let peer_key = central.peer_key_for("peer-6").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert!(
            central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "physical CCCD enabled"
        );
        assert_eq!(
            central
                .with_core(|core| core.consumer_state(path_index, "consumer-b"))
                .await,
            Some(ConsumerState::Ready),
            "second consumer shares the physical enablement"
        );
        // First removal keeps the other consumer's live CCCD.
        let disabled = central
            .unsubscribe("peer-6", &selector, "consumer-a")
            .await
            .expect("unsubscribe a");
        assert!(!disabled, "CCCD stays for consumer-b");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            1,
            "no physical toggle while a consumer remains"
        );
        // Last removal disables the physical CCCD.
        let disabled = central
            .unsubscribe("peer-6", &selector, "consumer-b")
            .await
            .expect("unsubscribe b");
        assert!(disabled, "last removal issues the physical disable");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, disable once"
        );
    }

    #[tokio::test]
    async fn notifications_reach_the_hub_stream() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-10"));
        central
            .connect("peer-10", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-10", vec![hrm_service()]);
        central
            .discover("peer-10", "lease-a")
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .subscribe("peer-10", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        let peer_key = central.peer_key_for("peer-10").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        // Overflow past the item bound under the error policy surfaces
        // exactly one terminal: values flow radio -> hub -> stream.
        for _ in 0..70 {
            central
                .boundary()
                .push_event(notification("peer-10", 0, vec![0x06, 0x40]));
        }
        for _ in 0..400 {
            let terminal = central
                .with_core_mut(|core| core.take_terminal(path_index, "consumer-a"))
                .await;
            if terminal.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            central
                .with_core_mut(|core| core.take_terminal(path_index, "consumer-a"))
                .await
                .is_none(),
            "terminal surfaces exactly once"
        );
        // M2: admitted values buffered before the overflow stay observable
        // through the take API — payloads are delivered, not dropped.
        let first = central
            .take_notification("peer-10", &selector, "consumer-a")
            .await
            .expect("take");
        assert_eq!(first, Some(vec![0x06, 0x40]), "buffered value observable");
        // M5 orphan-disable: removing the terminal (post-overflow Failed)
        // consumer releases the live CCCD instead of leaking it.
        let disabled = central
            .unsubscribe("peer-10", &selector, "consumer-a")
            .await
            .expect("unsubscribe");
        assert!(disabled, "terminal removal issues the orphan disable");
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "orphan CCCD released"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "enable once, orphan-disable once"
        );
    }

    #[tokio::test]
    async fn failed_subscribe_leaves_no_live_cccd() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-7"));
        central
            .connect("peer-7", "lease-a", 5000)
            .await
            .expect("connect");
        central
            .boundary()
            .set_services("peer-7", vec![hrm_service()]);
        central
            .discover("peer-7", "lease-a")
            .await
            .expect("discover");
        let selector = hrm_selector(0);
        central
            .boundary()
            .fail_next(FaultOp::Subscribe, "cccd refused");
        let error = central
            .subscribe("peer-7", &selector, "consumer-a", 5000)
            .await
            .expect_err("scripted subscribe failure");
        assert_eq!(error.code_str(), "gatt.subscribe-failed");
        let peer_key = central.peer_key_for("peer-7").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "no live CCCD after failed subscribe"
        );
    }

    #[tokio::test]
    async fn cancel_unknown_operation_fails_closed() {
        use ubm_core::contracts::OperationId;
        let central = open().await;
        let unknown = OperationId::new("central-op-9999").expect("id");
        let error = central
            .cancel_operation(&unknown)
            .await
            .expect_err("unknown op cannot cancel");
        assert!(!error.operation().is_empty(), "attributed error");
    }

    /// Connect, discover, and leave `peer_id` ready for GATT ops.
    async fn ready_peer(
        central: &DesktopCentral<FakeRadio>,
        peer_id: &str,
        services: Vec<ServiceSnapshot>,
    ) {
        central.boundary().push_event(advertisement(peer_id));
        central
            .connect(peer_id, "lease-a", 5000)
            .await
            .expect("connect");
        central.boundary().set_services(peer_id, services);
        central
            .discover(peer_id, "lease-a")
            .await
            .expect("discover");
    }

    #[tokio::test]
    async fn h1_duplicate_uuid_instances_route_per_instance() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-h1", vec![duplicate_hrm_service()]).await;
        // Per-instance payloads: wrist on occurrence 0, chest on 1.
        central.boundary().set_characteristic_value(
            "peer-h1",
            HRM_SERVICE,
            0,
            HRM_MEASUREMENT,
            0,
            vec![0x77],
        );
        central.boundary().set_characteristic_value(
            "peer-h1",
            HRM_SERVICE,
            0,
            HRM_MEASUREMENT,
            1,
            vec![0xc4, 0x35],
        );
        let wrist = central
            .read("peer-h1", &hrm_instance_selector(0, 0), 5000)
            .await
            .expect("read wrist");
        assert_eq!(wrist, vec![0x77], "occurrence 0 reads instance 0");
        let chest = central
            .read("peer-h1", &hrm_instance_selector(0, 1), 5000)
            .await
            .expect("read chest");
        assert_eq!(
            chest,
            vec![0xc4, 0x35],
            "occurrence 1 reads instance 1, not instance 0"
        );
        // Two same-UUID subscriptions keep distinct routing: each
        // instance's values reach only its own consumer.
        central
            .subscribe("peer-h1", &hrm_instance_selector(0, 0), "wrist-app", 5000)
            .await
            .expect("subscribe wrist");
        central
            .subscribe("peer-h1", &hrm_instance_selector(0, 1), "chest-app", 5000)
            .await
            .expect("subscribe chest");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            2,
            "two instances enable twice, never one shared forwarder"
        );
        central
            .boundary()
            .push_event(notification("peer-h1", 0, vec![0x01]));
        central
            .boundary()
            .push_event(notification("peer-h1", 1, vec![0x02]));
        let mut wrist_value = None;
        let mut chest_value = None;
        for _ in 0..200 {
            if wrist_value.is_none() {
                wrist_value = central
                    .take_notification("peer-h1", &hrm_instance_selector(0, 0), "wrist-app")
                    .await
                    .expect("take wrist");
            }
            if chest_value.is_none() {
                chest_value = central
                    .take_notification("peer-h1", &hrm_instance_selector(0, 1), "chest-app")
                    .await
                    .expect("take chest");
            }
            if wrist_value.is_some() && chest_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(wrist_value, Some(vec![0x01]), "wrist values reach wrist");
        assert_eq!(chest_value, Some(vec![0x02]), "chest values reach chest");
        let peer_key = central.peer_key_for("peer-h1").await.expect("peer");
        for (occurrence, consumer) in [(0u64, "wrist-app"), (1u64, "chest-app")] {
            let path = central
                .with_core(|core| {
                    core.resolve_path(&peer_key, &hrm_instance_selector(0, occurrence))
                        .expect("path")
                })
                .await;
            assert_eq!(
                central
                    .with_core(|core| core.consumer_state(path, consumer))
                    .await,
                Some(ConsumerState::Ready),
                "both consumers live on their own hub"
            );
        }
    }

    #[tokio::test]
    async fn m1_write_does_not_hold_core_lock_across_mtu() {
        let central = open().await;
        ready_peer(&central, "peer-m1", vec![battery_service()]).await;
        central.boundary().set_mtu("peer-m1", 23);
        central.boundary().block_op(FaultOp::Mtu);
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        let writer = central.clone();
        let pending_write = tokio::spawn(async move {
            writer
                .write("peer-m1", &selector, vec![1], "with-response", 5000)
                .await
        });
        // Let the write reach the gated MTU lookup, then prove the core
        // lock is free: link loss still completes while the write pends.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !pending_write.is_finished(),
            "write pends on the stuck MTU lookup"
        );
        tokio::time::timeout(Duration::from_secs(5), central.remote_peer_loss("peer-m1"))
            .await
            .expect("link loss never queues behind the write")
            .expect("loss recorded");
        central.boundary().unblock_op(FaultOp::Mtu);
        let outcome = tokio::time::timeout(Duration::from_secs(5), pending_write)
            .await
            .expect("write completes after unblock");
        let error = outcome
            .expect("write task")
            .expect_err("stale path fails closed");
        assert_eq!(error.code_str(), "lifecycle.invalid-state");
        assert!(
            !central
                .boundary()
                .calls()
                .contains(&"write_characteristic".to_owned()),
            "no radio call for the invalidated path"
        );
    }

    #[tokio::test]
    async fn m2_values_observable_after_subscribe() {
        let central = open().await;
        ready_peer(&central, "peer-m2", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-m2", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        // Nothing before the radio delivers: no synthesized values.
        assert_eq!(
            central
                .take_notification("peer-m2", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "no values before delivery"
        );
        central
            .boundary()
            .push_event(notification("peer-m2", 0, vec![0xde, 0xad]));
        central
            .boundary()
            .push_event(notification("peer-m2", 0, vec![0xbe, 0xef]));
        let mut first = None;
        let mut second = None;
        for _ in 0..200 {
            if first.is_none() {
                first = central
                    .take_notification("peer-m2", &selector, "consumer-a")
                    .await
                    .expect("take");
            } else if second.is_none() {
                second = central
                    .take_notification("peer-m2", &selector, "consumer-a")
                    .await
                    .expect("take");
            }
            if first.is_some() && second.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(first, Some(vec![0xde, 0xad]), "first value FIFO");
        assert_eq!(second, Some(vec![0xbe, 0xef]), "second value FIFO");
        assert_eq!(
            central
                .take_notification("peer-m2", &selector, "consumer-a")
                .await
                .expect("take"),
            None,
            "drained exactly, nothing invented"
        );
    }

    #[tokio::test]
    async fn m4_open_projects_desktop_capabilities() {
        use ubm_core::central::CapabilityAdmission;
        use ubm_core::contracts::BleErrorCode;

        let central = open().await;
        // A provided row gates open with its limitation...
        assert!(
            matches!(
                central
                    .with_core(
                        |core| core.check_capability("peer:resolve-reference", "desktop.probe")
                    )
                    .await,
                Ok(CapabilityAdmission::ProceedWithLimitation)
            ),
            "resolve-reference projects as provided-with-limitation"
        );
        // ...while open adapter work stays closed.
        let error = central
            .with_core(|core| core.check_capability("peer:address-targeting", "desktop.probe"))
            .await
            .expect_err("adapter work gates closed");
        assert_eq!(error.code(), BleErrorCode::CapabilityUnsupported);
    }

    #[tokio::test]
    async fn l5_connect_failure_cleanup_is_bounded() {
        let central = open().await;
        central.boundary().push_event(advertisement("peer-l5"));
        central.boundary().block_op(FaultOp::Disconnect);
        central.boundary().fail_next(FaultOp::Connect, "os refused");
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            central.connect("peer-l5", "lease-a", 5000),
        )
        .await
        .expect("failing connect never hangs on cleanup")
        .expect_err("scripted connect failure");
        assert_eq!(error.code_str(), "connection.failed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cleanup bounded by the 1 s discipline"
        );
        assert!(
            central
                .boundary()
                .calls()
                .contains(&"disconnect".to_owned()),
            "half-open link cleanup attempted despite the stuck radio"
        );
        central.boundary().unblock_op(FaultOp::Disconnect);
    }

    #[tokio::test]
    async fn l6_services_changed_invalidates_paths() {
        use ubm_core::central::DatabaseState;

        let central = open().await;
        ready_peer(&central, "peer-l6", vec![battery_service()]).await;
        let selector = DesktopCentral::<FakeRadio>::selector(
            BATTERY_SERVICE,
            Some(0),
            Some(BATTERY_LEVEL),
            Some(0),
            None,
            None,
        )
        .expect("selector");
        // A read works before the change...
        central
            .read("peer-l6", &selector, 5000)
            .await
            .expect("read before change");
        central
            .boundary()
            .push_event(RadioEvent::ServicesChanged("peer-l6".to_owned()));
        let peer_key = central.peer_key_for("peer-l6").await.expect("peer");
        let mut invalidated = false;
        for _ in 0..200 {
            let state = central
                .with_core(|core| core.database_state(&peer_key))
                .await;
            if state == Some(DatabaseState::Undiscovered) {
                invalidated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(invalidated, "database requires rediscovery after change");
        // ...and fails closed after it, with no radio dispatch through
        // the stale handle.
        let reads_before = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| *call == "read_characteristic")
            .count();
        central
            .read("peer-l6", &selector, 5000)
            .await
            .expect_err("stale handle never dispatches");
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "read_characteristic")
                .count(),
            reads_before,
            "no radio call for the invalidated path"
        );
    }

    #[tokio::test]
    async fn l6_disconnect_processed_under_notification_flood() {
        use ubm_core::central::ConnectionState;

        let central = open().await;
        ready_peer(&central, "peer-flood", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-flood", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        for _ in 0..100 {
            central
                .boundary()
                .push_event(notification("peer-flood", 0, vec![0x01]));
        }
        central
            .boundary()
            .push_event(RadioEvent::Disconnected("peer-flood".to_owned()));
        for _ in 0..100 {
            central
                .boundary()
                .push_event(notification("peer-flood", 0, vec![0x02]));
        }
        // The disconnect reconciles even though notifications surround it.
        let peer_key = central.peer_key_for("peer-flood").await.expect("peer");
        let mut lost = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.connection_state(&peer_key))
                .await
                == Some(ConnectionState::Lost)
            {
                lost = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(lost, "disconnect reconciled under flood");
        // Every flood value was accounted (bounded terminal or buffer),
        // never wedged behind the disconnect.
        let mut terminal_seen = false;
        for _ in 0..200 {
            let terminal = central
                .with_core_mut(|core| {
                    let path = core.resolve_path(&peer_key, &hrm_selector(0)).ok()?;
                    core.take_terminal(path, "consumer-a")
                })
                .await;
            if terminal.is_some() {
                terminal_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(terminal_seen, "flood accounted with a bounded terminal");
    }

    #[tokio::test]
    async fn l7_cancel_live_scan_operation() {
        use ubm_core::central::CompletionOutcome;
        use ubm_core::contracts::OperationTerminalKind;

        let central = open().await;
        let session = central
            .start_scan("owner-a", &[], 5000)
            .await
            .expect("start scan");
        let outcome = central
            .cancel_operation(session.operation_id())
            .await
            .expect("cancel live scan");
        match outcome {
            CompletionOutcome::Settled { kind, .. } => {
                assert_eq!(kind, OperationTerminalKind::Aborted, "live cancel aborts");
            }
            CompletionOutcome::DuplicateSuppressed { .. } | CompletionOutcome::ContenderIgnored => {
                panic!("live cancel must settle the operation, got {outcome:?}");
            }
        }
        // Cleanup after cancel stays safe: the late stop settles nothing
        // twice.
        central.stop_scan().await.expect("late stop");
    }

    #[tokio::test]
    async fn l7_pre_ready_values_quarantine_before_enable() {
        use ubm_core::central::ConsumerState;

        let central = open().await;
        ready_peer(&central, "peer-q", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central.boundary().block_op(FaultOp::Subscribe);
        let subscriber = central.clone();
        let pending_subscribe = tokio::spawn(async move {
            subscriber
                .subscribe("peer-q", &selector, "consumer-a", 5000)
                .await
        });
        // Wait until the enable reaches the (gated) radio...
        let mut reached_radio = false;
        for _ in 0..200 {
            if central
                .boundary()
                .calls()
                .contains(&"set_notifications".to_owned())
            {
                reached_radio = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(reached_radio, "enable attempted before quarantine check");
        // ...then prove values arriving mid-enable quarantine instead of
        // delivering or dropping.
        central
            .boundary()
            .push_event(notification("peer-q", 0, vec![0x09]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let peer_key = central.peer_key_for("peer-q").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        assert_eq!(
            central
                .with_core(|core| core.quarantined_count(path_index, "consumer-a"))
                .await,
            Some(1),
            "pre-ready value quarantined (GATT-04 ordering)"
        );
        assert_eq!(
            central
                .take_notification("peer-q", &hrm_selector(0), "consumer-a")
                .await
                .expect("take"),
            None,
            "quarantined values never deliver early"
        );
        central.boundary().unblock_op(FaultOp::Subscribe);
        pending_subscribe
            .await
            .expect("subscribe task")
            .expect("subscribe completes after unblock");
        assert_eq!(
            central
                .with_core(|core| core.consumer_state(path_index, "consumer-a"))
                .await,
            Some(ConsumerState::Ready),
            "consumer ready after enable settles"
        );
    }

    #[tokio::test]
    async fn l7_unsubscribe_disable_failure_retries() {
        let central = open().await;
        ready_peer(&central, "peer-ud", vec![hrm_service()]).await;
        let selector = hrm_selector(0);
        central
            .subscribe("peer-ud", &selector, "consumer-a", 5000)
            .await
            .expect("subscribe");
        central
            .boundary()
            .fail_next(FaultOp::Unsubscribe, "cccd stuck");
        let error = central
            .unsubscribe("peer-ud", &selector, "consumer-a")
            .await
            .expect_err("scripted disable failure");
        assert_eq!(error.code_str(), "gatt.subscribe-failed");
        // The CCCD is still live, so routing stays: values keep flowing
        // instead of dropping silently.
        let peer_key = central.peer_key_for("peer-ud").await.expect("peer");
        let path_index = central
            .with_core(|core| {
                core.resolve_path(&peer_key, &hrm_selector(0))
                    .expect("path")
            })
            .await;
        central
            .boundary()
            .push_event(notification("peer-ud", 0, vec![0x05]));
        let mut still_flows = false;
        for _ in 0..200 {
            if central
                .with_core(|core| core.pending_value_count(path_index, "consumer-a"))
                .await
                == Some(1)
            {
                still_flows = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            still_flows,
            "values still flow while the disable is pending"
        );
        // Resubscribing on the same instance fails closed until the
        // pending disable completes...
        let resubscribe = central
            .subscribe("peer-ud", &selector, "consumer-a", 5000)
            .await
            .expect_err("resubscribe races pending disable");
        assert_eq!(resubscribe.code_str(), "lifecycle.invalid-state");
        // ...and a later unsubscribe retries the disable to completion.
        let disabled = central
            .unsubscribe("peer-ud", &selector, "consumer-a")
            .await
            .expect("retry");
        assert!(disabled, "retry completes the pending disable");
        assert!(
            !central
                .with_core(|core| core.physical_cccd_enabled(path_index))
                .await,
            "CCCD released after retry"
        );
        assert_eq!(
            central
                .boundary()
                .calls()
                .iter()
                .filter(|call| *call == "set_notifications")
                .count(),
            3,
            "enable, failed disable, retry disable"
        );
    }
}

/// Test-only access to the core for state assertions. Real hosts observe
/// through the typed API, never through this lock.
#[cfg(test)]
pub(crate) mod test_support {
    use super::DesktopCentral;
    use crate::boundary::RadioBoundary;

    impl<B: RadioBoundary> DesktopCentral<B> {
        pub(crate) async fn with_core<T>(
            &self,
            view: impl FnOnce(&ubm_core::central::Central) -> T,
        ) -> T {
            let core = self.inner.core.lock().await;
            view(&core)
        }

        pub(crate) async fn with_core_mut<T>(
            &self,
            view: impl FnOnce(&mut ubm_core::central::Central) -> T,
        ) -> T {
            let mut core = self.inner.core.lock().await;
            view(&mut core)
        }
    }
}
