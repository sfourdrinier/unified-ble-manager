// tool/h10-sim/src/bluer_radio.rs
//! Linux radio backend: `bluer` 0.17.4 directly behind [`PeripheralRadio`].
//!
//! Registers the advertisement shape [`crate::advertisement`] describes
//! (Type `peripheral`, ServiceUUIDs 180D + FEEE, `LocalName`, empty
//! `Includes` — byte-identical to what `ble-peripheral-rust` sends), with
//! two deliberate differences from that crate's BlueZ backend:
//!
//! * every `LEAdvertisement1` property is constructed here, so host-side
//!   bisection (Discoverable, Includes, Duration, intervals) needs no
//!   dependency change — see the Linux section of the README;
//! * the GATT application is registered *before* the advertisement, so a
//!   failed advertisement never leaves services advertised without a server,
//!   and a failure names the stage that rejected it (`serve GATT
//!   application` vs `register advertisement`).
//!
//! Advertising takes one of two paths (`--linux-advertising`, see
//! [`crate::linux_advertising`]): an `LEAdvertisement1` object through
//! bluetoothd (default), or — explicit opt-in — an instance the sim adds on
//! the kernel's MGMT control channel ([`super::mgmt_socket`]) while the GATT
//! application stays on bluetoothd.
//!
//! Read/write/notify plumbing mirrors the `ble-peripheral-rust` BlueZ
//! backend against the same `bluer` version: callback funs bridge into the
//! simulator loop over channels, addressed notification sessions are tracked
//! per characteristic and peer, and refusals answer `NotSupported` exactly as
//! before.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use bluer::{
    adv::{Advertisement, AdvertisementHandle, Type as AdvertisementType},
    gatt::{
        local::{
            characteristic_control, service_control, Application, ApplicationHandle,
            Characteristic, CharacteristicControl, CharacteristicControlEvent,
            CharacteristicControlHandle, CharacteristicNotify, CharacteristicNotifyMethod,
            CharacteristicRead, CharacteristicReadRequest, CharacteristicWrite,
            CharacteristicWriteMethod, CharacteristicWriteRequest, ReqError, Service,
        },
        CharacteristicWriter,
    },
    Adapter, AdapterEvent, AdapterProperty,
};
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, oneshot, Notify};
use uuid::Uuid;

use super::mgmt_socket::MgmtAdvertiser;
use crate::daemon_lifetime::DaemonLifetime;
use crate::linux_advertising::{self, AliasRecord, BluezRegistrationFailure};
use crate::radio::{
    short_of, CharPermission, CharProperty, CharSpec, DisconnectReport, GattClientSet,
    OwnedNotifyTarget, PeripheralRadio, QueuedSend, RadioError, RadioEvent, RadioReadAnswer,
    SendOutcome, SendQueue, ServiceSpec, SubscriptionLedger, SEND_QUEUE_CAPACITY,
    SKIP_NOT_CONNECTED,
};

fn backend_error(stage: &str, error: bluer::Error) -> RadioError {
    RadioError(format!("bluer {stage}: {error}"))
}

/// Subscription session handler: characteristic UUIDs plus its notify stream.
struct CharNotifyHandler {
    service_uuid: Uuid,
    characteristic_uuid: Uuid,
    control: CharacteristicControl,
}

/// The notification transports. IO is used for notify-only streams and for the
/// PMD control point, whose acquired socket carries the peer identity and the
/// one-byte BlueZ indication confirmation back to the owner.
enum NotificationWriter {
    Io(Arc<CharacteristicWriter>),
    OwnedIndication {
        writer: Arc<CharacteristicWriter>,
        endpoint: Arc<IndicationEndpoint>,
    },
}

const INDICATION_ID_START: u64 = 1 << 63;

struct IndicationIdAllocator {
    next: AtomicU64,
    exhausted: AtomicBool,
}

impl IndicationIdAllocator {
    fn new() -> Self {
        Self::with_next(INDICATION_ID_START)
    }

    fn with_next(next: u64) -> Self {
        Self {
            next: AtomicU64::new(next),
            exhausted: AtomicBool::new(false),
        }
    }

    fn allocate(&self) -> Option<u64> {
        if self.exhausted.load(Ordering::Acquire) {
            return None;
        }
        loop {
            let current = self.next.load(Ordering::Relaxed);
            if current < INDICATION_ID_START {
                self.exhausted.store(true, Ordering::Release);
                return None;
            }
            if current == u64::MAX {
                if self
                    .next
                    .compare_exchange(current, 0, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    self.exhausted.store(true, Ordering::Release);
                    return Some(current);
                }
                continue;
            }
            if self
                .next
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(current);
            }
        }
    }
}

struct IndicationEndpoint {
    tx: mpsc::Sender<QueuedSend>,
    ids: Arc<IndicationIdAllocator>,
    receipts: IndicationReceipts,
    state: Mutex<bool>,
    closed: Notify,
    confirmations: Mutex<u64>,
    confirmation_ready: Notify,
}

impl IndicationEndpoint {
    #[cfg(test)]
    fn new(tx: mpsc::Sender<QueuedSend>) -> Self {
        Self::with_allocator(
            tx,
            Arc::new(IndicationIdAllocator::new()),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    fn with_allocator(
        tx: mpsc::Sender<QueuedSend>,
        ids: Arc<IndicationIdAllocator>,
        receipts: IndicationReceipts,
    ) -> Self {
        Self {
            tx,
            ids,
            receipts,
            state: Mutex::new(false),
            closed: Notify::new(),
            confirmations: Mutex::new(0),
            confirmation_ready: Notify::new(),
        }
    }

    fn enqueue(&self, mut send: QueuedSend) -> Result<u64, QueuedSend> {
        let closed = match self.state.lock() {
            Ok(closed) => closed,
            Err(_) => return Err(send),
        };
        if *closed {
            return Err(send);
        }
        let Some(generation) = send.indication_generation else {
            return Err(send);
        };
        let Some(id) = self.ids.allocate() else {
            return Err(send);
        };
        send.id = id;
        let receipt = IndicationReceipt {
            service: send.service.clone(),
            characteristic: send.characteristic.to_string(),
            generation,
            stage: "queued",
        };
        if let Ok(mut receipts) = self.receipts.lock() {
            receipts.insert(id, receipt);
        } else {
            return Err(send);
        }
        match self.tx.try_send(send) {
            Ok(()) => Ok(id),
            Err(
                mpsc::error::TrySendError::Full(send) | mpsc::error::TrySendError::Closed(send),
            ) => {
                remove_indication_receipt(&self.receipts, id);
                Err(send)
            }
        }
    }

    fn close(&self) {
        if let Ok(mut closed) = self.state.lock() {
            *closed = true;
        }
        self.closed.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.state.lock().map(|closed| *closed).unwrap_or(true)
    }

    async fn closed(&self) {
        loop {
            if self.state.lock().map(|closed| *closed).unwrap_or(true) {
                return;
            }
            let notified = self.closed.notified();
            if self.state.lock().map(|closed| *closed).unwrap_or(true) {
                return;
            }
            notified.await;
        }
    }

    fn confirm(&self) {
        if let Ok(mut confirmations) = self.confirmations.lock() {
            *confirmations = confirmations.saturating_add(1);
        }
        self.confirmation_ready.notify_one();
    }

    async fn wait_confirmation(&self, timeout: std::time::Duration) -> Result<(), &'static str> {
        let wait = async {
            loop {
                if self.state.lock().map(|closed| *closed).unwrap_or(true) {
                    return Err("indication lane closed before confirmation");
                }
                if let Ok(mut confirmations) = self.confirmations.lock() {
                    if *confirmations > 0 {
                        *confirmations -= 1;
                        return Ok(());
                    }
                } else {
                    return Err("indication confirmation lock poisoned");
                }
                let notified = self.confirmation_ready.notified();
                if self.state.lock().map(|closed| *closed).unwrap_or(true) {
                    return Err("indication lane closed before confirmation");
                }
                notified.await;
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err("indication confirmation timed out"),
        }
    }
}

#[derive(Debug, Clone)]
struct IndicationReceipt {
    service: String,
    characteristic: String,
    generation: u64,
    stage: &'static str,
}

type WriterKey = (Uuid, Option<String>);
type WriterMap = Arc<Mutex<HashMap<WriterKey, RegisteredWriter>>>;
type IndicationReceipts = Arc<Mutex<HashMap<u64, IndicationReceipt>>>;
type SessionTasks = Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>;
/// Bound a blocked BlueZ peer so the ordered pump can advance to other peers.
const PEER_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
const INDICATION_CONFIRMATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
struct RegisteredWriter {
    generation: u64,
    writer: Arc<NotificationWriter>,
}

fn mark_indication_stage(receipts: &IndicationReceipts, id: u64, stage: &'static str) {
    if let Ok(mut receipts) = receipts.lock() {
        if let Some(receipt) = receipts.get_mut(&id) {
            receipt.stage = stage;
        }
    }
}

fn remove_indication_receipt(receipts: &IndicationReceipts, id: u64) -> Option<IndicationReceipt> {
    receipts.lock().ok()?.remove(&id)
}

/// The adapter alias this run replaced, with its record: restored on stop,
/// and adopted by the next start when this run dies without restoring.
struct AdapterAliasClaim {
    /// The alias to restore (`None`: the alias already was the sim name).
    previous: Option<String>,
    /// The alias while running: always the advertised name.
    current: String,
    /// Where `previous` is recorded for the next start.
    record: PathBuf,
}

/// [`PeripheralRadio`] implemented with `bluer` on Linux.
pub struct BluerRadio {
    daemon: DaemonWatch,
    pump_task: tokio::task::JoinHandle<()>,
    adapter: Adapter,
    services: Vec<ServiceSpec>,
    adv_handle: Option<AdvertisementHandle>,
    app_handle: Option<ApplicationHandle>,
    events: mpsc::Sender<RadioEvent>,
    writers: WriterMap,
    /// Generation-aware subscription registry: readiness is announced only
    /// after each addressed writer is installed, and stale session cleanup can
    /// never remove a newer peer session's writer.
    ledger: Arc<Mutex<SubscriptionLedger>>,
    /// Bounded ordered send pump: `notify` pushes here and reports
    /// `Queued`; the pump delivers FIFO and settles each send with a
    /// `NotifySettled` event. A full queue fails the send loudly instead of
    /// growing memory or reordering frames.
    pump_queue: Arc<Mutex<SendQueue>>,
    /// Wakes the pump when a send is queued.
    pump_wake: Arc<Notify>,
    indication_ids: Arc<IndicationIdAllocator>,
    indication_receipts: IndicationReceipts,
    session_tasks: SessionTasks,
    /// Staged manufacturer data (None = not configured or empty payload).
    mfr: Option<(u16, Vec<u8>)>,
    /// `Some` in `--linux-advertising mgmt-legacy`: the advertisement is the
    /// sim's own MGMT instance instead of an `LEAdvertisement1` object.
    mgmt: Option<MgmtAdvertiser>,
    /// The adapter alias claim held while advertising (`None` when no claim
    /// is held: never started, already stopped, or the alias already was
    /// the advertised name).
    adapter_alias: Option<AdapterAliasClaim>,
    /// Set when bluetoothd failed with the known BlueZ/kernel mismatch.
    advertising_unavailable: bool,
    /// Centrals that interacted with this peripheral's GATT application
    /// during the current connection: every read, write and
    /// notify-subscribe carries the central's device address, so drop-link
    /// disconnects exactly the sim's own clients. Entries are removed when
    /// observed disconnected — never immortal.
    gatt_clients: Arc<Mutex<GattClientSet>>,
    _drop_tx: oneshot::Sender<()>,
}

struct DaemonWatch {
    lifetime: Arc<DaemonLifetime>,
    connection: Arc<dbus::nonblock::SyncConnection>,
    _subscription: dbus::nonblock::MsgMatch,
    worker: tokio::task::JoinHandle<()>,
}

impl Drop for DaemonWatch {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl DaemonWatch {
    async fn owner(connection: &Arc<dbus::nonblock::SyncConnection>) -> Result<String, RadioError> {
        let proxy = dbus::nonblock::Proxy::new(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            std::time::Duration::from_secs(2),
            connection.clone(),
        );
        let (owner,): (String,) = proxy
            .method_call("org.freedesktop.DBus", "GetNameOwner", ("org.bluez",))
            .await
            .map_err(|error| RadioError(format!("BlueZ owner query: {error}")))?;
        Ok(owner)
    }

    async fn open() -> Result<Self, RadioError> {
        Self::open_on_bus(dbus::channel::BusType::System).await
    }

    async fn open_on_bus(bus: dbus::channel::BusType) -> Result<Self, RadioError> {
        let (resource, connection) =
            dbus_tokio::connection::new::<dbus::nonblock::SyncConnection>(bus)
                .map_err(|error| RadioError(format!("BlueZ lifetime connection: {error}")))?;
        let lifetime = Arc::new(DaemonLifetime::default());
        let failure = lifetime.clone();
        let worker = tokio::spawn(async move {
            let error = resource.await;
            failure.owner_changed();
            eprintln!("h10-sim: BlueZ lifetime connection ended: {error}");
        });
        let state = lifetime.clone();
        let subscribed = connection
            .add_match(
                dbus::message::MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged")
                    .with_sender("org.freedesktop.DBus"),
            )
            .await;
        let subscription = match subscribed {
            Ok(value) => value.cb(move |_, (name, _old, _new): (String, String, String)| {
                if name == "org.bluez" {
                    state.owner_changed();
                }
                true
            }),
            Err(error) => {
                worker.abort();
                return Err(RadioError(format!("BlueZ lifetime watch: {error}")));
            }
        };
        let watch = Self {
            lifetime,
            connection,
            _subscription: subscription,
            worker,
        };
        let owner = Self::owner(&watch.connection).await?;
        watch.lifetime.bind(&owner).map_err(RadioError)?;
        Ok(watch)
    }

    async fn verify(&self) -> Result<(), RadioError> {
        self.lifetime.admit().map_err(RadioError)?;
        match Self::owner(&self.connection).await {
            Ok(owner) => self.lifetime.check(&owner).map_err(RadioError),
            Err(error) => {
                self.lifetime.owner_changed();
                Err(error)
            }
        }
    }
}

/// Records one GATT interaction's central address in the client set. A
/// malformed address or a dead lock is loud on stderr, never a silent skip.
fn note_client(clients: &Arc<Mutex<GattClientSet>>, address: bluer::Address) {
    let text = address.to_string();
    match clients.lock() {
        Ok(mut set) => {
            if !set.insert(&text) {
                eprintln!("h10-sim: ignoring malformed GATT client address {text:?}");
            }
        }
        Err(error) => eprintln!("h10-sim: GATT client tracker lock: {error}"),
    }
}

fn writer_peer(writer: &NotificationWriter) -> Option<String> {
    match writer {
        NotificationWriter::Io(writer) => {
            Some(writer.device_address().to_string().to_ascii_uppercase())
        }
        NotificationWriter::OwnedIndication { writer, .. } => {
            Some(writer.device_address().to_string().to_ascii_uppercase())
        }
    }
}

/// Captures the D2H writer owner at the native write callback boundary. The
/// ledger→writers order is shared with subscription install/retirement.
fn owned_target_for_write(
    writers: &WriterMap,
    ledger: &Arc<Mutex<SubscriptionLedger>>,
    characteristic: Uuid,
    peer_address: &str,
) -> Option<OwnedNotifyTarget> {
    let ledger = ledger.lock().ok()?;
    let peer_address = peer_address.to_ascii_uppercase();
    let generation = ledger.current(characteristic, Some(&peer_address))?;
    let writers = writers.lock().ok()?;
    let registration = writers.get(&(characteristic, Some(peer_address.clone())))?;
    (registered_writer_matches_target(
        &OwnedNotifyTarget::new(peer_address.clone(), generation),
        registration.generation,
        writer_peer(registration.writer.as_ref())
            .as_deref()
            .unwrap_or_default(),
    ))
    .then(|| OwnedNotifyTarget::new(peer_address, generation))
}

fn target_matches(target: &OwnedNotifyTarget, peer_address: &str, generation: u64) -> bool {
    target.peer_address == peer_address && target.generation == generation
}

fn registered_writer_matches_target(
    target: &OwnedNotifyTarget,
    writer_generation: u64,
    writer_peer_address: &str,
) -> bool {
    target_matches(target, writer_peer_address, writer_generation)
}

fn redact_peer_addresses(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut redacted = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let address_end = index.saturating_add(17);
        if address_end <= bytes.len()
            && (0..17).all(|offset| {
                if matches!(offset, 2 | 5 | 8 | 11 | 14) {
                    bytes[index + offset] == b':'
                } else {
                    bytes[index + offset].is_ascii_hexdigit()
                }
            })
            && crate::radio::parse_bt_address(&text[index..address_end]).is_ok()
        {
            redacted.push_str("peer");
            index = address_end;
            continue;
        }
        let character = text[index..]
            .chars()
            .next()
            .expect("byte index must remain on a character boundary");
        redacted.push(character);
        index += character.len_utf8();
    }
    redacted
}

fn peer_send_outcome(failures: Vec<String>) -> SendOutcome {
    if failures.is_empty() {
        SendOutcome::OsAccepted
    } else {
        let failures = failures
            .into_iter()
            .map(|failure| redact_peer_addresses(&failure))
            .collect::<Vec<_>>();
        SendOutcome::Failed(format!(
            "one or more peer sends failed: {}",
            failures.join("; ")
        ))
    }
}

async fn attempt_peer_targets<T, F, Fut>(
    targets: Vec<T>,
    timeout: std::time::Duration,
    mut attempt: F,
) -> Vec<(T, Result<(), String>)>
where
    T: Clone,
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        let outcome = match tokio::time::timeout(timeout, attempt(target.clone())).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!("peer send timed out after {timeout:?}")),
        };
        outcomes.push((target, outcome));
    }
    outcomes
}

/// Removes one canonical address from the client set. A dead lock is loud
/// on stderr — the prune is bookkeeping, never silent.
fn prune_client(clients: &Arc<Mutex<GattClientSet>>, canonical: &str) {
    match clients.lock() {
        Ok(mut clients) => {
            clients.remove(canonical);
        }
        Err(error) => eprintln!("h10-sim: GATT client tracker lock: {error}"),
    }
}

#[cfg(test)]
fn retire_failed_sends(
    pump: &tokio::task::JoinHandle<()>,
    writers: &WriterMap,
    queue: &Arc<Mutex<SendQueue>>,
) -> serde_json::Value {
    retire_failed_sends_with_indications(
        pump,
        writers,
        queue,
        &Arc::new(Mutex::new(HashMap::new())),
        &Arc::new(Mutex::new(Vec::new())),
    )
}

fn retire_failed_sends_with_indications(
    pump: &tokio::task::JoinHandle<()>,
    writers: &WriterMap,
    queue: &Arc<Mutex<SendQueue>>,
    indication_receipts: &IndicationReceipts,
    session_tasks: &SessionTasks,
) -> serde_json::Value {
    pump.abort();
    let mut failures = Vec::new();
    let indication_receipts_not_confirmed = match indication_receipts.lock() {
        Ok(receipts) => receipts
            .iter()
            .map(|(id, receipt)| {
                serde_json::json!({
                    "id": id,
                    "service": receipt.service,
                    "characteristic": receipt.characteristic,
                    "generation": receipt.generation,
                    "stage": receipt.stage,
                })
            })
            .collect::<Vec<_>>(),
        Err(_) => {
            failures.push("indication receipt lock poisoned");
            Vec::new()
        }
    };
    let session_task_count = match session_tasks.lock() {
        Ok(mut tasks) => {
            let count = tasks.len();
            for task in tasks.drain(..) {
                task.abort();
            }
            count
        }
        Err(_) => {
            failures.push("session task lock poisoned");
            0
        }
    };
    match writers.lock() {
        Ok(mut writers) => {
            for registration in writers.values() {
                match registration.writer.as_ref() {
                    NotificationWriter::OwnedIndication { endpoint, .. } => endpoint.close(),
                    NotificationWriter::Io(_) => {}
                }
            }
            writers.clear();
        }
        Err(_) => failures.push("writer lock poisoned"),
    }
    let mut queued = Vec::new();
    match queue.lock() {
        Ok(mut queue) => {
            while let Some(send) = queue.pop() {
                queued.push(serde_json::json!({"id": send.id, "bytes": send.value.len(), "characteristic": send.characteristic.to_string()}));
            }
        }
        Err(_) => failures.push("send queue lock poisoned"),
    }
    serde_json::json!({
        "queuedSendsNotConfirmed": queued,
        "inFlightDelivery": "unknown-at-most-one",
        "indicationReceiptsNotConfirmed": indication_receipts_not_confirmed,
        "sessionTasksAborted": session_task_count,
        "taskDrain": "not-awaitable-in-synchronous-retirement",
        "failures": failures
    })
}

#[async_trait]
impl PeripheralRadio for BluerRadio {
    fn fatal_failure(&self) -> Option<String> {
        self.daemon.lifetime.admit().err()
    }

    fn retire_failed_collection(&mut self) -> serde_json::Value {
        self.app_handle = None;
        retire_failed_sends_with_indications(
            &self.pump_task,
            &self.writers,
            &self.pump_queue,
            &self.indication_receipts,
            &self.session_tasks,
        )
    }
    async fn notification_payload_capacity(
        &self,
        characteristic: Uuid,
    ) -> Result<Option<usize>, RadioError> {
        if !self.services.iter().any(|service| {
            service
                .characteristics
                .iter()
                .any(|entry| entry.uuid == characteristic)
        }) {
            return Err(RadioError(format!(
                "unknown notification characteristic {characteristic}"
            )));
        }
        let writers = self
            .writers
            .lock()
            .map_err(|error| RadioError(format!("notification capacity writer lock: {error}")))?;
        // bluer already applies its payload safety adjustment; do not subtract
        // the ATT header a second time.
        let capacities = writers
            .iter()
            .filter(|((uuid, _), _)| *uuid == characteristic)
            .map(|(_, registration)| match registration.writer.as_ref() {
                NotificationWriter::Io(writer) => Some(writer.mtu()),
                NotificationWriter::OwnedIndication { writer, .. } => Some(writer.mtu()),
            })
            .collect::<Option<Vec<_>>>();
        Ok(capacities.and_then(|capacities| capacities.into_iter().min()))
    }

    async fn open(events: mpsc::Sender<RadioEvent>) -> Result<Self, RadioError> {
        Self::open_with_adapter(events, None).await
    }

    async fn open_with_adapter(
        events: mpsc::Sender<RadioEvent>,
        requested: Option<&str>,
    ) -> Result<Self, RadioError> {
        let daemon = DaemonWatch::open().await?;
        let session = bluer::Session::new()
            .await
            .map_err(|error| backend_error("open session", error))?;
        let adapter = if requested.is_some() {
            let names = session
                .adapter_names()
                .await
                .map_err(|error| backend_error("list adapters", error))?;
            let selected = linux_advertising::select_requested_adapter(requested, &names)
                .map_err(RadioError)?;
            match selected {
                Some(name) => session
                    .adapter(&name)
                    .map_err(|error| backend_error("open requested adapter", error))?,
                None => {
                    return Err(RadioError(
                        "explicit adapter selection lost its requested identity".to_string(),
                    ))
                }
            }
        } else {
            session
                .default_adapter()
                .await
                .map_err(|error| backend_error("open adapter", error))?
        };
        adapter
            .set_powered(true)
            .await
            .map_err(|error| backend_error("power on", error))?;

        let (drop_tx, drop_rx) = oneshot::channel::<()>();
        // Power changes are observed for the radio's lifetime. Other adapter
        // events (devices appearing, other properties) are skipped, never a
        // reason to stop watching.
        let mut adapter_stream = adapter
            .events()
            .await
            .map_err(|error| backend_error("watch adapter events", error))?;
        let sender = events.clone();
        tokio::spawn(async move {
            let stream_future = async {
                while let Some(event) = adapter_stream.next().await {
                    if let AdapterEvent::PropertyChanged(AdapterProperty::Powered(powered)) = event
                    {
                        if sender.send(RadioEvent::Powered(powered)).await.is_err() {
                            break;
                        }
                    }
                }
            };
            tokio::select! {
                _ = stream_future => {},
                _ = drop_rx => {}
            }
        });

        let writers = Arc::new(Mutex::new(HashMap::new()));
        let ledger = Arc::new(Mutex::new(SubscriptionLedger::new()));
        let pump_queue = Arc::new(Mutex::new(SendQueue::new(SEND_QUEUE_CAPACITY)));
        let pump_wake = Arc::new(Notify::new());
        let indication_ids = Arc::new(IndicationIdAllocator::new());
        let indication_receipts = Arc::new(Mutex::new(HashMap::new()));
        let session_tasks = Arc::new(Mutex::new(Vec::new()));
        daemon.verify().await?;
        let pump_task = tokio::spawn(send_pump(
            pump_queue.clone(),
            pump_wake.clone(),
            writers.clone(),
            ledger.clone(),
            events.clone(),
        ));

        Ok(Self {
            daemon,
            pump_task,
            adapter,
            services: Vec::new(),
            adv_handle: None,
            app_handle: None,
            events,
            writers,
            ledger,
            pump_queue,
            pump_wake,
            indication_ids,
            indication_receipts,
            session_tasks,
            mfr: None,
            mgmt: None,
            adapter_alias: None,
            advertising_unavailable: false,
            gatt_clients: Arc::new(Mutex::new(GattClientSet::new())),
            _drop_tx: drop_tx,
        })
    }

    async fn is_powered(&mut self) -> Result<bool, RadioError> {
        self.adapter
            .is_powered()
            .await
            .map_err(|error| backend_error("is powered", error))
    }

    async fn is_advertising(&mut self) -> Result<bool, RadioError> {
        self.daemon.lifetime.admit().map_err(RadioError)?;
        if let Some(mgmt) = self.mgmt.as_mut() {
            let active = tokio::task::block_in_place(|| mgmt.is_active()).map_err(RadioError)?;
            return Ok(active && self.app_handle.is_some());
        }
        let instances = self
            .adapter
            .active_advertising_instances()
            .await
            .map_err(|error| backend_error("is advertising", error))?;
        Ok(instances > 0 && self.adv_handle.is_some())
    }

    async fn start_advertising(&mut self, name: &str, uuids: &[Uuid]) -> Result<(), RadioError> {
        self.daemon.verify().await?;
        // GATT application first — but only once: re-registering an identical
        // application makes BlueZ emit Service Changed to connected centrals,
        // and a real H10 never re-registers on re-advertise. The declarations
        // are fixed at startup (`add_service` refuses late additions), so a
        // live application is always current.
        let had_app = self.app_handle.is_some();
        if !had_app {
            // The name already fits the scan-response budget — the caller
            // enforces it via `crate::advertisement::fit_name`.
            let (handlers, services) = build_services_with_state(
                self.services.clone(),
                self.events.clone(),
                self.gatt_clients.clone(),
                self.writers.clone(),
                self.ledger.clone(),
            )?;
            let app_handle = self
                .adapter
                .serve_gatt_application(Application {
                    services,
                    ..Default::default()
                })
                .await
                .map_err(|error| backend_error("serve GATT application", error))?;
            self.setup_char_handlers(handlers);
            self.app_handle = Some(app_handle);
        }

        self.daemon.verify().await?;

        if let Some(mgmt) = self.mgmt.as_mut() {
            let uuids16 = uuids
                .iter()
                .map(|uuid| {
                    short_of(uuid).ok_or_else(|| {
                        RadioError(format!("advertised UUID {uuid} is not a 16-bit UUID"))
                    })
                })
                .collect::<Result<Vec<u16>, RadioError>>()?;
            let mfr = self
                .mfr
                .as_ref()
                .map(|(company, payload)| (*company, payload.as_slice()));
            // A failed add after a fresh registration drops the application
            // again: never a half-registered peripheral.
            match tokio::task::block_in_place(|| mgmt.start(name, &uuids16, mfr)) {
                Ok(outcome) => {
                    if let Some(reason) = outcome.fallback_reason {
                        eprintln!("h10-sim: {reason}");
                    }
                }
                Err(error) => {
                    if !had_app {
                        self.app_handle = None;
                    }
                    return Err(RadioError(format!(
                        "mgmt-legacy add advertisement: {error}"
                    )));
                }
            }
            if let Err(error) = self.claim_adapter_alias(name).await {
                // The advertisement is up but the GAP name is wrong: take it
                // back down rather than impersonate half the strap.
                if let Some(mgmt) = self.mgmt.as_mut() {
                    if let Err(remove) = tokio::task::block_in_place(|| mgmt.stop()) {
                        eprintln!(
                            "h10-sim: alias claim failed ({error}); removing the advertisement failed too: {remove}"
                        );
                    }
                }
                return Err(error);
            }
            return self.daemon.verify().await;
        }

        let instances_before = self.advertising_instances().await;
        match self
            .adapter
            .advertise(h10_advertisement(name, uuids, self.mfr.as_ref()))
            .await
        {
            Ok(adv_handle) => {
                self.adv_handle = Some(adv_handle);
                if let Err(error) = self.claim_adapter_alias(name).await {
                    self.adv_handle = None;
                    return Err(error);
                }
                self.daemon.verify().await
            }
            Err(error) => {
                // Never leave a half-registered peripheral behind after a
                // fresh registration: dropping the handle unregisters the
                // GATT application again.
                if !had_app {
                    self.app_handle = None;
                }
                let diagnosis =
                    linux_advertising::diagnose_bluez_registration(&BluezRegistrationFailure {
                        failed_kind: matches!(error.kind, bluer::ErrorKind::Failed),
                        message: &error.message,
                        instances_before: &instances_before,
                        kernel_release: &kernel_release(),
                    });
                self.advertising_unavailable = diagnosis.known_mismatch;
                Err(RadioError(format!(
                    "bluer register advertisement: {}",
                    diagnosis.text
                )))
            }
        }
    }

    async fn stop_advertising(&mut self) -> Result<(), RadioError> {
        // Only the advertisement goes away: the GATT application stays
        // registered, so connected centrals see no Service Changed and the
        // next advertise needs no re-registration. Process exit unregisters
        // the application with the D-Bus connection.
        let removed = match self.mgmt.as_mut() {
            Some(mgmt) => tokio::task::block_in_place(|| mgmt.stop()).map(|_| ()),
            None => Ok(()),
        };
        self.adv_handle = None;
        removed
            .map_err(|error| RadioError(format!("mgmt-legacy remove advertisement: {error}")))?;
        self.restore_adapter_alias().await
    }

    async fn add_service(&mut self, service: &ServiceSpec) -> Result<(), RadioError> {
        // Declarations are fixed at startup: a live application would never
        // see a late addition, so it is refused loudly instead of queued
        // into a config nobody reads.
        if self.app_handle.is_some() {
            return Err(RadioError(format!(
                "service {} added after the GATT application was registered",
                service.uuid
            )));
        }
        self.services.push(service.clone());
        Ok(())
    }

    async fn set_adv_manufacturer_data(
        &mut self,
        company: u16,
        data: Vec<u8>,
    ) -> Result<(), RadioError> {
        // An empty payload stages nothing: a company id with no payload bytes
        // claims more than the captures pin down, so it stays off the air.
        self.mfr = if data.is_empty() {
            None
        } else {
            Some((company, data))
        };
        Ok(())
    }

    fn supports_manufacturer_data(&self) -> bool {
        true
    }

    fn simulator_clients(&self) -> Vec<String> {
        match self.gatt_clients.lock() {
            Ok(set) => set.snapshot(),
            Err(error) => {
                eprintln!("h10-sim: GATT client tracker lock: {error}");
                Vec::new()
            }
        }
    }

    async fn connected_devices(&mut self) -> Result<Vec<String>, RadioError> {
        let addresses = self
            .adapter
            .device_addresses()
            .await
            .map_err(|error| backend_error("list devices", error))?;
        let mut connected = Vec::new();
        for address in addresses {
            let device = match self.adapter.device(address) {
                Ok(device) => device,
                Err(error) => {
                    eprintln!(
                        "h10-sim: connected-devices {address}: cannot open device: {error:?}"
                    );
                    continue;
                }
            };
            match device.is_connected().await {
                Ok(true) => match crate::radio::normalize_bt_address(&address.to_string()) {
                    Ok(canonical) => connected.push(canonical),
                    Err(error) => {
                        eprintln!("h10-sim: connected-devices {address}: {error}");
                    }
                },
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "h10-sim: connected-devices {address}: is-connected failed: {error:?}"
                    );
                }
            }
        }
        connected.sort();
        Ok(connected)
    }

    async fn disconnect_centrals(
        &mut self,
        targets: &[String],
    ) -> Result<DisconnectReport, RadioError> {
        // Only targets are ever touched: tracked simulator clients plus the
        // explicit allowlist, unioned by the caller. The adapter's other
        // devices — phones, laptops, anything paired to the host — are never
        // disturbed. Genuine disconnects: Device1.Disconnect on each target,
        // using the caller's own D-Bus session (no escalation — the same
        // access that registered the advertisement). Every address resolves
        // independently: one stale record can never abort the batch, and a
        // failure lands in the report as well as on stderr, never silent.
        let known = self
            .adapter
            .device_addresses()
            .await
            .map_err(|error| backend_error("list devices", error))?;
        let mut report = DisconnectReport::new();
        for target in targets {
            let canonical = match crate::radio::normalize_bt_address(target) {
                Ok(canonical) => canonical,
                Err(error) => {
                    // Targets leave here canonical, so a malformed entry is
                    // a caller bug: loud skip, never a disconnect.
                    eprintln!("h10-sim: drop-link target ignored: {error}");
                    report.skip(target.clone(), format!("invalid address: {error}"));
                    continue;
                }
            };
            let octets = match crate::radio::parse_bt_address(&canonical) {
                Ok(octets) => octets,
                Err(error) => {
                    eprintln!("h10-sim: drop-link target ignored: {error}");
                    report.skip(canonical, format!("invalid address: {error}"));
                    continue;
                }
            };
            let address = bluer::Address::new(octets);
            if !known.contains(&address) {
                // The adapter doesn't know this address at all: nothing
                // behind it can be connected through this adapter, so the
                // client record is pruned and the address is reported
                // skipped — a rotated phone address lands here after it
                // disconnects, instead of poisoning the next drop.
                prune_client(&self.gatt_clients, &canonical);
                report.skip(canonical, SKIP_NOT_CONNECTED.to_string());
                continue;
            }
            let device = match self.adapter.device(address) {
                Ok(device) => device,
                Err(error) => {
                    let message = format!("cannot open device: {error:?}");
                    eprintln!("h10-sim: disconnect {canonical}: {message}");
                    report.skip_failed(canonical, message);
                    continue;
                }
            };
            match device.is_connected().await {
                Ok(false) => {
                    // Observed disconnect: the client record goes with it,
                    // so a device that left on its own is pruned, never
                    // immortal.
                    prune_client(&self.gatt_clients, &canonical);
                    report.skip(canonical, SKIP_NOT_CONNECTED.to_string());
                }
                Ok(true) => match device.disconnect().await {
                    Ok(()) => {
                        prune_client(&self.gatt_clients, &canonical);
                        report.add_dropped(canonical);
                    }
                    Err(error) => {
                        let message = format!("{error:?}");
                        eprintln!("h10-sim: disconnect {canonical} failed: {error:?}");
                        report.skip_failed(canonical, message);
                    }
                },
                Err(error) => {
                    // Genuinely unknown: the record is kept and the skip is
                    // loud. The next drop re-resolves it — pruning happens
                    // only on observed absence, never on a failed read.
                    let message = format!("is-connected failed: {error:?}");
                    eprintln!("h10-sim: disconnect {canonical}: {message}");
                    report.skip_failed(canonical, message);
                }
            }
        }
        Ok(report)
    }

    async fn drop_subscription(&mut self, characteristic: Uuid) -> Result<bool, RadioError> {
        // Tear down every live session for this characteristic (adversarial
        // interrupted-setup). Each peer generation is retired independently,
        // so a late callback cannot remove a later subscription.
        drop_current_subscriptions(
            &self.events,
            &self.ledger,
            &self.writers,
            &self.service_of(characteristic),
            characteristic,
        )
        .await
    }

    async fn notify(
        &mut self,
        characteristic: Uuid,
        value: Vec<u8>,
    ) -> Result<SendOutcome, RadioError> {
        self.notify_inner(characteristic, value, None).await
    }

    async fn notify_to(
        &mut self,
        characteristic: Uuid,
        value: Vec<u8>,
        target: OwnedNotifyTarget,
    ) -> Result<SendOutcome, RadioError> {
        self.notify_inner(characteristic, value, Some(target)).await
    }
    fn advertising_detail(&self) -> Option<serde_json::Value> {
        let mut detail = match &self.mgmt {
            Some(mgmt) => mgmt.detail(),
            None => serde_json::json!({"backend": "bluez"}),
        };
        if let (Some(claim), Some(object)) = (&self.adapter_alias, detail.as_object_mut()) {
            object.insert(
                "adapterAlias".to_string(),
                serde_json::json!({"previous": claim.previous, "current": claim.current}),
            );
        }
        Some(detail)
    }

    fn advertising_unavailable(&self) -> bool {
        self.advertising_unavailable
    }
}

impl BluerRadio {
    async fn notify_inner(
        &mut self,
        characteristic: Uuid,
        value: Vec<u8>,
        target: Option<OwnedNotifyTarget>,
    ) -> Result<SendOutcome, RadioError> {
        self.daemon.lifetime.admit().map_err(RadioError)?;
        let (outcome, wake_pump) = {
            let ledger = self
                .ledger
                .lock()
                .map_err(|error| RadioError(format!("bluer notify lock: {error}")))?;
            if !ledger.is_subscribed(characteristic) {
                return Ok(SendOutcome::NotSubscribed);
            }
            let writers = self
                .writers
                .lock()
                .map_err(|error| RadioError(format!("bluer writer lock: {error}")))?;
            if let Some(target) = target {
                let selected = match ledger.select_targets(characteristic, Some(&target)) {
                    Ok(selected) => selected,
                    Err(reason) => return Ok(SendOutcome::Failed(reason)),
                };
                let target = selected
                    .into_iter()
                    .next()
                    .ok_or_else(|| RadioError("targeted notify selected no peer".to_string()))?;
                let writer = match writers
                    .get(&(characteristic, Some(target.peer_address.clone())))
                    .cloned()
                {
                    Some(registration) if registration.generation == target.generation => {
                        registration.writer
                    }
                    Some(_) => {
                        return Ok(SendOutcome::Failed(
                            "owned notification writer generation is stale".to_string(),
                        ))
                    }
                    None => {
                        return Ok(SendOutcome::Failed(
                            "owned notification writer unavailable".to_string(),
                        ))
                    }
                };
                match writer.as_ref() {
                    NotificationWriter::OwnedIndication { endpoint, .. } => {
                        let generation = target.generation;
                        let send = QueuedSend {
                            id: 0,
                            service: self.service_of(characteristic),
                            characteristic,
                            targets: vec![target],
                            indication_generation: Some(generation),
                            value,
                        };
                        (
                            match endpoint.enqueue(send) {
                                Ok(id) => SendOutcome::Queued { id },
                                Err(_) => SendOutcome::Failed(format!(
                                    "bluer indication {characteristic}: bounded lane full or closed; the value was not admitted"
                                )),
                            },
                            false,
                        )
                    }
                    NotificationWriter::Io(_) => {
                        let send = QueuedSend {
                            id: 0,
                            service: self.service_of(characteristic),
                            characteristic,
                            targets: vec![target],
                            indication_generation: None,
                            value,
                        };
                        let queued = self
                            .pump_queue
                            .lock()
                            .map_err(|error| RadioError(format!("bluer pump lock: {error}")))?
                            .push(send);
                        let admitted = queued.is_ok();
                        (
                            match queued {
                                Ok(id) => SendOutcome::Queued { id },
                                Err(_) => SendOutcome::Failed(format!(
                                    "bluer notify {characteristic}: send queue full ({SEND_QUEUE_CAPACITY}) or correlation IDs exhausted; the value was not admitted"
                                )),
                            },
                            admitted,
                        )
                    }
                }
            } else {
                let targets = ledger
                    .select_targets(characteristic, None)
                    .map_err(RadioError)?;
                if targets.is_empty() {
                    return Ok(SendOutcome::NotSubscribed);
                }
                if targets.iter().any(|target| {
                    !writers.contains_key(&(characteristic, Some(target.peer_address.clone())))
                }) {
                    return Ok(SendOutcome::Failed(
                        "one or more subscribed notification writers are unavailable".to_string(),
                    ));
                }
                let send = QueuedSend {
                    id: 0,
                    service: self.service_of(characteristic),
                    characteristic,
                    targets,
                    indication_generation: None,
                    value,
                };
                let queued = self
                    .pump_queue
                    .lock()
                    .map_err(|error| RadioError(format!("bluer pump lock: {error}")))?
                    .push(send);
                let admitted = queued.is_ok();
                (
                    match queued {
                        Ok(id) => SendOutcome::Queued { id },
                        Err(_) => SendOutcome::Failed(format!(
                            "bluer notify {characteristic}: send queue full ({SEND_QUEUE_CAPACITY}) or correlation IDs exhausted; the value was not admitted"
                        )),
                    },
                    admitted,
                )
            }
        };
        if wake_pump {
            self.pump_wake.notify_one();
        }
        Ok(outcome)
    }
}

/// What a readable notify-session fd turned out to be. BlueZ makes the
/// `AcquireNotify` fd readable both when the central confirms an indication
/// and when the session really ends — only the second drops the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFate {
    /// An indication confirmation arrived: the session stays up.
    Confirmed,
    /// The session ended: the writer must go.
    Ended,
    /// Readiness fired with nothing to read: keep waiting.
    Spurious,
}

/// Classifies one non-blocking drain of a readable notify-session fd:
/// confirmation bytes keep the session, an empty read or a socket error ends
/// it, `WouldBlock` is a spurious wakeup. Pure so it is unit-testable without
/// a radio.
fn classify_session_drain(result: std::io::Result<usize>) -> SessionFate {
    match result {
        Ok(n) if n > 0 => SessionFate::Confirmed,
        Ok(_) => SessionFate::Ended,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => SessionFate::Spurious,
        Err(_) => SessionFate::Ended,
    }
}

/// Drains one message from a readable notify-session fd without blocking.
/// `bluer` never reads this fd itself, so a raw `recv` here races with
/// nothing; `MSG_DONTWAIT` keeps a spurious wakeup from ever blocking the
/// session task.
fn drain_session_fd(writer: &impl std::os::fd::AsRawFd) -> std::io::Result<usize> {
    let mut buf = [0u8; 64];
    let ret = unsafe {
        libc::recv(
            writer.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

/// Own a separate readiness registration for session reads. Calling bluer's
/// `closed()` and then reading through libc does not clear its Tokio readiness
/// cache: after an indication confirmation that combination spins forever.
fn session_readiness(
    socket: &impl std::os::fd::AsRawFd,
) -> std::io::Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let duplicate = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The fd is newly owned; AsyncFd drops it even when registration fails.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) };
    tokio::io::unix::AsyncFd::new(owned)
}

async fn next_session_fate(
    readiness: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
) -> std::io::Result<SessionFate> {
    loop {
        let mut guard = readiness.readable().await?;
        // try_io consumes/clears readiness on WouldBlock; the next iteration
        // genuinely waits for a fresh kernel event rather than hot-spinning.
        match guard.try_io(|fd| drain_session_fd(fd.get_ref())) {
            Ok(result) => return result.map(|count| classify_session_drain(Ok(count))),
            Err(_) => continue,
        }
    }
}

/// `uname -r`, or why it could not be read — for failure diagnoses.
fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_string())
        .unwrap_or_else(|error| format!("unknown (/proc/sys/kernel/osrelease: {error})"))
}

impl BluerRadio {
    /// Switches advertising to the sim's own MGMT instance
    /// (`--linux-advertising mgmt-legacy`) on this adapter's controller.
    /// Fails, naming the `setcap` command, without `CAP_NET_ADMIN`; removes
    /// an instance a previous run left behind. Returns what it found.
    pub fn use_mgmt_legacy(&mut self) -> Result<serde_json::Value, RadioError> {
        let index = linux_advertising::hci_index(self.adapter.name()).map_err(RadioError)?;
        let (advertiser, found) =
            tokio::task::block_in_place(|| MgmtAdvertiser::open(index)).map_err(RadioError)?;
        self.mgmt = Some(advertiser);
        Ok(found)
    }

    /// Sets the adapter alias to the advertised name so a central that
    /// connects reads the strap name — not the host name — from BlueZ's GAP
    /// Device Name characteristic (`btd_adapter_get_name` prefers the stored
    /// alias over the system name, so MGMT Set Local Name alone would not do
    /// it). The replaced alias is recorded for the next start and restored
    /// by [`PeripheralRadio::stop_advertising`], the panic hook, and the next
    /// start's stale-record adoption. Needs only the D-Bus access the radio
    /// already requires; no new privilege.
    async fn claim_adapter_alias(&mut self, name: &str) -> Result<(), RadioError> {
        let index = linux_advertising::hci_index(self.adapter.name()).map_err(RadioError)?;
        let record = linux_advertising::alias_record_path(index);
        let boot_id = linux_advertising::boot_id().map_err(RadioError)?;
        let stale_prev = match std::fs::read_to_string(&record) {
            Ok(text) => match AliasRecord::parse(&text) {
                Ok(found) if found.boot_id == boot_id && found.index == index => {
                    Some(found.prev_alias)
                }
                Ok(_) => {
                    eprintln!(
                        "h10-sim: discarding stale alias record {} from another boot or controller",
                        record.display()
                    );
                    linux_advertising::remove_alias_record(&record);
                    None
                }
                Err(error) => {
                    eprintln!(
                        "h10-sim: discarding unreadable alias record {}: {error}",
                        record.display()
                    );
                    linux_advertising::remove_alias_record(&record);
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(RadioError(format!(
                    "read alias record {}: {error}",
                    record.display()
                )));
            }
        };
        let current = self
            .adapter
            .alias()
            .await
            .map_err(|error| backend_error("read adapter alias", error))?;
        let plan = linux_advertising::plan_adapter_alias(&current, name, stale_prev.as_deref());
        if let Some(previous) = plan.restore_to.clone() {
            std::fs::write(
                &record,
                AliasRecord {
                    boot_id,
                    index,
                    prev_alias: previous.clone(),
                }
                .encode(),
            )
            .map_err(|error| {
                RadioError(format!("write alias record {}: {error}", record.display()))
            })?;
        }
        if current != plan.set_to {
            if let Err(error) = self.adapter.set_alias(plan.set_to.clone()).await {
                linux_advertising::remove_alias_record(&record);
                return Err(backend_error("set adapter alias", error));
            }
        }
        if plan.adopted_stale {
            eprintln!(
                "h10-sim: adapter alias on hci{index} was the leftover {current:?}; \
                 adopted the recorded previous alias"
            );
        }
        eprintln!(
            "h10-sim: adapter alias on hci{index}: {current:?} -> {:?} \
             (restored on stop; record at {})",
            plan.set_to,
            record.display()
        );
        linux_advertising::install_adapter_alias_panic_hook(record.clone());
        self.adapter_alias = Some(AdapterAliasClaim {
            previous: plan.restore_to,
            current: plan.set_to,
            record,
        });
        Ok(())
    }

    /// Restores the alias [`BluerRadio::claim_adapter_alias`] replaced.
    /// A failed restore keeps the claim (and its record) so the next stop —
    /// or the next start's stale-record adoption — retries it; the failure
    /// itself is the returned error, never silent.
    async fn restore_adapter_alias(&mut self) -> Result<(), RadioError> {
        let Some(claim) = self.adapter_alias.take() else {
            return Ok(());
        };
        let Some(previous) = claim.previous.clone() else {
            linux_advertising::remove_alias_record(&claim.record);
            return Ok(());
        };
        match self.adapter.set_alias(previous.clone()).await {
            Ok(()) => {
                eprintln!(
                    "h10-sim: adapter alias restored to {previous:?} (was {:?})",
                    claim.current
                );
                linux_advertising::remove_alias_record(&claim.record);
                Ok(())
            }
            Err(error) => {
                self.adapter_alias = Some(claim);
                Err(RadioError(format!(
                    "restore adapter alias to {previous:?}: {error} \
                     (the alias record is kept; the next stop or start retries the restore)"
                )))
            }
        }
    }

    /// `LEAdvertisingManager1` instance counters, read so a registration
    /// failure carries the adapter state it happened in. A counter that
    /// cannot be read is reported as such, never guessed.
    async fn advertising_instances(&self) -> String {
        let active = self.adapter.active_advertising_instances().await;
        let supported = self.adapter.supported_advertising_instances().await;
        match (active, supported) {
            (Ok(active), Ok(supported)) => {
                format!("ActiveInstances={active} SupportedInstances={supported}")
            }
            (active, supported) => {
                format!("instance counters unreadable: active {active:?}, supported {supported:?}")
            }
        }
    }

    /// The service UUID declaring `characteristic`, for settle reports.
    fn service_of(&self, characteristic: Uuid) -> String {
        for service in &self.services {
            if service
                .characteristics
                .iter()
                .any(|char_spec| char_spec.uuid == characteristic)
            {
                return service.uuid.to_string();
            }
        }
        characteristic.to_string()
    }

    fn setup_char_handlers(&mut self, handlers: Vec<CharNotifyHandler>) {
        for mut handler in handlers {
            let sender = self.events.clone();
            let writers = self.writers.clone();
            let ledger = self.ledger.clone();
            let clients = self.gatt_clients.clone();
            let session_tasks = self.session_tasks.clone();
            let handler_session_tasks = session_tasks.clone();
            let indication_ids = self.indication_ids.clone();
            let indication_receipts = self.indication_receipts.clone();
            let task = tokio::spawn(async move {
                while let Some(CharacteristicControlEvent::Notify(writer)) =
                    handler.control.next().await
                {
                    let raw_writer = Arc::new(writer);
                    let control_point = crate::gatt_spec::pmd::CONTROL_POINT
                        .parse::<Uuid>()
                        .unwrap_or_else(|_| Uuid::nil());
                    let is_control_point = handler.characteristic_uuid == control_point;
                    let (writer, owned_endpoint, owned_receiver) = if is_control_point {
                        let (tx, receiver) = mpsc::channel(1);
                        let endpoint = Arc::new(IndicationEndpoint::with_allocator(
                            tx,
                            indication_ids.clone(),
                            indication_receipts.clone(),
                        ));
                        (
                            Arc::new(NotificationWriter::OwnedIndication {
                                writer: raw_writer.clone(),
                                endpoint: endpoint.clone(),
                            }),
                            Some(endpoint),
                            Some(receiver),
                        )
                    } else {
                        (
                            Arc::new(NotificationWriter::Io(raw_writer.clone())),
                            None,
                            None,
                        )
                    };
                    let readiness = match session_readiness(raw_writer.as_ref()) {
                        Ok(readiness) => readiness,
                        Err(error) => {
                            eprintln!(
                                "h10-sim: cannot monitor notification session {}: {error}",
                                handler.characteristic_uuid
                            );
                            continue;
                        }
                    };
                    let service = handler.service_uuid.to_string();
                    let characteristic = handler.characteristic_uuid.to_string();
                    // Install before announcing: the pump and `notify` see a
                    // live session only after the writer is in place, so no
                    // frame is ever sent before the central's CCCD write
                    // completes. The generation supersedes any older session.
                    // The notify session carries its central's address, like
                    // reads and writes, so subscribers are recorded as
                    // clients even when they never read or wrote.
                    note_client(&clients, raw_writer.device_address());
                    let peer_address = raw_writer.device_address().to_string().to_ascii_uppercase();
                    let (generation, first_subscriber, transition_revision) = match ledger.lock() {
                        Ok(mut ledger) => {
                            let first_subscriber =
                                !ledger.is_subscribed(handler.characteristic_uuid);
                            let transition_revision = if first_subscriber {
                                match ledger.allocate_transition_revision() {
                                    Ok(revision) => Some(revision),
                                    Err(error) => {
                                        eprintln!("h10-sim: subscription revision allocation failed: {error}");
                                        continue;
                                    }
                                }
                            } else {
                                None
                            };
                            let generation =
                                ledger.subscribe(handler.characteristic_uuid, Some(&peer_address));
                            match writers.lock() {
                                Ok(mut writers) => {
                                    let previous = writers.insert(
                                        (handler.characteristic_uuid, Some(peer_address.clone())),
                                        RegisteredWriter {
                                            generation,
                                            writer: writer.clone(),
                                        },
                                    );
                                    if let Some(previous) = previous {
                                        match previous.writer.as_ref() {
                                            NotificationWriter::OwnedIndication {
                                                endpoint,
                                                ..
                                            } => endpoint.close(),
                                            NotificationWriter::Io(_) => {}
                                        }
                                    }
                                }
                                Err(error) => {
                                    ledger.unsubscribe(
                                        handler.characteristic_uuid,
                                        Some(&peer_address),
                                        generation,
                                    );
                                    eprintln!(
                                        "h10-sim: notification writer registry lock failed: {error}"
                                    );
                                    continue;
                                }
                            }
                            (generation, first_subscriber, transition_revision)
                        }
                        Err(_) => continue,
                    };
                    if let (Some(endpoint), Some(receiver)) =
                        (owned_endpoint.clone(), owned_receiver)
                    {
                        let pump = tokio::spawn(owned_indication_pump(
                            receiver,
                            OwnedIndicationPumpContext {
                                writer: raw_writer.clone(),
                                endpoint,
                                ledger: ledger.clone(),
                                writers: writers.clone(),
                                events: sender.clone(),
                                service: service.clone(),
                                characteristic: handler.characteristic_uuid,
                                peer_address: peer_address.clone(),
                                generation,
                            },
                        ));
                        if let Ok(mut tasks) = handler_session_tasks.lock() {
                            tasks.push(pump);
                        } else {
                            eprintln!(
                                "h10-sim: indication task ownership failed: task lock poisoned"
                            );
                        }
                    }
                    if first_subscriber
                        && sender
                            .send(RadioEvent::Subscription {
                                service: service.clone(),
                                characteristic: characteristic.clone(),
                                subscribed: true,
                                revision: transition_revision,
                            })
                            .await
                            .is_err()
                    {
                        if let Ok(mut ledger) = ledger.lock() {
                            if ledger.unsubscribe(
                                handler.characteristic_uuid,
                                Some(&peer_address),
                                generation,
                            ) {
                                if let Ok(mut writers) = writers.lock() {
                                    writers.remove(&(
                                        handler.characteristic_uuid,
                                        Some(peer_address.clone()),
                                    ));
                                }
                            }
                        }
                        break;
                    }
                    // BlueZ makes the `AcquireNotify` fd readable both when
                    // the central confirms an indication and when the session
                    // really ends: drain the confirmation and keep the writer,
                    // and only drop it on a closed fd. Treating a confirmation
                    // as an unsubscribe silently drops every later indication.
                    let mut ended = false;
                    while !ended {
                        match next_session_fate(&readiness).await {
                            Ok(SessionFate::Confirmed) => {
                                if ledger
                                    .lock()
                                    .map(|ledger| {
                                        ledger.is_current(
                                            handler.characteristic_uuid,
                                            Some(&peer_address),
                                            generation,
                                        )
                                    })
                                    .unwrap_or(false)
                                {
                                    if let Some(endpoint) = owned_endpoint.as_ref() {
                                        endpoint.confirm();
                                    } else if sender
                                        .send(RadioEvent::IndicationConfirmed {
                                            service: service.clone(),
                                            characteristic: characteristic.clone(),
                                            receipt_id: None,
                                            generation: Some(generation),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        ended = true;
                                    }
                                } else {
                                    eprintln!(
                                        "h10-sim: suppressed stale confirmation for {characteristic} generation {generation}"
                                    );
                                }
                            }
                            Ok(SessionFate::Spurious) => {}
                            Ok(SessionFate::Ended) => ended = true,
                            Err(error) => {
                                eprintln!("h10-sim: notification session {characteristic} read failed: {error}");
                                ended = true;
                            }
                        }
                    }
                    if let Some(endpoint) = owned_endpoint.as_ref() {
                        endpoint.close();
                    }
                    // Generation-aware cleanup: only the still-live session
                    // removes its writer, so a resubscribe that landed while
                    // this session drained keeps its writer.
                    if let Err(error) = retire_failed_subscription(
                        &sender,
                        &ledger,
                        &writers,
                        &service,
                        handler.characteristic_uuid,
                        Some(&peer_address),
                        generation,
                    )
                    .await
                    {
                        eprintln!("h10-sim: notification session {characteristic} retirement failed: {error}");
                        break;
                    }
                }
            });
            if let Ok(mut tasks) = session_tasks.lock() {
                tasks.push(task);
            };
        }
    }
}

/// The bounded ordered send pump: delivers queued sends FIFO to the peer
/// writers selected at admission and settles each logical send once. A stale
/// peer generation fails loudly while remaining peers still receive the value.
async fn send_pump(
    pump_queue: Arc<Mutex<SendQueue>>,
    pump_wake: Arc<Notify>,
    writers: WriterMap,
    ledger: Arc<Mutex<SubscriptionLedger>>,
    events: mpsc::Sender<RadioEvent>,
) {
    loop {
        pump_wake.notified().await;
        loop {
            let send = match pump_queue.lock() {
                Ok(mut queue) => queue.pop(),
                Err(_) => None,
            };
            let Some(send) = send else { break };
            let characteristic = send.characteristic.to_string();
            let mut failures = Vec::new();
            let mut ready = Vec::with_capacity(send.targets.len());
            if send.targets.is_empty() {
                failures.push("no addressed notification peers were selected".to_string());
            }
            for (target_index, target) in send.targets.iter().enumerate() {
                let peer = target.peer_address.as_str();
                let target_label = target_index + 1;
                let registration = match ledger.lock() {
                    Ok(ledger) => {
                        if !ledger.is_current(send.characteristic, Some(peer), target.generation) {
                            failures.push(format!(
                                "delivery target #{target_label}: subscription ended or was replaced before delivery"
                            ));
                            continue;
                        }
                        let key = (send.characteristic, Some(peer.to_string()));
                        match writers.lock() {
                            Ok(writers) => writers.get(&key).cloned(),
                            Err(error) => {
                                failures.push(format!(
                                    "delivery target #{target_label}: writer lock failed: {error}"
                                ));
                                continue;
                            }
                        }
                    }
                    Err(error) => {
                        failures.push(format!(
                            "delivery target #{target_label}: subscription ledger lock failed: {error}"
                        ));
                        continue;
                    }
                };
                let Some(registration) = registration else {
                    failures.push(format!(
                        "delivery target #{target_label}: writer unavailable before delivery"
                    ));
                    continue;
                };
                if !registered_writer_matches_target(
                    target,
                    registration.generation,
                    writer_peer(registration.writer.as_ref())
                        .as_deref()
                        .unwrap_or_default(),
                ) {
                    failures.push(format!(
                        "delivery target #{target_label}: writer changed before delivery"
                    ));
                    continue;
                }
                if !matches!(registration.writer.as_ref(), NotificationWriter::Io(_)) {
                    failures.push(format!(
                        "delivery target #{target_label}: indication writer entered notify pump"
                    ));
                    continue;
                }
                ready.push((target.clone(), registration));
            }

            let value = Arc::new(send.value);
            let attempts = attempt_peer_targets(ready, PEER_SEND_TIMEOUT, |(_, registration)| {
                let value = value.clone();
                async move {
                    match registration.writer.as_ref() {
                        NotificationWriter::Io(writer) => writer
                            .send(value.as_slice())
                            .await
                            .map_err(|error| error.to_string()),
                        NotificationWriter::OwnedIndication { .. } => {
                            Err("indication writer entered notify pump".to_string())
                        }
                    }
                }
            })
            .await;
            for (target_index, ((target, _registration), result)) in
                attempts.into_iter().enumerate()
            {
                let peer = target.peer_address.as_str();
                if let Err(error) = result {
                    let retirement = retire_failed_subscription(
                        &events,
                        &ledger,
                        &writers,
                        &send.service,
                        send.characteristic,
                        Some(peer),
                        target.generation,
                    )
                    .await;
                    failures.push(match retirement {
                        Ok(_) => format!(
                            "delivery target #{}: bluer notify failed: {error}",
                            target_index + 1
                        ),
                        Err(retirement) => format!(
                            "delivery target #{}: bluer notify failed: {error}; subscription retirement: {retirement}",
                            target_index + 1
                        ),
                    });
                }
            }
            let outcome = peer_send_outcome(failures);
            settle(&events, send.id, send.service, characteristic, outcome).await;
        }
    }
}

/// Delivers one peer-owned PMD indication at a time. The acquired BlueZ
/// socket accepts the value first; its confirmation byte is observed by the
/// peer session reader and then matched to this lane's receipt.
async fn publish_indication_unconfirmed(
    events: &mpsc::Sender<RadioEvent>,
    receipts: &IndicationReceipts,
    id: u64,
    service: String,
    characteristic: String,
    generation: u64,
    reason: impl Into<String>,
) {
    let receipt = remove_indication_receipt(receipts, id);
    let (service, characteristic, generation) = receipt
        .map(|receipt| (receipt.service, receipt.characteristic, receipt.generation))
        .unwrap_or((service, characteristic, generation));
    let _ = events
        .send(RadioEvent::IndicationUnconfirmed {
            service,
            characteristic,
            receipt_id: id,
            generation,
            reason: reason.into(),
        })
        .await;
}

struct OwnedIndicationPumpContext {
    writer: Arc<CharacteristicWriter>,
    endpoint: Arc<IndicationEndpoint>,
    ledger: Arc<Mutex<SubscriptionLedger>>,
    writers: WriterMap,
    events: mpsc::Sender<RadioEvent>,
    service: String,
    characteristic: Uuid,
    peer_address: String,
    generation: u64,
}

async fn owned_indication_pump(
    mut receiver: mpsc::Receiver<QueuedSend>,
    context: OwnedIndicationPumpContext,
) {
    let OwnedIndicationPumpContext {
        writer,
        endpoint,
        ledger,
        writers,
        events,
        service,
        characteristic,
        peer_address,
        generation,
    } = context;
    loop {
        let send = receive_owned_indication(&mut receiver, &endpoint).await;
        if send.is_none() && endpoint.is_closed() {
            receiver.close();
            drain_owned_indication_queue(&mut receiver, &endpoint, &events, characteristic).await;
            break;
        }
        let Some(send) = send else { break };
        if endpoint.is_closed() {
            receiver.close();
            settle_owned_indication_failure(
                &endpoint,
                &events,
                &send,
                characteristic,
                "owned indication subscription closed before delivery",
            )
            .await;
            drain_owned_indication_queue(&mut receiver, &endpoint, &events, characteristic).await;
            break;
        }
        if !ledger
            .lock()
            .map(|ledger| ledger.is_current(characteristic, Some(&peer_address), generation))
            .unwrap_or(false)
        {
            remove_indication_receipt(&endpoint.receipts, send.id);
            settle(
                &events,
                send.id,
                send.service,
                characteristic.to_string(),
                SendOutcome::Failed("owned indication subscription is stale".to_string()),
            )
            .await;
            continue;
        }

        mark_indication_stage(&endpoint.receipts, send.id, "inflight");
        let write = tokio::time::timeout(PEER_SEND_TIMEOUT, writer.send(&send.value)).await;
        let write = match write {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => Err(format!("peer send timed out after {PEER_SEND_TIMEOUT:?}")),
        };
        if let Err(error) = write {
            remove_indication_receipt(&endpoint.receipts, send.id);
            settle(
                &events,
                send.id,
                send.service,
                characteristic.to_string(),
                SendOutcome::Failed(format!("bluer indication write failed: {error}")),
            )
            .await;
            let _ = retire_failed_subscription(
                &events,
                &ledger,
                &writers,
                &service,
                characteristic,
                Some(&peer_address),
                generation,
            )
            .await;
            receiver.close();
            drain_owned_indication_queue(&mut receiver, &endpoint, &events, characteristic).await;
            break;
        }

        mark_indication_stage(&endpoint.receipts, send.id, "accepted_waiting_confirm");
        settle(
            &events,
            send.id,
            send.service,
            characteristic.to_string(),
            SendOutcome::OsAccepted,
        )
        .await;
        match endpoint
            .wait_confirmation(INDICATION_CONFIRMATION_TIMEOUT)
            .await
        {
            Ok(()) => {
                remove_indication_receipt(&endpoint.receipts, send.id);
                let _ = events
                    .send(RadioEvent::IndicationConfirmed {
                        service: service.clone(),
                        characteristic: characteristic.to_string(),
                        receipt_id: Some(send.id),
                        generation: Some(generation),
                    })
                    .await;
            }
            Err(reason) => {
                publish_indication_unconfirmed(
                    &events,
                    &endpoint.receipts,
                    send.id,
                    service.clone(),
                    characteristic.to_string(),
                    generation,
                    reason,
                )
                .await;
                endpoint.close();
                let _ = retire_failed_subscription(
                    &events,
                    &ledger,
                    &writers,
                    &service,
                    characteristic,
                    Some(&peer_address),
                    generation,
                )
                .await;
                receiver.close();
                drain_owned_indication_queue(&mut receiver, &endpoint, &events, characteristic)
                    .await;
                break;
            }
        }
    }
}

async fn receive_owned_indication(
    receiver: &mut mpsc::Receiver<QueuedSend>,
    endpoint: &IndicationEndpoint,
) -> Option<QueuedSend> {
    tokio::select! {
        send = receiver.recv() => send,
        _ = endpoint.closed() => None,
    }
}

async fn settle_owned_indication_failure(
    endpoint: &IndicationEndpoint,
    events: &mpsc::Sender<RadioEvent>,
    send: &QueuedSend,
    characteristic: Uuid,
    reason: &str,
) {
    remove_indication_receipt(&endpoint.receipts, send.id);
    settle(
        events,
        send.id,
        send.service.clone(),
        characteristic.to_string(),
        SendOutcome::Failed(reason.to_string()),
    )
    .await;
}

async fn drain_owned_indication_queue(
    receiver: &mut mpsc::Receiver<QueuedSend>,
    endpoint: &IndicationEndpoint,
    events: &mpsc::Sender<RadioEvent>,
    characteristic: Uuid,
) {
    while let Some(send) = receiver.recv().await {
        settle_owned_indication_failure(
            endpoint,
            events,
            &send,
            characteristic,
            "owned indication subscription closed before delivery",
        )
        .await;
    }
}

async fn drop_current_subscriptions(
    events: &mpsc::Sender<RadioEvent>,
    ledger: &Arc<Mutex<SubscriptionLedger>>,
    writers: &WriterMap,
    service: &str,
    characteristic: Uuid,
) -> Result<bool, RadioError> {
    let (targets, indication_generation) = {
        let ledger = ledger
            .lock()
            .map_err(|error| RadioError(format!("subscription ledger lock: {error}")))?;
        (
            ledger.targets(characteristic),
            ledger.current(characteristic, None),
        )
    };
    let mut dropped = false;
    for target in targets {
        dropped |= retire_failed_subscription(
            events,
            ledger,
            writers,
            service,
            characteristic,
            Some(&target.peer_address),
            target.generation,
        )
        .await?;
    }
    if let Some(generation) = indication_generation {
        dropped |= retire_failed_subscription(
            events,
            ledger,
            writers,
            service,
            characteristic,
            None,
            generation,
        )
        .await?;
    }
    Ok(dropped)
}

/// Retire and publish only this generation's loss. The session observer may
/// finish later; it must not remove a replacement or emit duplicate loss.
async fn retire_failed_subscription(
    events: &mpsc::Sender<RadioEvent>,
    ledger: &Arc<Mutex<SubscriptionLedger>>,
    writers: &WriterMap,
    service: &str,
    characteristic: Uuid,
    peer_address: Option<&str>,
    generation: u64,
) -> Result<bool, RadioError> {
    let retired = {
        let mut ledger = ledger
            .lock()
            .map_err(|error| RadioError(format!("subscription ledger lock: {error}")))?;
        let mut writers = writers
            .lock()
            .map_err(|error| RadioError(format!("subscription writer lock: {error}")))?;
        let key = (
            characteristic,
            peer_address.map(|peer| peer.to_ascii_uppercase()),
        );
        if ledger.unsubscribe(characteristic, peer_address, generation) {
            if let Some(RegisteredWriter {
                writer,
                generation: _,
            }) = writers.remove(&key)
            {
                match writer.as_ref() {
                    NotificationWriter::OwnedIndication { endpoint, .. } => endpoint.close(),
                    NotificationWriter::Io(_) => {}
                }
            }
            let final_subscriber = !ledger.is_subscribed(characteristic);
            let transition_revision = if final_subscriber {
                Some(ledger.allocate_transition_revision().map_err(RadioError)?)
            } else {
                None
            };
            (true, final_subscriber, transition_revision)
        } else {
            (false, false, None)
        }
    };
    if retired.0 && retired.1 {
        events
            .send(RadioEvent::Subscription {
                service: service.into(),
                characteristic: characteristic.to_string(),
                subscribed: false,
                revision: retired.2,
            })
            .await
            .map_err(|error| RadioError(format!("publish subscription loss: {error}")))?;
    }
    Ok(retired.0)
}

/// Reports one settled send; a gone receiver means the loop is gone too, so
/// there is nobody left to tell.
async fn settle(
    events: &mpsc::Sender<RadioEvent>,
    id: u64,
    service: String,
    characteristic: String,
    outcome: SendOutcome,
) {
    let _ = events
        .send(RadioEvent::NotifySettled {
            id,
            service,
            characteristic,
            outcome,
        })
        .await;
}

/// The `LEAdvertisement1` object the sim registers: a connectable
/// (`peripheral`) advertisement carrying the H10 service set, the name for the
/// scan response, General Discoverable flags, and manufacturer data only when
/// a payload is staged. Every other property stays unset so BlueZ applies its
/// defaults — no `Includes`, `Appearance`, `TxPower`, `Duration`, `Timeout`,
/// `DiscoverableTimeout`, `SecondaryChannel` or intervals. Pure construction,
/// pinned by `advertisement_properties_are_pinned`.
fn h10_advertisement(name: &str, uuids: &[Uuid], mfr: Option<&(u16, Vec<u8>)>) -> Advertisement {
    Advertisement {
        advertisement_type: AdvertisementType::Peripheral,
        service_uuids: uuids.iter().copied().collect(),
        manufacturer_data: mfr.cloned().into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(name.to_string()),
        ..Default::default()
    }
}

/// How the GATT server answers one ATT operation on a characteristic.
///
/// Mirrors the CoreBluetooth split the declarations are written for: the
/// property advertises the operation, the permission grants it. `bluer` has
/// one flag for both, so an advertised-but-not-granted operation keeps its
/// flag and is refused in the callback with `NotPermitted` — the answer a
/// CoreBluetooth peripheral gives — without consulting the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Absent,
    Granted,
    Refused,
}

fn access(
    spec: &CharSpec,
    property: CharProperty,
    permission: CharPermission,
) -> Result<Access, RadioError> {
    let advertised = spec.properties.contains(&property);
    let granted = spec.permissions.contains(&permission);
    match (advertised, granted) {
        (true, true) => Ok(Access::Granted),
        (true, false) => Ok(Access::Refused),
        (false, false) => Ok(Access::Absent),
        (false, true) => Err(RadioError(format!(
            "characteristic {} grants {permission:?} without the {property:?} property",
            spec.uuid
        ))),
    }
}

/// Write access across both write properties: a `Writeable` permission is
/// granted by `Write`, by `WriteWithoutResponse`, or by both (the FEEE
/// characteristics carry both, the vendor `6217ff4d` only
/// `WriteWithoutResponse`).
fn write_access(spec: &CharSpec) -> Result<(Access, bool, bool), RadioError> {
    let with_response = spec.properties.contains(&CharProperty::Write);
    let without_response = spec
        .properties
        .contains(&CharProperty::WriteWithoutResponse);
    let granted = spec.permissions.contains(&CharPermission::Writeable);
    let access = match (with_response || without_response, granted) {
        (true, true) => Access::Granted,
        (true, false) => Access::Refused,
        (false, false) => Access::Absent,
        (false, true) => {
            return Err(RadioError(format!(
                "characteristic {} grants Writeable without a write property \
                 (Write or WriteWithoutResponse)",
                spec.uuid
            )));
        }
    };
    Ok((access, with_response, without_response))
}

/// Checks a declared initial value against the Linux read path. BlueZ has no
/// cached-value slot: every read reaches the simulator, whose answer at start
/// is the declared initial value (pinned by
/// `live_reads_start_at_declared_initial_values`) and then tracks live state
/// (battery drain). An initial value on a characteristic nobody can read
/// could never be served, so the declaration is refused rather than dropped.
fn check_initial_value(spec: &CharSpec, read: Access) -> Result<(), RadioError> {
    match (&spec.initial_value, read) {
        (Some(_), Access::Absent | Access::Refused) => Err(RadioError(format!(
            "characteristic {} declares an initial value but is not readable \
             (needs the Read property and the Readable permission)",
            spec.uuid
        ))),
        _ => Ok(()),
    }
}

/// Answers one ATT read: refused reads never reach the simulator; granted
/// reads are forwarded to the simulator loop, whose answer is authoritative.
async fn answer_read(
    access: Access,
    sender: mpsc::Sender<RadioEvent>,
    service_uuid: Uuid,
    char_uuid: Uuid,
    offset: u16,
) -> Result<Vec<u8>, ReqError> {
    if access != Access::Granted {
        eprintln!("h10-sim: read of {char_uuid} refused: not granted Readable");
        return Err(ReqError::NotPermitted);
    }
    let (reply_tx, reply_rx) = oneshot::channel::<RadioReadAnswer>();
    let event = RadioEvent::Read {
        service: service_uuid.to_string(),
        characteristic: char_uuid.to_string(),
        offset: u64::from(offset),
        reply: reply_tx,
    };
    if sender.send(event).await.is_err() {
        eprintln!("h10-sim: simulator loop gone during read");
        return Err(ReqError::Failed);
    }
    match reply_rx.await {
        Ok(answer) if answer.ok => Ok(answer.value),
        Ok(_) => Err(ReqError::NotSupported),
        Err(_) => Err(ReqError::Failed),
    }
}

/// Builds the `bluer` GATT application from the queued service declarations.
/// Pure construction: no D-Bus traffic, so it is unit-testable without a radio.
/// An inconsistent declaration (a permission without its property, an
/// unreadable initial value) fails the whole application, loudly.
#[cfg(test)]
fn build_services(
    specs: Vec<ServiceSpec>,
    sender: mpsc::Sender<RadioEvent>,
    clients: Arc<Mutex<GattClientSet>>,
    writers: WriterMap,
    ledger: Arc<Mutex<SubscriptionLedger>>,
) -> Result<(Vec<CharNotifyHandler>, Vec<Service>), RadioError> {
    build_services_with_state(specs, sender, clients, writers, ledger)
}

fn build_services_with_state(
    specs: Vec<ServiceSpec>,
    sender: mpsc::Sender<RadioEvent>,
    clients: Arc<Mutex<GattClientSet>>,
    writers: WriterMap,
    ledger: Arc<Mutex<SubscriptionLedger>>,
) -> Result<(Vec<CharNotifyHandler>, Vec<Service>), RadioError> {
    let mut services = Vec::with_capacity(specs.len());
    let mut handlers = Vec::new();
    for spec in specs {
        let (_, service_handle) = service_control();
        let mut characteristics = Vec::with_capacity(spec.characteristics.len());
        for char_spec in spec.characteristics {
            let service_uuid = spec.uuid;
            let char_uuid = char_spec.uuid;
            let read_access = access(&char_spec, CharProperty::Read, CharPermission::Readable)?;
            let (write_access, with_response, without_response) = write_access(&char_spec)?;
            check_initial_value(&char_spec, read_access)?;
            let has_notify = char_spec.properties.contains(&CharProperty::Notify);
            let has_indicate = char_spec.properties.contains(&CharProperty::Indicate);

            let read = (read_access != Access::Absent).then(|| {
                let sender = sender.clone();
                let clients = clients.clone();
                CharacteristicRead {
                    read: true,
                    encrypt_read: false,
                    encrypt_authenticated_read: false,
                    secure_read: false,
                    fun: Box::new(move |request: CharacteristicReadRequest| {
                        // Every read carries its central's address: this is
                        // what makes the requester the sim's own client for
                        // drop-link.
                        note_client(&clients, request.device_address);
                        answer_read(
                            read_access,
                            sender.clone(),
                            service_uuid,
                            char_uuid,
                            request.offset,
                        )
                        .boxed()
                    }),
                    ..Default::default()
                }
            });

            let write = (write_access != Access::Absent).then(|| {
                let sender = sender.clone();
                let clients = clients.clone();
                let writers = writers.clone();
                let ledger = ledger.clone();
                let psftp_wire = crate::psftp::wire_characteristic();
                let control_point = crate::gatt_spec::pmd::CONTROL_POINT
                    .parse::<Uuid>()
                    .unwrap_or_else(|_| Uuid::nil());
                CharacteristicWrite {
                    write: with_response,
                    write_without_response: without_response,
                    reliable_write: false,
                    authenticated_signed_writes: false,
                    encrypt_write: false,
                    encrypt_authenticated_write: false,
                    secure_write: false,
                    method: CharacteristicWriteMethod::Fun(Box::new(
                        move |value: Vec<u8>, request: CharacteristicWriteRequest| {
                            let sender = sender.clone();
                            let writers = writers.clone();
                            let ledger = ledger.clone();
                            // Every write carries its central's address, like
                            // reads: CCCD and PMD control-point writers are
                            // recorded as clients here.
                            let device_address = request.device_address.to_string();
                            note_client(&clients, request.device_address);
                            async move {
                                if write_access == Access::Refused {
                                    eprintln!(
                                        "h10-sim: write to {char_uuid} refused: \
                                         not granted Writeable"
                                    );
                                    return Err(ReqError::NotPermitted);
                                }
                                let (reply_tx, reply_rx) = oneshot::channel::<bool>();
                                let event = RadioEvent::Write {
                                    service: service_uuid.to_string(),
                                    characteristic: char_uuid.to_string(),
                                    value,
                                    device_address: Some(device_address.clone()),
                                    owned_target: (char_uuid == psftp_wire
                                        || char_uuid == control_point)
                                        .then(|| {
                                            owned_target_for_write(
                                                &writers,
                                                &ledger,
                                                char_uuid,
                                                &device_address,
                                            )
                                        })
                                        .flatten(),
                                    reply: reply_tx,
                                };
                                if sender.send(event).await.is_err() {
                                    eprintln!("h10-sim: simulator loop gone during write");
                                    return Err(ReqError::Failed);
                                }
                                match reply_rx.await {
                                    Ok(true) => Ok(()),
                                    Ok(false) => Err(ReqError::NotSupported),
                                    Err(_) => Err(ReqError::Failed),
                                }
                            }
                            .boxed()
                        },
                    )),
                    ..Default::default()
                }
            });

            let (notify, control_handle) = if has_notify || has_indicate {
                let (control, handle) = characteristic_control();
                handlers.push(CharNotifyHandler {
                    service_uuid,
                    characteristic_uuid: char_uuid,
                    control,
                });
                (
                    Some(CharacteristicNotify {
                        notify: has_notify,
                        indicate: has_indicate,
                        method: CharacteristicNotifyMethod::Io,
                        ..Default::default()
                    }),
                    handle,
                )
            } else {
                (None, CharacteristicControlHandle::default())
            };
            characteristics.push(Characteristic {
                uuid: char_uuid,
                read,
                write,
                notify,
                control_handle,
                ..Default::default()
            });
        }
        services.push(Service {
            uuid: spec.uuid,
            primary: true,
            characteristics,
            control_handle: service_handle,
            ..Default::default()
        });
    }
    Ok((handlers, services))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn fatal_retirement_reports_queued_values_and_cancels_held_pump() {
        let delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let task = {
            let ready = ready.clone();
            let delivered = delivered.clone();
            tokio::spawn(async move {
                ready.notified().await;
                delivered.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        let queue = std::sync::Arc::new(std::sync::Mutex::new(crate::radio::SendQueue::new(2)));
        let characteristic = uuid::Uuid::nil();
        queue
            .lock()
            .unwrap()
            .push(crate::radio::QueuedSend {
                id: 0,
                service: "test".into(),
                characteristic,
                targets: Vec::new(),
                indication_generation: Some(1),
                value: vec![1, 2, 3],
            })
            .unwrap();
        let writers = Default::default();
        let receipt = super::retire_failed_sends(&task, &writers, &queue);
        ready.notify_one();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!delivered.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(receipt["queuedSendsNotConfirmed"][0]["bytes"], 3);
        assert_eq!(receipt["queuedSendsNotConfirmed"][0]["id"], 1);
        assert_eq!(receipt["inFlightDelivery"], "unknown-at-most-one");
        assert!(queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fatal_retirement_reports_all_indication_receipt_stages() {
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        let queue = std::sync::Arc::new(std::sync::Mutex::new(crate::radio::SendQueue::new(1)));
        let writers = Default::default();
        let receipts = std::sync::Arc::new(std::sync::Mutex::new(HashMap::from([
            (
                1,
                IndicationReceipt {
                    service: "svc".into(),
                    characteristic: "one".into(),
                    generation: 1,
                    stage: "queued",
                },
            ),
            (
                2,
                IndicationReceipt {
                    service: "svc".into(),
                    characteristic: "two".into(),
                    generation: 2,
                    stage: "inflight",
                },
            ),
            (
                3,
                IndicationReceipt {
                    service: "svc".into(),
                    characteristic: "three".into(),
                    generation: 3,
                    stage: "accepted_waiting_confirm",
                },
            ),
        ])));
        let tasks = std::sync::Arc::new(std::sync::Mutex::new(vec![task]));
        let receipt = super::retire_failed_sends_with_indications(
            &tokio::spawn(async { std::future::pending::<()>().await }),
            &writers,
            &queue,
            &receipts,
            &tasks,
        );
        let entries = receipt["indicationReceiptsNotConfirmed"]
            .as_array()
            .unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|entry| entry["stage"] == "queued"));
        assert!(entries.iter().any(|entry| entry["stage"] == "inflight"));
        assert!(entries
            .iter()
            .any(|entry| entry["stage"] == "accepted_waiting_confirm"));
        assert_eq!(receipt["sessionTasksAborted"], 1);
        assert_eq!(
            receipt["taskDrain"],
            "not-awaitable-in-synchronous-retirement"
        );
    }

    #[tokio::test]
    async fn confirmation_failure_is_observable_as_a_terminal_event() {
        let receipts = Arc::new(Mutex::new(HashMap::from([(
            7,
            IndicationReceipt {
                service: "svc".into(),
                characteristic: "char".into(),
                generation: 4,
                stage: "accepted_waiting_confirm",
            },
        )])));
        let (events, mut receiver) = mpsc::channel(1);
        publish_indication_unconfirmed(
            &events,
            &receipts,
            7,
            "fallback-service".into(),
            "fallback-characteristic".into(),
            99,
            "confirmation failed",
        )
        .await;
        assert!(receipts.lock().unwrap().is_empty());
        assert!(matches!(
            receiver.recv().await,
            Some(RadioEvent::IndicationUnconfirmed {
                receipt_id: 7,
                generation: 4,
                reason,
                ..
            }) if reason == "confirmation failed"
        ));
    }

    #[test]
    fn daemon_watch_uses_a_private_bus_and_detects_real_owner_loss() {
        let output = std::process::Command::new("dbus-run-session")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "radio::bluer_radio::tests::daemon_watch_private_bus_child",
                "--ignored",
                "--nocapture",
            ])
            .env("UBM_PRIVATE_DAEMON_WATCH_TEST", "1")
            .output()
            .expect("dbus-run-session is required for the Linux daemon-lifetime regression");
        assert!(
            output.status.success(),
            "private daemon watcher regression: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test must actually execute"
        );
    }

    #[tokio::test]
    #[ignore = "invoked only by private dbus-run-session parent test"]
    async fn daemon_watch_private_bus_child() {
        assert_eq!(
            std::env::var("UBM_PRIVATE_DAEMON_WATCH_TEST").as_deref(),
            Ok("1")
        );
        let (resource, owner) = dbus_tokio::connection::new_session_sync().unwrap();
        let worker = tokio::spawn(resource);
        owner
            .request_name("org.bluez", true, false, true)
            .await
            .unwrap();
        let watch = super::DaemonWatch::open_on_bus(dbus::channel::BusType::Session)
            .await
            .unwrap();
        watch.verify().await.unwrap();
        // Release and reacquire on the SAME unique connection: checking only
        // the current owner would miss loss of every daemon registration.
        owner.release_name("org.bluez").await.unwrap();
        owner
            .request_name("org.bluez", true, false, true)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while watch.lifetime.admit().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("real NameOwnerChanged must close admission");
        assert!(watch.verify().await.is_err());
        drop(watch);
        // A DISTINCT owner replacing the daemon after open but before the
        // caller's registration fence must also fail, without relying on a
        // second owner query inside open itself.
        let unopened_registration =
            super::DaemonWatch::open_on_bus(dbus::channel::BusType::Session)
                .await
                .unwrap();
        let (replacement_resource, replacement) =
            dbus_tokio::connection::new_session_sync().unwrap();
        let replacement_worker = tokio::spawn(replacement_resource);
        replacement
            .request_name("org.bluez", true, true, true)
            .await
            .unwrap();
        assert!(unopened_registration.verify().await.is_err());
        drop(unopened_registration);
        replacement.release_name("org.bluez").await.unwrap();
        replacement_worker.abort();
        // A late owner change after watcher disposal cannot resurrect or panic it.
        owner.release_name("org.bluez").await.unwrap();
        assert!(
            super::DaemonWatch::open_on_bus(dbus::channel::BusType::Session)
                .await
                .is_err()
        );
        worker.abort();
    }
    use super::*;

    #[tokio::test]
    async fn explicit_subscription_drop_publishes_final_loss_once() {
        let characteristic = Uuid::nil();
        let ledger = Arc::new(Mutex::new(SubscriptionLedger::new()));
        let writers = Arc::new(Mutex::new(HashMap::new()));
        let (events, mut receiver) = mpsc::channel(4);
        assert!(
            !drop_current_subscriptions(&events, &ledger, &writers, "service", characteristic)
                .await
                .unwrap()
        );
        let generation = ledger
            .lock()
            .unwrap()
            .subscribe(characteristic, Some("AA:AA:AA:AA:AA:AA"));
        assert!(
            drop_current_subscriptions(&events, &ledger, &writers, "service", characteristic)
                .await
                .unwrap()
        );
        assert!(matches!(
            receiver.recv().await,
            Some(RadioEvent::Subscription {
                subscribed: false,
                ..
            })
        ));
        assert!(
            !drop_current_subscriptions(&events, &ledger, &writers, "service", characteristic)
                .await
                .unwrap()
        );
        retire_failed_subscription(
            &events,
            &ledger,
            &writers,
            "service",
            characteristic,
            Some("AA:AA:AA:AA:AA:AA"),
            generation,
        )
        .await
        .unwrap();
        assert!(
            receiver.try_recv().is_err(),
            "late observer must not repeat forced loss"
        );
    }

    #[tokio::test]
    async fn one_peer_leaving_does_not_publish_final_subscription_loss() {
        let characteristic = Uuid::nil();
        let ledger = Arc::new(Mutex::new(SubscriptionLedger::new()));
        let writers = Arc::new(Mutex::new(HashMap::new()));
        let (events, mut receiver) = mpsc::channel(4);
        let peer_a = "AA:AA:AA:AA:AA:AA";
        let peer_b = "BB:BB:BB:BB:BB:BB";
        let generation_a = ledger
            .lock()
            .unwrap()
            .subscribe(characteristic, Some(peer_a));
        let generation_b = ledger
            .lock()
            .unwrap()
            .subscribe(characteristic, Some(peer_b));

        assert!(retire_failed_subscription(
            &events,
            &ledger,
            &writers,
            "service",
            characteristic,
            Some(peer_a),
            generation_a,
        )
        .await
        .unwrap());
        assert!(ledger.lock().unwrap().is_subscribed(characteristic));
        assert!(receiver.try_recv().is_err(), "A is not the final peer");

        assert!(retire_failed_subscription(
            &events,
            &ledger,
            &writers,
            "service",
            characteristic,
            Some(peer_b),
            generation_b,
        )
        .await
        .unwrap());
        assert!(!ledger.lock().unwrap().is_subscribed(characteristic));
        assert!(matches!(
            receiver.recv().await,
            Some(RadioEvent::Subscription {
                subscribed: false,
                ..
            })
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn confirmation_readiness_clears_and_next_observation_waits_for_new_data() {
        use std::io::Write;
        let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let readiness = session_readiness(&socket).unwrap();
        peer.write_all(&[1]).unwrap();
        assert_eq!(
            next_session_fate(&readiness).await.unwrap(),
            SessionFate::Confirmed
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                next_session_fate(&readiness)
            )
            .await
            .is_err(),
            "drained confirmation must not leave cached readiness spinning"
        );
        peer.write_all(&[2]).unwrap();
        assert_eq!(
            next_session_fate(&readiness).await.unwrap(),
            SessionFate::Confirmed
        );
        drop(peer);
        assert_eq!(
            next_session_fate(&readiness).await.unwrap(),
            SessionFate::Ended
        );
    }

    #[tokio::test]
    async fn failed_send_reports_final_loss_once_without_retiring_replacement() {
        let characteristic = Uuid::nil();
        let ledger = Arc::new(Mutex::new(SubscriptionLedger::new()));
        let writers = Arc::new(Mutex::new(HashMap::new()));
        let (events, mut receiver) = mpsc::channel(4);
        let peer = "AA:AA:AA:AA:AA:AA";
        let old = ledger.lock().unwrap().subscribe(characteristic, Some(peer));
        let current = ledger.lock().unwrap().subscribe(characteristic, Some(peer));
        retire_failed_subscription(
            &events,
            &ledger,
            &writers,
            "service",
            characteristic,
            Some(peer),
            old,
        )
        .await
        .unwrap();
        assert!(receiver.try_recv().is_err());
        assert!(ledger
            .lock()
            .unwrap()
            .is_current(characteristic, Some(peer), current));
        retire_failed_subscription(
            &events,
            &ledger,
            &writers,
            "service",
            characteristic,
            Some(peer),
            current,
        )
        .await
        .unwrap();
        assert!(matches!(
            receiver.recv().await,
            Some(RadioEvent::Subscription {
                subscribed: false,
                ..
            })
        ));
        retire_failed_subscription(
            &events,
            &ledger,
            &writers,
            "service",
            characteristic,
            Some(peer),
            current,
        )
        .await
        .unwrap();
        assert!(receiver.try_recv().is_err());
        assert!(!ledger.lock().unwrap().is_subscribed(characteristic));
    }
    use crate::radio::h10_services;
    use crate::sim::SimConfig;
    use std::collections::BTreeMap;

    fn build_app() -> (Vec<CharNotifyHandler>, Vec<Service>) {
        let specs = h10_services(&SimConfig::default()).unwrap();
        let (sender, _receiver) = mpsc::channel::<RadioEvent>(8);
        let clients = Arc::new(Mutex::new(GattClientSet::new()));
        build_services(
            specs,
            sender,
            clients,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(SubscriptionLedger::new())),
        )
        .expect("the H10 surface is consistent")
    }

    #[test]
    fn application_declares_modeled_h10_surface() {
        let (handlers, services) = build_app();
        assert_eq!(
            services.len(),
            5,
            "180D, 180A, 180F, PMD, FEEE; unmodeled PFC is absent"
        );
        let counts: Vec<usize> = services
            .iter()
            .map(|service| service.characteristics.len())
            .collect();
        assert_eq!(counts, vec![2, 7, 1, 2, 3]);
        assert!(services.iter().all(|service| service.primary));
        // All notify/indicate characteristics use acquired IO handlers so
        // BlueZ can retain a per-peer endpoint and confirmation channel.
        assert_eq!(handlers.len(), 6);
        let indicating_io_characteristics = services
            .iter()
            .flat_map(|service| &service.characteristics)
            .filter(|characteristic| {
                characteristic.notify.as_ref().is_some_and(|notify| {
                    notify.indicate
                        && !notify.notify
                        && matches!(notify.method, CharacteristicNotifyMethod::Io)
                })
            })
            .count();
        assert_eq!(indicating_io_characteristics, 1);
    }

    #[test]
    fn control_point_is_readable_writable_and_indicating() {
        use crate::gatt_spec;
        let (_, services) = build_app();
        let pmd_service = gatt_spec::pmd::SERVICE.parse::<Uuid>().unwrap();
        let pmd = services
            .iter()
            .find(|service| service.uuid == pmd_service)
            .expect("PMD service must be registered");
        assert_eq!(pmd.characteristics.len(), 2);
        let control_uuid = gatt_spec::pmd::CONTROL_POINT.parse::<Uuid>().unwrap();
        let control = pmd
            .characteristics
            .iter()
            .find(|characteristic| characteristic.uuid == control_uuid)
            .expect("PMD control point must be registered");
        let read = control.read.as_ref().expect("control point is readable");
        assert!(read.read);
        let write = control.write.as_ref().expect("control point is writable");
        assert!(
            write.write,
            "write request, as before — no behaviour change"
        );
        assert!(
            !write.write_without_response,
            "write command stays off, as before"
        );
        let notify = control.notify.as_ref().expect("control point indicates");
        assert!(!notify.notify);
        assert!(notify.indicate);
        assert!(matches!(notify.method, CharacteristicNotifyMethod::Io));
        let data_uuid = gatt_spec::pmd::DATA.parse::<Uuid>().unwrap();
        let data = pmd
            .characteristics
            .iter()
            .find(|characteristic| characteristic.uuid == data_uuid)
            .expect("PMD data must be registered");
        assert!(data.read.is_none() && data.write.is_none());
        let data_notify = data.notify.as_ref().expect("PMD data notifies");
        assert!(data_notify.notify && !data_notify.indicate);
        assert!(matches!(data_notify.method, CharacteristicNotifyMethod::Io));
    }

    fn queued_send(value: u8) -> QueuedSend {
        QueuedSend {
            id: 0,
            service: "service".to_string(),
            characteristic: Uuid::nil(),
            targets: Vec::new(),
            indication_generation: Some(1),
            value: vec![value],
        }
    }

    #[test]
    fn replaced_writer_generation_cannot_settle_an_old_owned_send() {
        let old = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 7);
        assert!(target_matches(&old, "AA:AA:AA:AA:AA:AA", 7));
        assert!(!target_matches(&old, "AA:AA:AA:AA:AA:AA", 8));
        assert!(!target_matches(&old, "BB:BB:BB:BB:BB:BB", 7));
    }

    #[test]
    fn queued_send_cannot_select_replacement_writer_for_same_peer() {
        let old_send = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 7);
        assert!(registered_writer_matches_target(
            &old_send,
            7,
            "AA:AA:AA:AA:AA:AA"
        ));
        assert!(!registered_writer_matches_target(
            &old_send,
            8,
            "AA:AA:AA:AA:AA:AA"
        ));
    }

    #[tokio::test]
    async fn a_stalled_peer_does_not_block_other_peer_delivery_or_settlement() {
        use crate::radio::OwnedNotifyTarget;

        let peer_a = OwnedNotifyTarget::new("AA:AA:AA:AA:AA:AA", 1);
        let peer_b = OwnedNotifyTarget::new("BB:BB:BB:BB:BB:BB", 1);
        let (delivered_tx, mut delivered_rx) = mpsc::channel(1);
        let (events, mut event_rx) = mpsc::channel(1);
        let attempt_run = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            let outcomes = attempt_peer_targets(
                vec![peer_a, peer_b],
                std::time::Duration::from_millis(10),
                |target| {
                    let delivered_tx = delivered_tx.clone();
                    async move {
                        if target.peer_address == "AA:AA:AA:AA:AA:AA" {
                            std::future::pending::<Result<(), String>>().await
                        } else {
                            delivered_tx
                                .send(target.peer_address)
                                .await
                                .map_err(|error| error.to_string())
                        }
                    }
                },
            )
            .await;
            let failures = outcomes
                .into_iter()
                .filter_map(|(_, outcome)| outcome.err())
                .collect();
            settle(
                &events,
                42,
                "service".to_string(),
                Uuid::nil().to_string(),
                peer_send_outcome(failures),
            )
            .await;
        })
        .await;

        assert!(
            attempt_run.is_ok(),
            "peer fan-out and settlement are bounded"
        );
        assert_eq!(
            delivered_rx.recv().await.as_deref(),
            Some("BB:BB:BB:BB:BB:BB")
        );
        assert!(matches!(
            event_rx.recv().await,
            Some(RadioEvent::NotifySettled {
                id: 42,
                outcome: SendOutcome::Failed(_),
                ..
            })
        ));
    }

    #[test]
    fn broadcast_settlement_fails_if_any_peer_send_fails() {
        assert_eq!(peer_send_outcome(Vec::new()), SendOutcome::OsAccepted);
        let outcome = peer_send_outcome(vec![
            "(peer=AA:AA:AA:AA:AA:AA): socket closed".to_string(),
            "[BB:BB:BB:BB:BB:BB] writer unavailable".to_string(),
        ]);
        let SendOutcome::Failed(reason) = outcome else {
            panic!("one failing peer must fail the logical notification");
        };
        assert!(reason.contains("socket closed"));
        assert!(reason.contains("writer unavailable"));
        assert!(!reason.contains("AA:AA:AA:AA:AA:AA"));
        assert!(!reason.contains("BB:BB:BB:BB:BB:BB"));
    }

    #[tokio::test]
    async fn indication_lane_is_fifo_and_capacity_one() {
        let (sender, mut receiver) = mpsc::channel(1);
        let endpoint = IndicationEndpoint::new(sender);
        assert_eq!(endpoint.enqueue(queued_send(1)), Ok(1 << 63));
        assert!(endpoint.enqueue(queued_send(2)).is_err());
        assert_eq!(
            receiver.recv().await.expect("first indication").value,
            vec![1]
        );
        // A rejected admission consumes an ID; receipt IDs are never reused.
        assert_eq!(endpoint.enqueue(queued_send(2)), Ok((1 << 63) + 2));
        assert_eq!(
            receiver.recv().await.expect("second indication").value,
            vec![2]
        );
    }

    #[tokio::test]
    async fn blocked_indication_lane_does_not_consume_notify_pump_capacity() {
        let (sender, _receiver) = mpsc::channel(1);
        let endpoint = IndicationEndpoint::new(sender);
        assert!(endpoint.enqueue(queued_send(1)).is_ok());
        assert!(endpoint.enqueue(queued_send(2)).is_err());

        let mut notify_queue = SendQueue::new(1);
        assert!(notify_queue.push(queued_send(3)).is_ok());
        assert_eq!(
            notify_queue
                .pop()
                .expect("notify remains independent")
                .value,
            vec![3]
        );
    }

    #[tokio::test]
    async fn closing_before_wait_is_observable_and_admission_is_rejected() {
        let (sender, _receiver) = mpsc::channel(1);
        let endpoint = IndicationEndpoint::new(sender);
        endpoint.close();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), endpoint.closed())
                .await
                .is_ok(),
            "a close before waiting must not lose the wake"
        );
        assert!(
            endpoint.enqueue(queued_send(1)).is_err(),
            "a close must atomically reject a racing admission"
        );
    }

    #[tokio::test]
    async fn owned_indication_confirmation_is_consumed_once_and_timeout_is_truthful() {
        let (sender, mut receiver) = mpsc::channel(1);
        let endpoint = Arc::new(IndicationEndpoint::new(sender));
        let first = endpoint.enqueue(queued_send(1)).unwrap();
        assert_eq!(receiver.recv().await.expect("queued indication").id, first);
        endpoint.confirm();
        assert_eq!(
            endpoint
                .wait_confirmation(std::time::Duration::from_millis(20))
                .await,
            Ok(())
        );
        assert_eq!(
            endpoint
                .wait_confirmation(std::time::Duration::from_millis(5))
                .await,
            Err("indication confirmation timed out")
        );
    }

    #[tokio::test]
    async fn owned_indication_close_wakes_confirmation_wait_without_acknowledging() {
        let (sender, _receiver) = mpsc::channel(1);
        let endpoint = Arc::new(IndicationEndpoint::new(sender));
        endpoint.close();
        assert_eq!(
            endpoint
                .wait_confirmation(std::time::Duration::from_millis(20))
                .await,
            Err("indication lane closed before confirmation")
        );
    }

    #[tokio::test]
    async fn owned_indication_idle_close_terminates_receive_wait() {
        let (sender, _receiver) = mpsc::channel(1);
        let endpoint = IndicationEndpoint::new(sender);
        endpoint.close();
        let (_sender, mut receiver) = mpsc::channel(1);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            receive_owned_indication(&mut receiver, &endpoint),
        )
        .await
        .expect("closed indication lane must wake an idle pump");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn owned_indication_close_drains_queued_receipts_once() {
        let (sender, mut receiver) = mpsc::channel(4);
        let receipts = Arc::new(Mutex::new(HashMap::new()));
        let endpoint = IndicationEndpoint::with_allocator(
            sender,
            Arc::new(IndicationIdAllocator::new()),
            receipts.clone(),
        );
        let first = endpoint.enqueue(queued_send(1)).expect("first admission");
        let second = endpoint.enqueue(queued_send(2)).expect("second admission");
        endpoint.close();
        receiver.close();
        let (events, mut event_rx) = mpsc::channel(4);
        drain_owned_indication_queue(&mut receiver, &endpoint, &events, Uuid::nil()).await;
        assert!(receipts.lock().expect("receipt lock").is_empty());
        let mut ids = Vec::new();
        while let Ok(Some(RadioEvent::NotifySettled { id, outcome, .. })) =
            tokio::time::timeout(std::time::Duration::from_millis(20), event_rx.recv()).await
        {
            assert!(matches!(outcome, SendOutcome::Failed(_)));
            ids.push(id);
        }
        ids.sort_unstable();
        assert_eq!(ids, vec![first, second]);
    }

    #[test]
    fn indication_ids_are_unique_across_endpoints_and_reconnects() {
        let allocator = Arc::new(IndicationIdAllocator::new());
        let receipts = Arc::new(Mutex::new(HashMap::new()));
        let (first_tx, _first_rx) = mpsc::channel(1);
        let first_endpoint =
            IndicationEndpoint::with_allocator(first_tx, allocator.clone(), receipts.clone());
        let first = first_endpoint.enqueue(queued_send(1)).unwrap();
        let (second_tx, _second_rx) = mpsc::channel(1);
        let second_endpoint =
            IndicationEndpoint::with_allocator(second_tx, allocator.clone(), receipts.clone());
        let second = second_endpoint.enqueue(queued_send(2)).unwrap();
        let (reconnect_tx, _reconnect_rx) = mpsc::channel(1);
        let reconnect_endpoint =
            IndicationEndpoint::with_allocator(reconnect_tx, allocator, receipts);
        let reconnect = reconnect_endpoint.enqueue(queued_send(3)).unwrap();
        assert_ne!(first, second);
        assert_ne!(second, reconnect);
        assert!(first >= INDICATION_ID_START);
        assert!(second >= INDICATION_ID_START);
    }

    #[test]
    fn indication_id_exhaustion_is_permanent_and_fail_closed() {
        let allocator = IndicationIdAllocator::with_next(u64::MAX);
        assert_eq!(allocator.allocate(), Some(u64::MAX));
        assert_eq!(allocator.allocate(), None);
        assert_eq!(allocator.allocate(), None);
    }

    fn one_characteristic(spec: CharSpec) -> ServiceSpec {
        ServiceSpec {
            uuid: crate::advertisement::short_uuid(0x180F),
            characteristics: vec![spec],
        }
    }

    #[test]
    fn advertisement_properties_are_pinned() {
        let uuids = [
            crate::advertisement::short_uuid(0x180D),
            crate::advertisement::short_uuid(0xFEEE),
        ];
        let adv = h10_advertisement("SIM Polar H10 0001", &uuids, None);
        assert_eq!(adv.advertisement_type, AdvertisementType::Peripheral);
        assert_eq!(adv.service_uuids, uuids.into_iter().collect());
        assert!(adv.manufacturer_data.is_empty(), "no payload staged");
        assert!(adv.solicit_uuids.is_empty());
        assert!(adv.service_data.is_empty());
        assert!(adv.advertising_data.is_empty());
        assert_eq!(adv.discoverable, Some(true));
        assert_eq!(adv.discoverable_timeout, None);
        assert!(
            adv.system_includes.is_empty(),
            "LocalName is set, so Includes must not repeat local-name"
        );
        assert_eq!(adv.local_name.as_deref(), Some("SIM Polar H10 0001"));
        assert_eq!(adv.appearance, None);
        assert_eq!(adv.duration, None);
        assert_eq!(adv.timeout, None);
        assert_eq!(adv.secondary_channel, None);
        assert_eq!(adv.min_interval, None);
        assert_eq!(adv.max_interval, None);
        assert_eq!(adv.tx_power, None);
    }

    #[test]
    fn staged_manufacturer_data_is_advertised() {
        let staged = (0x006B, vec![0x33, 0x1C]);
        let adv = h10_advertisement("SIM Polar H10 0001", &[], Some(&staged));
        assert_eq!(
            adv.manufacturer_data,
            BTreeMap::from([(0x006B, vec![0x33, 0x1C])])
        );
    }

    #[test]
    fn h10_surface_requires_no_encryption() {
        let (_, services) = build_app();
        for characteristic in services.iter().flat_map(|service| &service.characteristics) {
            if let Some(read) = &characteristic.read {
                assert!(read.read);
                assert!(
                    !read.encrypt_read && !read.encrypt_authenticated_read && !read.secure_read
                );
            }
            if let Some(write) = &characteristic.write {
                assert!(
                    write.write || write.write_without_response,
                    "a declared write flag must survive"
                );
                assert!(
                    !write.encrypt_write
                        && !write.encrypt_authenticated_write
                        && !write.secure_write
                        && !write.authenticated_signed_writes
                );
            }
        }
    }

    #[test]
    fn readable_characteristics_follow_their_declarations() {
        let specs = h10_services(&SimConfig::default()).unwrap();
        let (_, services) = build_app();
        for (spec, service) in specs.iter().zip(&services) {
            for (char_spec, characteristic) in
                spec.characteristics.iter().zip(&service.characteristics)
            {
                assert_eq!(char_spec.uuid, characteristic.uuid);
                assert_eq!(
                    characteristic.read.is_some(),
                    char_spec.properties.contains(&CharProperty::Read),
                    "read flag of {}",
                    char_spec.uuid
                );
                assert_eq!(
                    characteristic.write.is_some(),
                    char_spec.properties.contains(&CharProperty::Write)
                        || char_spec
                            .properties
                            .contains(&CharProperty::WriteWithoutResponse),
                    "write flag of {}",
                    char_spec.uuid
                );
                if let Some(write) = &characteristic.write {
                    assert_eq!(
                        write.write,
                        char_spec.properties.contains(&CharProperty::Write),
                        "with-response flag of {}",
                        char_spec.uuid
                    );
                    assert_eq!(
                        write.write_without_response,
                        char_spec
                            .properties
                            .contains(&CharProperty::WriteWithoutResponse),
                        "without-response flag of {}",
                        char_spec.uuid
                    );
                }
            }
        }
    }

    #[test]
    fn live_reads_start_at_declared_initial_values() {
        let config = SimConfig::default();
        let sim = crate::sim::SimState::new(config.clone());
        let mut declared = 0;
        for spec in h10_services(&config).unwrap() {
            for characteristic in spec.characteristics {
                let Some(initial) = characteristic.initial_value else {
                    continue;
                };
                declared += 1;
                let answer = crate::read_answer(&sim, &characteristic.uuid, 0);
                assert!(answer.ok, "{} must be readable", characteristic.uuid);
                assert_eq!(
                    answer.value, initial,
                    "first read of {}",
                    characteristic.uuid
                );
            }
        }
        assert_eq!(declared, 10, "BSL, battery, 7 DIS, PMD control point");
    }

    #[tokio::test]
    async fn read_without_readable_permission_is_refused_before_the_sim() {
        let spec = one_characteristic(CharSpec {
            uuid: crate::advertisement::short_uuid(0x2A19),
            properties: vec![CharProperty::Read],
            permissions: Vec::new(),
            initial_value: None,
        });
        let (sender, mut receiver) = mpsc::channel::<RadioEvent>(8);
        let service_uuid = spec.uuid;
        let char_uuid = spec.characteristics[0].uuid;
        let (_, services) = build_services(
            vec![spec],
            sender.clone(),
            Arc::new(Mutex::new(GattClientSet::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(SubscriptionLedger::new())),
        )
        .unwrap();
        assert!(
            services[0].characteristics[0].read.is_some(),
            "the Read property keeps the flag"
        );
        let answer = answer_read(Access::Refused, sender, service_uuid, char_uuid, 0).await;
        assert!(matches!(answer, Err(ReqError::NotPermitted)));
        assert!(receiver.try_recv().is_err(), "the sim is never consulted");
    }

    #[test]
    fn permission_without_property_is_refused() {
        let spec = one_characteristic(CharSpec {
            uuid: crate::advertisement::short_uuid(0x2A19),
            properties: vec![CharProperty::Notify],
            permissions: vec![CharPermission::Writeable],
            initial_value: None,
        });
        let (sender, _receiver) = mpsc::channel::<RadioEvent>(8);
        let error = build_services(
            vec![spec],
            sender,
            Arc::new(Mutex::new(GattClientSet::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(SubscriptionLedger::new())),
        )
        .err()
        .expect("refused");
        assert!(
            error.0.contains("Writeable without a write property"),
            "{}",
            error.0
        );
    }

    #[test]
    fn write_without_response_only_advertises_no_response_write() {
        use crate::gatt_spec;
        let (_, services) = build_app();
        let find = |uuid: &str| {
            let target = uuid.parse::<Uuid>().unwrap();
            services
                .iter()
                .flat_map(|service| &service.characteristics)
                .find(|characteristic| characteristic.uuid == target)
                .unwrap_or_else(|| panic!("{uuid} must be registered"))
        };
        assert!(
            services
                .iter()
                .flat_map(|service| &service.characteristics)
                .all(|characteristic| characteristic.uuid
                    != gatt_spec::vendor::WRITE_INDICATE.parse::<Uuid>().unwrap()),
            "unmodeled PFC must not be advertised"
        );
        // The PMD control point keeps write-with-response and no command flag.
        let control = find(gatt_spec::pmd::CONTROL_POINT)
            .write
            .as_ref()
            .expect("control point is writable");
        assert!(control.write);
        assert!(!control.write_without_response);
        // FEEE 0x51 carries both write flags plus notify.
        let char51 = find(gatt_spec::feee::CHAR_51);
        let write51 = char51.write.as_ref().expect("0x51 is writable");
        assert!(write51.write && write51.write_without_response);
        assert!(char51.notify.as_ref().expect("0x51 notifies").notify);
    }

    #[test]
    fn session_drain_keeps_confirmed_indications_and_drops_closed() {
        assert_eq!(
            classify_session_drain(Ok(1)),
            SessionFate::Confirmed,
            "BlueZ's per-indication confirmation byte keeps the session"
        );
        assert_eq!(
            classify_session_drain(Ok(0)),
            SessionFate::Ended,
            "an empty read means the peer closed the session"
        );
        assert_eq!(
            classify_session_drain(Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))),
            SessionFate::Spurious,
            "a spurious wakeup keeps waiting"
        );
        assert_eq!(
            classify_session_drain(Err(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset
            ))),
            SessionFate::Ended,
            "a dead socket ends the session"
        );
    }

    #[test]
    fn unreadable_initial_value_is_refused() {
        let spec = one_characteristic(CharSpec {
            uuid: crate::advertisement::short_uuid(0x2A19),
            properties: vec![CharProperty::Read],
            permissions: Vec::new(),
            initial_value: Some(vec![85]),
        });
        let (sender, _receiver) = mpsc::channel::<RadioEvent>(8);
        let error = build_services(
            vec![spec],
            sender,
            Arc::new(Mutex::new(GattClientSet::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(SubscriptionLedger::new())),
        )
        .err()
        .expect("refused");
        assert!(
            error
                .0
                .contains("declares an initial value but is not readable"),
            "{}",
            error.0
        );
    }
}
