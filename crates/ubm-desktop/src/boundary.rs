//! Mockable btleplug radio boundary (HOST-DESKTOP).
//!
//! [`RadioBoundary`] is the only seam between [`crate::DesktopCentral`] and
//! the OS radio. Production traffic flows through the btleplug backend
//! (`btleplug_backend.rs`); unit tests drive [`FakeRadio`], a deterministic
//! scriptable boundary that never touches hardware. NO BLE hardware exists
//! on this host, so every radio proof here is a boundary-fault receipt, and
//! the physical central slice stays explicitly queued (see
//! `PARITY_GAPS.md`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Notify;

use crate::errors::DesktopError;

/// Which radio operation a scripted fault targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultOp {
    StartScan,
    StopScan,
    Connect,
    Disconnect,
    Discover,
    Read,
    Write,
    Subscribe,
    Unsubscribe,
    /// OS-reported ATT MTU lookup (`mtu()` returns `None`, as withheld).
    Mtu,
    /// Adapter identity readout (`adapter_name()` fails).
    AdapterName,
}

/// Service filter for scan start, mirroring the validated core request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanFilterSpec {
    /// Canonical 128-bit service UUIDs.
    pub service_uuids: Vec<String>,
}

/// One manufacturer-data section of an advertisement: the SIG company ID
/// plus the raw payload bytes verbatim. An empty payload is preserved as
/// empty (not dropped): vendor decoders distinguish "section present with
/// no payload" from "section absent" (no entry at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManufacturerData {
    /// SIG company identifier (e.g. `0x006b` Polar, `0x02b2` Oura).
    pub company_id: u16,
    /// Raw section payload bytes, verbatim.
    pub payload: Vec<u8>,
}

/// One service-data section of an advertisement: the service UUID plus the
/// raw payload bytes verbatim. Same empty-payload rule as
/// [`ManufacturerData`]: present-with-empty is preserved, never dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceData {
    /// Service UUID (canonical string).
    pub uuid: String,
    /// Raw section payload bytes, verbatim.
    pub payload: Vec<u8>,
}

/// One observed peer: radio identity plus latest advertisement facts.
///
/// Carries the complete discovery fact set the public observation contract
/// needs (`localName`, `serviceUuids`, `manufacturerData`, `serviceData`,
/// `rssi`, `txPowerLevel` per `CompactScanAdvertisement`; the device-kind
/// matcher consumes the first three): null/unknown/empty distinctions are
/// preserved (`local_name: None` vs `Some("")`, empty vs populated
/// vectors), and section payload bytes cross verbatim. One boundary note:
/// btleplug reports services as a plain vector, so "no services observed"
/// is always `[]` (unknown collapses to empty); the matcher treats `[]`
/// like `null` (no match), so no verdict changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSnapshot {
    /// btleplug peripheral id (opaque platform handle string).
    pub id: String,
    /// BLE address string when the OS exposes one (`None` on privacy-masked
    /// CoreBluetooth advertisements).
    pub address: Option<String>,
    /// Advertised service UUIDs (canonical strings).
    pub service_uuids: Vec<String>,
    /// Last RSSI in dBm, when measured.
    pub rssi: Option<i16>,
    /// Complete or shortened local name, when the advertisement carries
    /// one (`None` when absent; `Some("")` is preserved, never coerced).
    pub local_name: Option<String>,
    /// Manufacturer-data sections, payload bytes verbatim. The production
    /// backend sorts by company ID (btleplug reports a map); scripted
    /// boundaries preserve push order.
    pub manufacturer_data: Vec<ManufacturerData>,
    /// Service-data sections, payload bytes verbatim. The production
    /// backend sorts by UUID (btleplug reports a map); scripted boundaries
    /// preserve push order.
    pub service_data: Vec<ServiceData>,
    /// Advertised TX power level in dBm, when the advertisement carries
    /// one (`None` when absent).
    pub tx_power_level: Option<i16>,
}

/// GATT property flags for one characteristic snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyFlags {
    pub read: bool,
    pub write: bool,
    pub write_without_response: bool,
    pub notify: bool,
    pub indicate: bool,
}

/// One descriptor snapshot (UUID plus occurrence; values travel
/// read/write calls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSnapshot {
    pub uuid: String,
    /// Occurrence among duplicate descriptor UUIDs under one
    /// characteristic instance (0-based per-UUID count, matching the
    /// central's registration order).
    pub occurrence: u64,
}

/// One characteristic snapshot with occurrence index (duplicates stay
/// addressable through occurrence/path information, never UUID alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacteristicSnapshot {
    pub uuid: String,
    pub occurrence: u64,
    pub properties: PropertyFlags,
    pub descriptors: Vec<DescriptorSnapshot>,
}

/// One service snapshot with occurrence index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub uuid: String,
    pub occurrence: u64,
    pub characteristics: Vec<CharacteristicSnapshot>,
}

/// Radio-side events delivered to the central event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioEvent {
    Advertisement(PeerSnapshot),
    Connected(String),
    Disconnected(String),
    /// The OS reports the GATT database changed underneath discovery:
    /// paths invalidate and rediscovery is required (never silently
    /// re-read through stale handles).
    ServicesChanged(String),
    Notification {
        peer_id: String,
        /// Owning service instance: UUID plus occurrence among duplicate
        /// service UUIDs.
        service_uuid: String,
        service_occurrence: u64,
        characteristic_uuid: String,
        /// Occurrence among duplicate characteristic UUIDs under the
        /// service instance. UUID alone never identifies the instance.
        characteristic_occurrence: u64,
        /// Immutable subscription epoch captured when the notifying
        /// forwarder was installed (F10). The central rejects events whose
        /// epoch no longer matches the live routing: a value queued before
        /// a disconnect or service change must never enter a subscription
        /// created after it.
        epoch: u64,
        value: Vec<u8>,
    },
}

/// The OS-radio seam. Implementations are `Send + Sync` and shareable: the
/// central holds one `Arc`-capable boundary for the executor lifetime, and
/// every future is `Send` so scan loops and op drivers can move across the
/// shared multi-thread executor. Errors are already contract-attributed
/// [`DesktopError`]s.
pub trait RadioBoundary: Send + Sync + 'static {
    fn adapter_name(&self) -> impl Future<Output = Result<String, DesktopError>> + Send + '_;
    fn start_scan(
        &self,
        filter: ScanFilterSpec,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    fn stop_scan(&self) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    fn peers(&self) -> impl Future<Output = Result<Vec<PeerSnapshot>, DesktopError>> + Send + '_;
    fn connect<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    /// Live OS link state for one peer. Unknown peers report `Ok(false)`:
    /// no device, no link. Used to disambiguate a timed-out disconnect —
    /// a link the OS already released reports success, never a false
    /// timeout.
    fn is_connected<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<bool, DesktopError>> + Send + 'a;
    fn discover<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<Vec<ServiceSnapshot>, DesktopError>> + Send + 'a;
    /// Read one characteristic instance. Occurrence selects among
    /// duplicate UUIDs under the service instance (0-based per-UUID
    /// count); UUID alone never identifies the instance.
    fn read_characteristic<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
    ) -> impl Future<Output = Result<Vec<u8>, DesktopError>> + Send + 'a;
    #[allow(clippy::too_many_arguments)]
    fn write_characteristic<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        with_response: bool,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    #[allow(clippy::too_many_arguments)]
    fn read_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        descriptor_uuid: &'a str,
        descriptor_occurrence: u64,
    ) -> impl Future<Output = Result<Vec<u8>, DesktopError>> + Send + 'a;
    #[allow(clippy::too_many_arguments)]
    fn write_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        descriptor_uuid: &'a str,
        descriptor_occurrence: u64,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    /// Toggle notifications on one characteristic instance (per-instance
    /// keying: duplicate UUIDs never share a forwarder). On enable,
    /// `epoch` is the central's current subscription epoch for the peer:
    /// the installed forwarder captures it immutably and stamps every
    /// notification it emits, so stale queued values fail the routing check
    /// after a reconnect (F10). Disable ignores it (teardown is keyed by
    /// instance, not generation).
    #[allow(clippy::too_many_arguments)]
    fn set_notifications<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    /// OS-reported ATT MTU for one peer, or `None` when the OS withholds
    /// it. An unmeasured MTU is never defaulted: the adapter fails writes
    /// closed with `capability.unavailable` instead of guessing 23.
    fn mtu<'a>(&'a self, peer_id: &'a str) -> impl Future<Output = Option<u16>> + Send + 'a;
    /// Next radio event, or `None` when the event source closes (scan
    /// cleanup path). Cancellation is owned by the caller: drop/stop the
    /// future rather than expecting the boundary to abort it.
    fn next_event(&self) -> impl Future<Output = Option<RadioEvent>> + Send + '_;
    /// Teardown hook: abort live notification forwarders and best-effort
    /// release OS-side CCCDs. Wired into central shutdown so no live OS
    /// subscription outlives the central. Infallible by contract: per-scope
    /// release failures are retained, never raised — the host drains them
    /// via [`RadioBoundary::take_close_failures`] into the shutdown report.
    fn close(&self) -> impl Future<Output = ()> + Send + '_;
    /// Drain close-time release failures retained by the last [`RadioBoundary::close`]
    /// (F14 receipts). Each entry names one characteristic scope whose native
    /// release did not complete; an empty vec means every scope released (or
    /// none was live). Radios without per-scope release accounting keep the
    /// default empty vec.
    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        Vec::new()
    }
}

/// Test ingress bounds (F07): data (notifications) and control
/// (advertisements, connection, service-change) travel separate bounded
/// queues so a data flood can neither exhaust memory nor starve control.
/// 256 data items / 256 KiB bytes holds every existing test workload (max
/// 200 flood events) while proving overload drops under larger floods;
/// 64 control events never fills in tests (control is low-volume).
const FAKE_DATA_CAP: usize = 256;
const FAKE_DATA_BYTES: u64 = 262_144;
const FAKE_CONTROL_CAP: usize = 64;

/// Deterministic scriptable boundary for unit tests. Faults are injected per
/// operation with [`FakeRadio::fail_next`]; queued events are replayed in
/// push order through [`FakeRadio::push_event`]. Like a real OS event stream,
/// [`RadioBoundary::next_event`] pends while the queues are empty and returns
/// `None` only after [`FakeRadio::close_events`] drops the source. Every
/// call is recorded so tests can assert cleanup ordering (e.g. stop-scan
/// after start failure never fires twice).
pub struct FakeRadio {
    state: StdMutex<FakeInner>,
    notify: Arc<Notify>,
}

/// One characteristic instance address: (peer, service uuid, service
/// occurrence, characteristic uuid, characteristic occurrence).
pub type InstanceKey = (String, String, u64, String, u64);

/// One descriptor address: characteristic instance plus descriptor
/// uuid/occurrence.
pub type DescriptorKey = (InstanceKey, String, u64);

/// One characteristic scope whose close-time native release did not
/// complete (F14 receipt). The scope stays live at the radio: a failed
/// unsubscribe leaves the OS enablement behind, and the shutdown report
/// must say so instead of claiming a clean release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioCloseFailure {
    /// Characteristic instance address (peer, service, occurrences).
    pub scope: InstanceKey,
    /// Radio-side reason (fault detail or OS error string).
    pub detail: String,
}

impl RadioCloseFailure {
    /// Record one unreleased scope with its reason.
    #[must_use]
    pub fn new(scope: InstanceKey, detail: String) -> Self {
        Self { scope, detail }
    }
}

struct FakeInner {
    faults: HashMap<FaultOp, VecDeque<String>>,
    /// Separate bounded queues (F07) with a shared push sequence: data
    /// (notifications) bounded by items+bytes with explicit drops, control
    /// (advertisements, connection, service-change) bounded by items. Drain
    /// order follows the shared sequence (push order), so a disconnect never
    /// jumps ahead of earlier data, yet never drops because data is full.
    control: VecDeque<(u64, RadioEvent)>,
    data: VecDeque<(u64, RadioEvent)>,
    seq: u64,
    events_closed: bool,
    /// Bytes currently queued in the data queue (F07 item+byte bound).
    data_bytes: u64,
    /// Data notifications dropped by explicit overload (F07): bounded
    /// ingress never grows memory, and drops are counted, never silent.
    dropped_data: u64,
    /// Control events dropped by explicit overload past [`FAKE_CONTROL_CAP`]:
    /// counted like data drops, never silent. Control is low-volume (tests
    /// push far fewer than 64), so any nonzero count here is a test-design
    /// signal, not expected backpressure.
    dropped_control: u64,
    calls: Vec<String>,
    connected: Vec<String>,
    notifications: Vec<(String, String, bool)>,
    scan_active: bool,
    services: HashMap<String, Vec<ServiceSnapshot>>,
    mtu: HashMap<String, u16>,
    /// Per-instance read payloads: values returned for one addressed
    /// characteristic instance (unset instances return the canned default).
    values: HashMap<InstanceKey, Vec<u8>>,
    /// Live notification registrations: (peer, service, svc occ, char,
    /// char occ) with the CCCD currently enabled. [`FakeRadio::close`]
    /// releases all of them, modelling OS-side unsubscribe at teardown.
    live: HashSet<InstanceKey>,
    /// Close-time release failures retained by the last [`FakeRadio::close`]
    /// (F14 receipts): one entry per scope whose unsubscribe fault fired.
    /// Drained by `take_close_failures`; failed scopes stay in `live`.
    close_failures: Vec<RadioCloseFailure>,
    /// Observed characteristic writes: addressed instance plus the
    /// response mode the adapter selected (`true` = with-response).
    writes: Vec<(InstanceKey, bool)>,
    /// Observed descriptor reads/writes: addressed descriptor keys.
    descriptor_reads: Vec<DescriptorKey>,
    descriptor_writes: Vec<DescriptorKey>,
    /// Subscription epochs captured at forwarder install, in enable
    /// order: addressed instance plus the epoch the central passed.
    enable_epochs: Vec<(InstanceKey, u64)>,
    /// Closed operation gates: an entry means calls to that op wait until
    /// [`FakeRadio::unblock_op`] (contention/failure-injection tests).
    gates: HashMap<FaultOp, Arc<Notify>>,
}

impl Default for FakeRadio {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRadio {
    pub fn new() -> Self {
        Self {
            state: StdMutex::new(FakeInner {
                faults: HashMap::new(),
                control: VecDeque::new(),
                data: VecDeque::new(),
                seq: 0,
                events_closed: false,
                data_bytes: 0,
                dropped_data: 0,
                dropped_control: 0,
                calls: Vec::new(),
                connected: Vec::new(),
                notifications: Vec::new(),
                scan_active: false,
                services: HashMap::new(),
                mtu: HashMap::new(),
                values: HashMap::new(),
                live: HashSet::new(),
                close_failures: Vec::new(),
                writes: Vec::new(),
                descriptor_reads: Vec::new(),
                descriptor_writes: Vec::new(),
                enable_epochs: Vec::new(),
                gates: HashMap::new(),
            }),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Script the OS-reported ATT MTU for `peer_id`. Unset means the OS
    /// withholds it, and writes fail closed as unmeasured.
    pub fn set_mtu(&self, peer_id: &str, mtu: u16) {
        self.state
            .lock()
            .expect("fake radio state")
            .mtu
            .insert(peer_id.to_owned(), mtu);
    }

    /// Script the next failure of `op`; the detail becomes the error detail.
    pub fn fail_next(&self, op: FaultOp, detail: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .faults
            .entry(op)
            .or_default()
            .push_back(detail.to_owned());
    }

    /// Queue one radio event for the next [`RadioBoundary::next_event`].
    /// Data (notifications) travel a bounded item+byte queue with explicit
    /// overload drops (counted via [`FakeRadio::dropped_notification_count`]);
    /// control travels a separate bounded queue so a data flood can neither
    /// drop control nor reorder it ahead of earlier data: drain follows push
    /// order (F07). Pushes after [`FakeRadio::close_events`] are dropped,
    /// like OS events after the source closes.
    pub fn push_event(&self, event: RadioEvent) {
        let mut state = self.state.lock().expect("fake radio state");
        if state.events_closed {
            return;
        }
        let seq = state.seq.saturating_add(1);
        state.seq = seq;
        match event {
            RadioEvent::Notification { ref value, .. } => {
                let value_len = value.len() as u64;
                if state.data.len() >= FAKE_DATA_CAP
                    || state.data_bytes.saturating_add(value_len) > FAKE_DATA_BYTES
                {
                    state.dropped_data = state.dropped_data.saturating_add(1);
                    return;
                }
                state.data_bytes = state.data_bytes.saturating_add(value_len);
                state.data.push_back((seq, event));
            }
            control => {
                if state.control.len() >= FAKE_CONTROL_CAP {
                    state.dropped_control = state.dropped_control.saturating_add(1);
                    return;
                }
                state.control.push_back((seq, control));
            }
        }
        drop(state);
        self.notify.notify_one();
    }

    /// Close the event source: a pending [`RadioBoundary::next_event`]
    /// resolves to `None` once both queues drain, modelling OS event-source
    /// teardown.
    pub fn close_events(&self) {
        self.state.lock().expect("fake radio state").events_closed = true;
        self.notify.notify_one();
    }

    /// Data notifications dropped by explicit ingress overload (F07).
    pub fn dropped_notification_count(&self) -> u64 {
        self.state.lock().expect("fake radio state").dropped_data
    }

    /// Control events dropped by explicit ingress overload past the control
    /// cap (64). Always zero in well-formed tests; nonzero means the test
    /// pushed more control than the bound admits.
    pub fn dropped_control_count(&self) -> u64 {
        self.state.lock().expect("fake radio state").dropped_control
    }

    /// Bytes currently queued in the data ingress (F07 bound evidence).
    pub fn data_queued_bytes(&self) -> u64 {
        self.state.lock().expect("fake radio state").data_bytes
    }

    /// Recorded call names in order (`"start_scan"`, `"stop_scan"`, ...).
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().expect("fake radio state").calls.clone()
    }

    /// Whether the fake believes a scan is active.
    pub fn scan_active(&self) -> bool {
        self.state.lock().expect("fake radio state").scan_active
    }

    /// Script the discovery snapshot returned for `peer_id`.
    pub fn set_services(&self, peer_id: &str, services: Vec<ServiceSnapshot>) {
        self.state
            .lock()
            .expect("fake radio state")
            .services
            .insert(peer_id.to_owned(), services);
    }

    /// Script the OS-side link state directly: models a radio whose link
    /// released (or stayed up) independent of the outstanding disconnect
    /// call — the shape a timed-out disconnect must disambiguate.
    pub fn set_link_connected(&self, peer_id: &str, connected: bool) {
        let mut state = self.state.lock().expect("fake radio state");
        state.connected.retain(|peer| peer != peer_id);
        if connected {
            state.connected.push(peer_id.to_owned());
        }
    }

    /// Script the read payload for one characteristic instance. Reads of
    /// unset instances return the canned default (`0x42`).
    pub fn set_characteristic_value(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
    ) {
        self.state.lock().expect("fake radio state").values.insert(
            (
                peer_id.to_owned(),
                service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid.to_owned(),
                characteristic_occurrence,
            ),
            value,
        );
    }

    /// Number of live notification registrations (CCCDs the fake believes
    /// are OS-enabled). Teardown must drive this to zero.
    pub fn live_subscription_count(&self) -> usize {
        self.state.lock().expect("fake radio state").live.len()
    }

    /// Observed characteristic writes in order: addressed instance key
    /// plus the response mode (`true` = with-response).
    pub fn writes(&self) -> Vec<(InstanceKey, bool)> {
        self.state.lock().expect("fake radio state").writes.clone()
    }

    /// Observed descriptor reads in order: addressed descriptor keys.
    pub fn descriptor_reads(&self) -> Vec<DescriptorKey> {
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_reads
            .clone()
    }

    /// Observed descriptor writes in order: addressed descriptor keys.
    pub fn descriptor_writes(&self) -> Vec<DescriptorKey> {
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_writes
            .clone()
    }

    /// Epochs captured at forwarder install, in enable order: addressed
    /// instance plus the epoch the central passed to `set_notifications`.
    pub fn enable_epochs(&self) -> Vec<(InstanceKey, u64)> {
        self.state
            .lock()
            .expect("fake radio state")
            .enable_epochs
            .clone()
    }

    /// Close the gate on `op`: calls to it wait until
    /// [`FakeRadio::unblock_op`]. Models a stuck OS call for contention
    /// tests (M1/M2/L5/L7).
    pub fn block_op(&self, op: FaultOp) {
        self.state
            .lock()
            .expect("fake radio state")
            .gates
            .entry(op)
            .or_insert_with(|| Arc::new(Notify::new()));
    }

    /// Open the gate on `op`, releasing one waiter (call again per
    /// waiter).
    pub fn unblock_op(&self, op: FaultOp) {
        let notify = self
            .state
            .lock()
            .expect("fake radio state")
            .gates
            .remove(&op);
        if let Some(notify) = notify {
            notify.notify_one();
        }
    }

    /// Wait while the gate on `op` is closed. The check-then-wait loop is
    /// race-free: `unblock_op` both removes the entry and delivers a
    /// stored permit, so a concurrent unblock either breaks the loop or
    /// releases the wait immediately.
    async fn gate(&self, op: FaultOp) {
        loop {
            let notify = self
                .state
                .lock()
                .expect("fake radio state")
                .gates
                .get(&op)
                .cloned();
            match notify {
                None => break,
                Some(notify) => notify.notified().await,
            }
        }
    }

    fn take_fault(&self, op: FaultOp) -> Option<String> {
        self.state
            .lock()
            .expect("fake radio state")
            .faults
            .get_mut(&op)
            .and_then(VecDeque::pop_front)
    }

    fn record(&self, call: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .calls
            .push(call.to_owned());
    }
}

/// Build the addressed descriptor key for one descriptor call.
#[allow(clippy::too_many_arguments)]
fn descriptor_key(
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
    descriptor_uuid: &str,
    descriptor_occurrence: u64,
) -> DescriptorKey {
    (
        (
            peer_id.to_owned(),
            service_uuid.to_owned(),
            service_occurrence,
            characteristic_uuid.to_owned(),
            characteristic_occurrence,
        ),
        descriptor_uuid.to_owned(),
        descriptor_occurrence,
    )
}

impl RadioBoundary for FakeRadio {
    async fn adapter_name(&self) -> Result<String, DesktopError> {
        self.record("adapter_name");
        if let Some(detail) = self.take_fault(FaultOp::AdapterName) {
            return Err(DesktopError::adapter_unavailable("adapter.name").with_detail(detail));
        }
        Ok("fake-desktop-adapter".to_owned())
    }

    async fn start_scan(&self, _filter: ScanFilterSpec) -> Result<(), DesktopError> {
        self.record("start_scan");
        if let Some(detail) = self.take_fault(FaultOp::StartScan) {
            return Err(DesktopError::scan_start_failed(detail));
        }
        // Contention gate (mirrors connect/disconnect): tests close it via
        // `block_op(FaultOp::StartScan)` to hold a scan start in flight for
        // stop-while-starting and shutdown-during-start races.
        self.gate(FaultOp::StartScan).await;
        self.state.lock().expect("fake radio state").scan_active = true;
        Ok(())
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        self.record("stop_scan");
        if let Some(detail) = self.take_fault(FaultOp::StopScan) {
            return Err(DesktopError::scan_stop_failed(detail));
        }
        // Contention gate (mirrors connect/disconnect): tests close it via
        // `block_op(FaultOp::StopScan)` to hold a scan stop in flight.
        self.gate(FaultOp::StopScan).await;
        self.state.lock().expect("fake radio state").scan_active = false;
        Ok(())
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        self.record("peers");
        Ok(Vec::new())
    }

    async fn connect(&self, peer_id: &str) -> Result<(), DesktopError> {
        self.record("connect");
        if let Some(detail) = self.take_fault(FaultOp::Connect) {
            return Err(DesktopError::connection_failed(detail));
        }
        self.gate(FaultOp::Connect).await;
        self.state
            .lock()
            .expect("fake radio state")
            .connected
            .push(peer_id.to_owned());
        Ok(())
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        self.record("disconnect");
        if let Some(detail) = self.take_fault(FaultOp::Disconnect) {
            return Err(DesktopError::new(
                ubm_core::contracts::BleErrorCode::ConnectionLost,
                ubm_core::contracts::BleErrorDomain::Connection,
                "connection.disconnect",
            )
            .with_detail(detail));
        }
        self.gate(FaultOp::Disconnect).await;
        self.state
            .lock()
            .expect("fake radio state")
            .connected
            .retain(|peer| peer != peer_id);
        Ok(())
    }

    async fn is_connected(&self, peer_id: &str) -> Result<bool, DesktopError> {
        Ok(self
            .state
            .lock()
            .expect("fake radio state")
            .connected
            .iter()
            .any(|peer| peer == peer_id))
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        self.record("discover");
        if let Some(detail) = self.take_fault(FaultOp::Discover) {
            return Err(DesktopError::new(
                ubm_core::contracts::BleErrorCode::GattDiscoveryRequired,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "discovery.complete",
            )
            .with_detail(detail));
        }
        Ok(self
            .state
            .lock()
            .expect("fake radio state")
            .services
            .get(peer_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.record("read_characteristic");
        if let Some(detail) = self.take_fault(FaultOp::Read) {
            return Err(DesktopError::read_failed(detail));
        }
        self.gate(FaultOp::Read).await;
        Ok(self
            .state
            .lock()
            .expect("fake radio state")
            .values
            .get(&(
                peer_id.to_owned(),
                service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid.to_owned(),
                characteristic_occurrence,
            ))
            .cloned()
            .unwrap_or_else(|| vec![0x42]))
    }

    async fn write_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        _value: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DesktopError> {
        self.record("write_characteristic");
        if let Some(detail) = self.take_fault(FaultOp::Write) {
            return Err(DesktopError::write_failed(detail));
        }
        self.gate(FaultOp::Write).await;
        self.state.lock().expect("fake radio state").writes.push((
            (
                peer_id.to_owned(),
                service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid.to_owned(),
                characteristic_occurrence,
            ),
            with_response,
        ));
        Ok(())
    }

    async fn read_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.record("read_descriptor");
        if let Some(detail) = self.take_fault(FaultOp::Read) {
            return Err(DesktopError::read_failed(detail));
        }
        self.gate(FaultOp::Read).await;
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_reads
            .push(descriptor_key(
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                descriptor_uuid,
                descriptor_occurrence,
            ));
        Ok(vec![0x01])
    }

    async fn write_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
        _value: Vec<u8>,
    ) -> Result<(), DesktopError> {
        self.record("write_descriptor");
        if let Some(detail) = self.take_fault(FaultOp::Write) {
            return Err(DesktopError::write_failed(detail));
        }
        self.gate(FaultOp::Write).await;
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_writes
            .push(descriptor_key(
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                descriptor_uuid,
                descriptor_occurrence,
            ));
        Ok(())
    }

    async fn set_notifications(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
    ) -> Result<(), DesktopError> {
        self.record("set_notifications");
        // Enable and disable faults inject independently: a CCCD-enable
        // refusal must not mask the disable-failure path under test.
        let fault = if enable {
            FaultOp::Subscribe
        } else {
            FaultOp::Unsubscribe
        };
        if let Some(detail) = self.take_fault(fault) {
            return Err(DesktopError::subscribe_failed(detail));
        }
        self.gate(fault).await;
        let key = (
            peer_id.to_owned(),
            service_uuid.to_owned(),
            service_occurrence,
            characteristic_uuid.to_owned(),
            characteristic_occurrence,
        );
        let mut state = self.state.lock().expect("fake radio state");
        state
            .notifications
            .push((peer_id.to_owned(), characteristic_uuid.to_owned(), enable));
        if enable {
            state.live.insert(key.clone());
            state.enable_epochs.push((key, epoch));
        } else {
            state.live.remove(&key);
        }
        Ok(())
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        // F07: drain follows push order across the separate queues — earlier
        // data delivers before a later disconnect (causality preserved), yet
        // a full data queue never drops control.
        loop {
            {
                let mut state = self.state.lock().expect("fake radio state");
                let control_seq = state.control.front().map(|(seq, _)| *seq);
                let data_seq = state.data.front().map(|(seq, _)| *seq);
                let take_control = match (control_seq, data_seq) {
                    (Some(c), Some(d)) => c < d,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => {
                        if state.events_closed {
                            return None;
                        }
                        false
                    }
                };
                if control_seq.is_some() || data_seq.is_some() {
                    if take_control {
                        let (_, event) = state.control.pop_front().expect("control head");
                        return Some(event);
                    }
                    let (_, event) = state.data.pop_front().expect("data head");
                    if let RadioEvent::Notification { ref value, .. } = event {
                        let len = value.len() as u64;
                        state.data_bytes = state.data_bytes.saturating_sub(len);
                    }
                    return Some(event);
                }
            }
            self.notify.notified().await;
        }
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        self.record("mtu");
        if self.take_fault(FaultOp::Mtu).is_some() {
            return None;
        }
        self.gate(FaultOp::Mtu).await;
        self.state
            .lock()
            .expect("fake radio state")
            .mtu
            .get(peer_id)
            .copied()
    }

    async fn close(&self) {
        self.record("close");
        // Deterministic scope order: HashSet iteration is unstable, and
        // close receipts must list scopes in a repeatable sequence.
        let mut scopes: Vec<InstanceKey> = self
            .state
            .lock()
            .expect("fake radio state")
            .live
            .iter()
            .cloned()
            .collect();
        scopes.sort();
        let mut state = self.state.lock().expect("fake radio state");
        state.close_failures.clear();
        for scope in scopes {
            // One queued `Unsubscribe` fault fails one scope's release (F14
            // injection): the failure is retained as a receipt and the
            // scope stays live, like an OS enablement that survived.
            let fault = state
                .faults
                .get_mut(&FaultOp::Unsubscribe)
                .and_then(VecDeque::pop_front);
            match fault {
                Some(detail) => {
                    state
                        .close_failures
                        .push(RadioCloseFailure::new(scope, detail));
                }
                None => {
                    state.live.remove(&scope);
                }
            }
        }
    }

    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        std::mem::take(&mut self.state.lock().expect("fake radio state").close_failures)
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeRadio, FaultOp, RadioBoundary, RadioEvent};

    #[tokio::test]
    async fn injected_faults_carry_contract_identities() {
        let radio = FakeRadio::new();
        radio.fail_next(FaultOp::StartScan, "os denied");
        let error = radio
            .start_scan(super::ScanFilterSpec::default())
            .await
            .expect_err("scripted scan failure");
        assert_eq!(error.code_str(), "scan.start-failed");
        assert_eq!(radio.calls(), vec!["start_scan".to_owned()]);
        assert!(!radio.scan_active(), "failed start leaves scan inactive");
    }

    #[tokio::test]
    async fn close_releases_all_live_subscriptions() {
        let radio = FakeRadio::new();
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0)
            .await
            .expect("enable 0");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 1, true, 0)
            .await
            .expect("enable 1");
        assert_eq!(radio.live_subscription_count(), 2, "two live CCCDs");
        radio.close().await;
        assert_eq!(
            radio.live_subscription_count(),
            0,
            "teardown releases every live CCCD"
        );
        // Disabling one instance leaves the other live.
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0)
            .await
            .expect("re-enable 0");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 1, true, 0)
            .await
            .expect("re-enable 1");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, false, 0)
            .await
            .expect("disable 0");
        assert_eq!(
            radio.live_subscription_count(),
            1,
            "per-instance disable releases only its own CCCD"
        );
    }

    #[tokio::test]
    async fn f14_close_retains_per_scope_receipts() {
        let radio = FakeRadio::new();
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0)
            .await
            .expect("enable 0");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 1, true, 0)
            .await
            .expect("enable 1");
        // One queued unsubscribe fault fails exactly one scope's release.
        radio.fail_next(FaultOp::Unsubscribe, "os stuck");
        radio.close().await;
        let failures = radio.take_close_failures();
        assert_eq!(failures.len(), 1, "exactly one close receipt");
        assert_eq!(
            failures[0].scope,
            (
                "peer-1".to_owned(),
                "svc".to_owned(),
                0,
                "char".to_owned(),
                0
            ),
            "failed scope is the first in deterministic order"
        );
        assert_eq!(failures[0].detail, "os stuck");
        assert_eq!(
            radio.live_subscription_count(),
            1,
            "failed scope stays live like a surviving OS enablement"
        );
        assert!(
            radio.take_close_failures().is_empty(),
            "receipts drain exactly once"
        );
    }

    #[tokio::test]
    async fn events_replay_in_order_then_close() {
        let radio = FakeRadio::new();
        radio.push_event(RadioEvent::Connected("peer-1".to_owned()));
        radio.push_event(RadioEvent::Disconnected("peer-1".to_owned()));
        assert_eq!(
            radio.next_event().await,
            Some(RadioEvent::Connected("peer-1".to_owned()))
        );
        assert_eq!(
            radio.next_event().await,
            Some(RadioEvent::Disconnected("peer-1".to_owned()))
        );
        radio.close_events();
        assert_eq!(
            radio.next_event().await,
            None,
            "closed source ends the stream"
        );
    }
}
