use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as SyncMutex,
    },
    time::Duration,
};

use btleplug::{
    api::{
        Central, CharPropFlags, Characteristic, Descriptor, Manager as _, Peripheral as _,
        ScanFilter, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures_util::StreamExt;
use serde_json::Number;
use tauri::async_runtime::JoinHandle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AuthenticatedCaller, DispatchFuture, IpcDispatcher, IpcEventSink, IpcValue};

const MAX_PENDING_EVENTS: usize = 256;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default)]
pub struct BtleplugDispatcherOptions {
    /// Exact `Adapter::adapter_info()` value to select when multiple adapters exist.
    pub adapter_id: Option<String>,
}

/// Production Tauri dispatcher backed by btleplug's CoreBluetooth, WinRT, and BlueZ hosts.
#[derive(Clone)]
pub struct BtleplugDispatcher {
    inner: Arc<Mutex<DispatcherState>>,
    next_id: Arc<AtomicU64>,
    next_revocation: Arc<AtomicU64>,
    revoked_callers: Arc<SyncMutex<HashMap<String, u64>>>,
    options: BtleplugDispatcherOptions,
}

struct DispatcherState {
    manager: Option<Manager>,
    adapter: Option<Adapter>,
    attachment: Option<Attachment>,
    callers: HashMap<String, CallerState>,
    scan_owner: Option<String>,
    peer_owners: HashMap<String, String>,
}

#[derive(Clone)]
struct Attachment {
    attachment_id: String,
    backend_instance_id: String,
    backend_generation: String,
    adapter_id: String,
    adapter_name: String,
    adapter_generation: String,
}

struct CallerState {
    lease_id: String,
    lease_generation: String,
    event_sink: IpcEventSink,
    scan_admitting: bool,
    scan: Option<ScanResource>,
    connections: HashMap<String, ConnectionResource>,
    databases: HashMap<String, DatabaseResource>,
    subscriptions: HashMap<String, SubscriptionResource>,
    operations: HashMap<String, CancellationToken>,
    pending_events: HashSet<String>,
}

struct ScanResource {
    handle: String,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct ConnectionResource {
    peer_id: String,
    peripheral: Peripheral,
}

struct DatabaseResource {
    connection_handle: String,
    characteristics: HashMap<String, Characteristic>,
    descriptors: HashMap<String, Descriptor>,
}

struct SubscriptionResource {
    database_handle: String,
    peripheral: Peripheral,
    characteristic: Characteristic,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct DispatchError {
    code: &'static str,
    domain: &'static str,
    operation: String,
    platform: Option<String>,
    retryable: bool,
}

impl DispatchError {
    fn new(code: &'static str, domain: &'static str, operation: impl Into<String>) -> Self {
        Self {
            code,
            domain,
            operation: operation.into(),
            platform: None,
            retryable: matches!(code, "operation.aborted" | "operation.timed-out"),
        }
    }

    fn platform(mut self, message: impl Into<String>) -> Self {
        self.platform = Some(message.into());
        self
    }

    fn into_response(self) -> IpcValue {
        let platform = self.platform.map_or(IpcValue::Null, |message| {
            object([
                ("domain", string("btleplug")),
                ("code", string("native-error")),
                ("safeMessage", string(message)),
                ("metadata", object([])),
            ])
        });
        object([
            ("kind", string("failure")),
            (
                "error",
                object([
                    ("code", string(self.code)),
                    ("domain", string(self.domain)),
                    ("operation", string(self.operation)),
                    ("platform", platform),
                    (
                        "retryability",
                        string(if self.retryable {
                            "caller-decides"
                        } else {
                            "never"
                        }),
                    ),
                ]),
            ),
        ])
    }
}

impl Default for BtleplugDispatcher {
    fn default() -> Self {
        Self::new(BtleplugDispatcherOptions::default())
    }
}

impl BtleplugDispatcher {
    pub fn new(options: BtleplugDispatcherOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DispatcherState {
                manager: None,
                adapter: None,
                attachment: None,
                callers: HashMap::new(),
                scan_owner: None,
                peer_owners: HashMap::new(),
            })),
            next_id: Arc::new(AtomicU64::new(1)),
            next_revocation: Arc::new(AtomicU64::new(1)),
            revoked_callers: Arc::new(SyncMutex::new(HashMap::new())),
            options,
        }
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn dispatch_request(
        &self,
        caller: AuthenticatedCaller,
        request: IpcValue,
        event_sink: IpcEventSink,
    ) -> Result<IpcValue, DispatchError> {
        let request = into_object(request, "tauri.request")?;
        let kind = required_string(&request, "kind", "tauri.request-kind")?;
        if kind != "bootstrap" && self.is_revoked(&caller_key(&caller)) {
            return Err(DispatchError::new(
                "ownership.denied",
                "ipc",
                "tauri.caller-revoked",
            ));
        }
        match kind.as_str() {
            "bootstrap" => self.bootstrap(caller, event_sink).await,
            "route" => self.route(caller, request, event_sink).await,
            "event.ack" => self.acknowledge(caller, request).await,
            "release" => self.release_request(caller, request).await,
            _ => Err(DispatchError::new(
                "protocol.malformed",
                "ipc",
                "tauri.request-kind",
            )),
        }
    }

    async fn ensure_adapter(&self) -> Result<Attachment, DispatchError> {
        {
            let state = self.inner.lock().await;
            if let Some(attachment) = &state.attachment {
                return Ok(attachment.clone());
            }
        }

        let manager = Manager::new().await.map_err(|error| {
            DispatchError::new("adapter.unavailable", "adapter", "tauri.manager")
                .platform(error.to_string())
        })?;
        let adapters = manager.adapters().await.map_err(|error| {
            DispatchError::new("adapter.unavailable", "adapter", "tauri.adapters")
                .platform(error.to_string())
        })?;
        if adapters.is_empty() {
            return Err(DispatchError::new(
                "adapter.unavailable",
                "adapter",
                "tauri.adapters-empty",
            ));
        }

        let mut candidates = Vec::with_capacity(adapters.len());
        for adapter in adapters {
            let info = adapter.adapter_info().await.map_err(|error| {
                DispatchError::new("adapter.unavailable", "adapter", "tauri.adapter-info")
                    .platform(error.to_string())
            })?;
            candidates.push((info, adapter));
        }
        let (adapter_name, adapter) = match &self.options.adapter_id {
            Some(requested) => candidates
                .into_iter()
                .find(|(info, _)| info == requested)
                .ok_or_else(|| {
                    DispatchError::new(
                        "adapter.selection-required",
                        "adapter",
                        "tauri.adapter-selection",
                    )
                })?,
            None if candidates.len() == 1 => candidates.remove(0),
            None => {
                return Err(DispatchError::new(
                    "adapter.ambiguous",
                    "adapter",
                    "tauri.adapter-selection",
                ))
            }
        };
        let attachment = Attachment {
            attachment_id: self.id("tauri-attachment"),
            backend_instance_id: self.id("tauri-btleplug"),
            backend_generation: self.id("tauri-backend-generation"),
            adapter_id: adapter_name.clone(),
            adapter_name,
            adapter_generation: self.id("tauri-adapter-generation"),
        };
        let mut state = self.inner.lock().await;
        if state.attachment.is_none() {
            state.manager = Some(manager);
            state.adapter = Some(adapter);
            state.attachment = Some(attachment.clone());
        }
        Ok(state.attachment.clone().unwrap_or(attachment))
    }

    async fn bootstrap(
        &self,
        caller: AuthenticatedCaller,
        event_sink: IpcEventSink,
    ) -> Result<IpcValue, DispatchError> {
        let attachment = self.ensure_adapter().await?;
        let key = caller_key(&caller);
        let cleanup = self.release(&key).await;
        if !is_released(&cleanup) {
            return Err(DispatchError::new(
                "platform.failure",
                "cleanup",
                "tauri.bootstrap-prior-release",
            ));
        }
        let lease_id = self.id("tauri-lease");
        let lease_generation = self.id("tauri-lease-generation");
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .remove(&key);
        self.inner.lock().await.callers.insert(
            key,
            CallerState {
                lease_id: lease_id.clone(),
                lease_generation: lease_generation.clone(),
                event_sink,
                scan_admitting: false,
                scan: None,
                connections: HashMap::new(),
                databases: HashMap::new(),
                subscriptions: HashMap::new(),
                operations: HashMap::new(),
                pending_events: HashSet::new(),
            },
        );

        let renderer = object([
            (
                "clientId",
                string(format!("{}:{}", caller.app_identifier, caller.window_label)),
            ),
            ("windowScope", string(caller.window_label)),
            ("sessionScope", string(lease_generation.clone())),
        ]);
        let attachment_record = attachment_record(&attachment);
        Ok(object([
            ("kind", string("bootstrap")),
            (
                "bootstrap",
                object([
                    ("attachment", attachment_record),
                    ("attachmentId", string(attachment.attachment_id)),
                    ("versions", ipc_versions()),
                    ("renderer", renderer),
                    (
                        "rendererLease",
                        object([
                            ("leaseId", string(lease_id)),
                            ("generation", string(lease_generation)),
                        ]),
                    ),
                ]),
            ),
        ]))
    }

    async fn route(
        &self,
        caller: AuthenticatedCaller,
        request: BTreeMap<String, IpcValue>,
        event_sink: IpcEventSink,
    ) -> Result<IpcValue, DispatchError> {
        let envelope = into_object(
            required_value(&request, "envelope", "tauri.route-envelope")?.clone(),
            "tauri.route-envelope",
        )?;
        let command = required_string(&envelope, "command", "tauri.route-command")?;
        let correlation = required_string(&envelope, "correlation", "tauri.route-correlation")?;
        let payload = into_object(
            required_value(&envelope, "payload", "tauri.route-payload")?.clone(),
            "tauri.route-payload",
        )?;
        let binary_payload = match envelope.get("binaryPayload") {
            Some(IpcValue::Bytes(bytes)) => Some(bytes.clone()),
            Some(IpcValue::Null) | None => None,
            _ => {
                return Err(DispatchError::new(
                    "bytes.invalid",
                    "ipc",
                    "tauri.route-binary",
                ))
            }
        };
        self.validate_envelope(&caller, &envelope, event_sink)
            .await?;

        if command == "operation.cancel" {
            let target = required_string(&payload, "targetCorrelation", "tauri.cancel")?;
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&caller_key(&caller)).ok_or_else(|| {
                DispatchError::new("ownership.denied", "ipc", "tauri.cancel-owner")
            })?;
            let cancellation = caller_state.operations.get(&target);
            if let Some(cancellation) = cancellation {
                cancellation.cancel();
            }
            return Ok(route_response(object([(
                "state",
                string(if cancellation.is_some() {
                    "cancellation-requested"
                } else {
                    "already-terminal"
                }),
            )])));
        }

        let cancellation = CancellationToken::new();
        {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&caller_key(&caller)).ok_or_else(|| {
                DispatchError::new("ownership.denied", "ipc", "tauri.route-owner")
            })?;
            if caller_state.operations.contains_key(&correlation) {
                return Err(DispatchError::new(
                    "protocol.violation",
                    "ipc",
                    "tauri.correlation-replay",
                ));
            }
            caller_state
                .operations
                .insert(correlation.clone(), cancellation.clone());
        }

        let operation_dispatcher = self.clone();
        let operation_caller = caller.clone();
        let operation_command = command.clone();
        let mut operation = tauri::async_runtime::spawn(async move {
            operation_dispatcher
                .execute(
                    &operation_caller,
                    &operation_command,
                    payload,
                    binary_payload,
                )
                .await
        });
        let result = tokio::select! {
            result = &mut operation => result.map_err(|error| {
                DispatchError::new("platform.failure", "ipc", format!("tauri.{command}.join"))
                    .platform(error.to_string())
            })?,
            () = cancellation.cancelled() => {
                let quarantine_dispatcher = self.clone();
                let quarantine_caller = caller.clone();
                let quarantine_command = command.clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(Ok(late_payload)) = operation.await {
                        quarantine_dispatcher
                            .quarantine_cancelled_success(
                                &quarantine_caller,
                                &quarantine_command,
                                &late_payload,
                            )
                            .await;
                    }
                });
                Err(DispatchError::new("operation.aborted", "ipc", format!("tauri.{command}")))
            },
        };
        if let Some(caller_state) = self
            .inner
            .lock()
            .await
            .callers
            .get_mut(&caller_key(&caller))
        {
            caller_state.operations.remove(&correlation);
        }
        result.map(route_response)
    }

    async fn quarantine_cancelled_success(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: &IpcValue,
    ) {
        let IpcValue::Object(payload) = payload else {
            return;
        };
        if command == "gatt.discover" {
            if let Some(handle) = payload.get("handle").and_then(as_string) {
                if let Some(caller_state) =
                    self.inner.lock().await.callers.get_mut(&caller_key(caller))
                {
                    caller_state.databases.remove(handle);
                }
            }
            return;
        }
        let cleanup = match command {
            "scan.start" => payload.get("handle").and_then(as_string).map(|handle| {
                (
                    "scan.stop",
                    object([("scanHandle", string(handle.to_owned()))]),
                )
            }),
            "connection.connect" => payload.get("handle").and_then(as_string).map(|handle| {
                (
                    "connection.disconnect",
                    object([("connectionHandle", string(handle.to_owned()))]),
                )
            }),
            "gatt.subscribe" => payload.get("handle").and_then(as_string).map(|handle| {
                (
                    "gatt.unsubscribe",
                    object([("subscriptionHandle", string(handle.to_owned()))]),
                )
            }),
            _ => None,
        };
        if let Some((cleanup_command, IpcValue::Object(cleanup_payload))) = cleanup {
            self.execute(caller, cleanup_command, cleanup_payload, None)
                .await
                .ok();
        }
    }

    async fn validate_envelope(
        &self,
        caller: &AuthenticatedCaller,
        envelope: &BTreeMap<String, IpcValue>,
        event_sink: IpcEventSink,
    ) -> Result<(), DispatchError> {
        let lease = into_object(
            required_value(envelope, "rendererLease", "tauri.route-lease")?.clone(),
            "tauri.route-lease",
        )?;
        let lease_id = required_string(&lease, "leaseId", "tauri.route-lease")?;
        let lease_generation = required_string(&lease, "generation", "tauri.route-lease")?;
        let attachment_id = required_string(envelope, "attachmentId", "tauri.route-attachment")?;
        let mut state = self.inner.lock().await;
        let attachment = state.attachment.as_ref().ok_or_else(|| {
            DispatchError::new("lifecycle.invalid-state", "ipc", "tauri.route-bootstrap")
        })?;
        if attachment.attachment_id != attachment_id {
            return Err(DispatchError::new(
                "protocol.violation",
                "ipc",
                "tauri.route-attachment",
            ));
        }
        let caller_state = state
            .callers
            .get_mut(&caller_key(caller))
            .ok_or_else(|| DispatchError::new("ownership.denied", "ipc", "tauri.route-caller"))?;
        if caller_state.lease_id != lease_id || caller_state.lease_generation != lease_generation {
            return Err(DispatchError::new(
                "ownership.denied",
                "ipc",
                "tauri.route-lease",
            ));
        }
        caller_state.event_sink = event_sink;
        Ok(())
    }

    async fn execute(
        &self,
        caller: &AuthenticatedCaller,
        command: &str,
        payload: BTreeMap<String, IpcValue>,
        binary_payload: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        match command {
            "adapter.state" => self.adapter_state().await,
            "scan.start" => self.start_scan(caller, payload).await,
            "scan.stop" => self.stop_scan(caller, payload).await,
            "connection.connect" => self.connect(caller, payload).await,
            "connection.disconnect" => self.disconnect(caller, payload).await,
            "connection.rssi" => self.read_rssi(caller, payload).await,
            "connection.maximum-write-length" => self.maximum_write_length(caller, payload).await,
            "gatt.discover" => self.discover(caller, payload).await,
            "gatt.read" => self.read(caller, payload).await,
            "gatt.write" => self.write(caller, payload, binary_payload).await,
            "gatt.subscribe" => self.subscribe(caller, payload).await,
            "gatt.unsubscribe" => self.unsubscribe(caller, payload).await,
            "gatt.descriptor.read" => self.read_descriptor(caller, payload).await,
            "gatt.descriptor.write" => self.write_descriptor(caller, payload, binary_payload).await,
            _ => Err(DispatchError::new(
                "argument.invalid",
                "ipc",
                "tauri.route-command",
            )),
        }
    }

    async fn adapter_state(&self) -> Result<IpcValue, DispatchError> {
        let state = self.inner.lock().await;
        let attachment = state.attachment.as_ref().ok_or_else(|| {
            DispatchError::new("adapter.unavailable", "adapter", "tauri.adapter-state")
        })?;
        Ok(adapter_state(attachment))
    }

    async fn start_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let service_uuids = optional_string_array(&payload, "serviceUuids", "tauri.scan-services")?
            .into_iter()
            .map(|value| parse_uuid(&value, "tauri.scan-services"))
            .collect::<Result<Vec<_>, _>>()?;
        let local_name_prefix =
            optional_nullable_string(&payload, "localNamePrefix", "tauri.scan-name")?;
        let manufacturer_filters = manufacturer_filters(&payload)?;
        let adapter = self.adapter().await?;
        let key = caller_key(caller);
        let requested_services = service_uuids.clone();
        {
            let mut state = self.inner.lock().await;
            if state.scan_owner.is_some() {
                return Err(DispatchError::new(
                    "scan.already-active",
                    "scan",
                    "tauri.scan-global-owner",
                ));
            }
            let caller_state = state.callers.get_mut(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "scan", "tauri.scan-owner")
            })?;
            if caller_state.scan_admitting || caller_state.scan.is_some() {
                return Err(DispatchError::new(
                    "scan.already-active",
                    "scan",
                    "tauri.scan-start",
                ));
            }
            caller_state.scan_admitting = true;
            state.scan_owner = Some(key.clone());
        }
        if let Err(error) = adapter
            .start_scan(ScanFilter {
                services: service_uuids,
            })
            .await
        {
            let mut state = self.inner.lock().await;
            if let Some(caller_state) = state.callers.get_mut(&key) {
                caller_state.scan_admitting = false;
            }
            if state.scan_owner.as_deref() == Some(&key) {
                state.scan_owner = None;
            }
            return Err(
                DispatchError::new("scan.start-failed", "scan", "tauri.scan-start")
                    .platform(error.to_string()),
            );
        }
        let handle = self.id("scan");
        let dispatcher = self.clone();
        let stream_handle = handle.clone();
        let scan_adapter = adapter.clone();
        let stream_owner = key.clone();
        let task = tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(SCAN_POLL_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let peripherals = match scan_adapter.peripherals().await {
                    Ok(peripherals) => peripherals,
                    Err(_) => {
                        dispatcher
                            .terminal(&stream_owner, &stream_handle, "source-failed")
                            .await
                            .ok();
                        dispatcher
                            .fail_scan_stream(&stream_owner, &stream_handle)
                            .await;
                        return;
                    }
                };
                for peripheral in peripherals {
                    let properties = match peripheral.properties().await {
                        Ok(Some(properties)) => properties,
                        _ => continue,
                    };
                    if !requested_services.is_empty()
                        && !requested_services
                            .iter()
                            .all(|uuid| properties.services.contains(uuid))
                    {
                        continue;
                    }
                    if let Some(prefix) = &local_name_prefix {
                        if !properties
                            .local_name
                            .as_deref()
                            .is_some_and(|name| name.starts_with(prefix))
                        {
                            continue;
                        }
                    }
                    if !manufacturer_filters.iter().all(|filter| {
                        properties
                            .manufacturer_data
                            .get(&filter.company_id)
                            .is_some_and(|data| {
                                filter
                                    .data_prefix
                                    .as_ref()
                                    .map_or(true, |prefix| data.starts_with(prefix))
                            })
                    }) {
                        continue;
                    }
                    let observation = peripheral_observation(&peripheral, properties);
                    if dispatcher
                        .emit(&stream_owner, &stream_handle, observation)
                        .await
                        .is_err()
                    {
                        dispatcher
                            .terminal(&stream_owner, &stream_handle, "source-failed")
                            .await
                            .ok();
                        dispatcher
                            .fail_scan_stream(&stream_owner, &stream_handle)
                            .await;
                        return;
                    }
                }
            }
        });
        let mut state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get_mut(&key) else {
            task.abort();
            if state.scan_owner.as_deref() == Some(&key) {
                state.scan_owner = None;
            }
            drop(state);
            adapter.stop_scan().await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "scan",
                "tauri.scan-owner",
            ));
        };
        caller_state.scan_admitting = false;
        caller_state.scan = Some(ScanResource {
            handle: handle.clone(),
            task,
        });
        Ok(object([("handle", string(handle))]))
    }

    async fn stop_scan(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "scanHandle", "tauri.scan-stop")?;
        let key = caller_key(caller);
        {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "scan", "tauri.scan-stop-owner")
            })?;
            match caller_state.scan.as_ref() {
                Some(scan) if scan.handle == handle => scan.task.abort(),
                Some(_) => {
                    return Err(DispatchError::new(
                        "ownership.denied",
                        "scan",
                        "tauri.scan-stop-handle",
                    ))
                }
                None => return Ok(released()),
            }
        }
        self.adapter().await?.stop_scan().await.map_err(|error| {
            DispatchError::new("scan.stop-failed", "scan", "tauri.scan-stop")
                .platform(error.to_string())
        })?;
        let mut state = self.inner.lock().await;
        if let Some(caller_state) = state.callers.get_mut(&key) {
            caller_state.scan.take();
        }
        if state.scan_owner.as_deref() == Some(&key) {
            state.scan_owner = None;
        }
        Ok(released())
    }

    async fn connect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let peer_id = required_string(&payload, "peerId", "tauri.connect-peer")?;
        let adapter = self.adapter().await?;
        let peripherals = adapter.peripherals().await.map_err(|error| {
            DispatchError::new(
                "connection.failed",
                "connection",
                "tauri.connect-peripherals",
            )
            .platform(error.to_string())
        })?;
        let peripheral = peripherals
            .into_iter()
            .find(|peripheral| peripheral_id(peripheral) == peer_id)
            .ok_or_else(|| {
                DispatchError::new("connection.not-found", "connection", "tauri.connect-peer")
            })?;
        let key = caller_key(caller);
        {
            let mut state = self.inner.lock().await;
            if state.peer_owners.contains_key(&peer_id) {
                return Err(DispatchError::new(
                    "connection.already-owned",
                    "connection",
                    "tauri.connect-peer-owner",
                ));
            }
            if !state.callers.contains_key(&key) {
                return Err(DispatchError::new(
                    "ownership.denied",
                    "connection",
                    "tauri.connect-owner",
                ));
            }
            state.peer_owners.insert(peer_id.clone(), key.clone());
        }
        let already_connected = match peripheral.is_connected().await {
            Ok(connected) => connected,
            Err(error) => {
                self.clear_peer_owner(&peer_id, &key).await;
                return Err(DispatchError::new(
                    "connection.failed",
                    "connection",
                    "tauri.connect-state",
                )
                .platform(error.to_string()));
            }
        };
        if already_connected {
            self.clear_peer_owner(&peer_id, &key).await;
            return Err(DispatchError::new(
                "connection.already-owned",
                "connection",
                "tauri.connect-existing-link",
            ));
        }
        if let Err(error) = peripheral.connect().await {
            self.clear_peer_owner(&peer_id, &key).await;
            return Err(
                DispatchError::new("connection.failed", "connection", "tauri.connect")
                    .platform(error.to_string()),
            );
        }
        let handle = self.id("connection");
        let mut state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get_mut(&key) else {
            state.peer_owners.remove(&peer_id);
            drop(state);
            peripheral.disconnect().await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "connection",
                "tauri.connect-owner",
            ));
        };
        caller_state.connections.insert(
            handle.clone(),
            ConnectionResource {
                peer_id: peer_id.clone(),
                peripheral,
            },
        );
        Ok(object([
            ("handle", string(handle)),
            ("peerId", string(peer_id)),
        ]))
    }

    async fn disconnect(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "connectionHandle", "tauri.disconnect")?;
        let key = caller_key(caller);
        let connection = {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "connection", "tauri.disconnect-owner")
            })?;
            caller_state.connections.get(&handle).cloned()
        };
        let Some(connection) = connection else {
            return Ok(released());
        };
        connection.peripheral.disconnect().await.map_err(|error| {
            DispatchError::new("platform.failure", "connection", "tauri.disconnect")
                .platform(error.to_string())
        })?;
        let subscriptions = {
            let mut state = self.inner.lock().await;
            let Some(caller_state) = state.callers.get_mut(&key) else {
                state.peer_owners.remove(&connection.peer_id);
                return Ok(released());
            };
            caller_state.connections.remove(&handle);
            let database_handles = caller_state
                .databases
                .iter()
                .filter_map(|(database_handle, database)| {
                    (database.connection_handle == handle).then_some(database_handle.clone())
                })
                .collect::<HashSet<_>>();
            let subscription_handles = caller_state
                .subscriptions
                .iter()
                .filter_map(|(subscription_handle, subscription)| {
                    database_handles
                        .contains(&subscription.database_handle)
                        .then_some(subscription_handle.clone())
                })
                .collect::<Vec<_>>();
            let subscriptions = subscription_handles
                .into_iter()
                .filter_map(|subscription_handle| {
                    caller_state.subscriptions.remove(&subscription_handle)
                })
                .collect::<Vec<_>>();
            caller_state
                .databases
                .retain(|database_handle, _| !database_handles.contains(database_handle));
            state.peer_owners.remove(&connection.peer_id);
            subscriptions
        };
        for subscription in subscriptions {
            subscription.task.abort();
        }
        Ok(released())
    }

    async fn discover(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let connection_handle = required_string(&payload, "connectionHandle", "tauri.discover")?;
        let peripheral = self
            .connection(caller, &connection_handle)
            .await?
            .peripheral;
        peripheral.discover_services().await.map_err(|error| {
            DispatchError::new("gatt.discovery-required", "gatt", "tauri.discover")
                .platform(error.to_string())
        })?;
        let database_handle = self.id("database");
        let mut characteristic_map = HashMap::new();
        let mut descriptor_map = HashMap::new();
        let mut characteristic_records = Vec::new();
        let mut descriptor_records = Vec::new();
        let mut occurrences: HashMap<(Uuid, Uuid), usize> = HashMap::new();
        for characteristic in peripheral.characteristics() {
            let occurrence = occurrences
                .entry((characteristic.service_uuid, characteristic.uuid))
                .or_default();
            let handle = self.id("characteristic");
            characteristic_records.push(object([
                ("handle", string(handle.clone())),
                (
                    "serviceUuid",
                    string(characteristic.service_uuid.to_string()),
                ),
                ("serviceOccurrence", string("0")),
                (
                    "characteristicUuid",
                    string(characteristic.uuid.to_string()),
                ),
                ("characteristicOccurrence", string(occurrence.to_string())),
                (
                    "properties",
                    characteristic_properties(characteristic.properties),
                ),
            ]));
            *occurrence += 1;
            for (index, descriptor) in characteristic.descriptors.iter().enumerate() {
                let descriptor_handle = self.id("descriptor");
                descriptor_records.push(object([
                    ("handle", string(descriptor_handle.clone())),
                    ("characteristicHandle", string(handle.clone())),
                    ("uuid", string(descriptor.uuid.to_string())),
                    ("occurrence", string(index.to_string())),
                ]));
                descriptor_map.insert(descriptor_handle, descriptor.clone());
            }
            characteristic_map.insert(handle, characteristic);
        }
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.discover-owner")
        })?;
        caller_state.databases.insert(
            database_handle.clone(),
            DatabaseResource {
                connection_handle,
                characteristics: characteristic_map,
                descriptors: descriptor_map,
            },
        );
        Ok(object([
            ("handle", string(database_handle)),
            ("characteristics", IpcValue::Array(characteristic_records)),
            ("descriptors", IpcValue::Array(descriptor_records)),
        ]))
    }

    async fn read(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let (peripheral, characteristic) = self.characteristic(caller, &payload).await?;
        let value = peripheral.read(&characteristic).await.map_err(|error| {
            DispatchError::new("gatt.read-failed", "gatt", "tauri.gatt-read")
                .platform(error.to_string())
        })?;
        Ok(object([("value", IpcValue::Bytes(value))]))
    }

    async fn write(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        let bytes = bytes
            .ok_or_else(|| DispatchError::new("bytes.invalid", "gatt", "tauri.gatt-write-bytes"))?;
        let mode = required_string(&payload, "mode", "tauri.gatt-write-mode")?;
        let write_type = match mode.as_str() {
            "with-response" => WriteType::WithResponse,
            "without-response" => WriteType::WithoutResponse,
            _ => {
                return Err(DispatchError::new(
                    "argument.invalid",
                    "gatt",
                    "tauri.gatt-write-mode",
                ))
            }
        };
        let (peripheral, characteristic) = self.characteristic(caller, &payload).await?;
        peripheral
            .write(&characteristic, &bytes, write_type)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.write-failed", "gatt", "tauri.gatt-write")
                    .platform(error.to_string())
            })?;
        Ok(object([
            ("commitState", string("committed")),
            ("outcome", string("succeeded")),
        ]))
    }

    async fn subscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let database_handle =
            required_string(&payload, "databaseHandle", "tauri.subscribe-database")?;
        let (peripheral, characteristic) = self.characteristic(caller, &payload).await?;
        let mut notifications = peripheral.notifications().await.map_err(|error| {
            DispatchError::new("gatt.subscribe-failed", "gatt", "tauri.notifications")
                .platform(error.to_string())
        })?;
        peripheral
            .subscribe(&characteristic)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.subscribe-failed", "gatt", "tauri.subscribe")
                    .platform(error.to_string())
            })?;
        let handle = self.id("subscription");
        let stream_handle = handle.clone();
        let key = caller_key(caller);
        let uuid = characteristic.uuid;
        let dispatcher = self.clone();
        let task = tauri::async_runtime::spawn(async move {
            while let Some(notification) = notifications.next().await {
                if notification.uuid != uuid {
                    continue;
                }
                if dispatcher
                    .emit(&key, &stream_handle, IpcValue::Bytes(notification.value))
                    .await
                    .is_err()
                {
                    dispatcher
                        .terminal(&key, &stream_handle, "source-failed")
                        .await
                        .ok();
                    dispatcher
                        .fail_subscription_stream(&key, &stream_handle)
                        .await;
                    return;
                }
            }
            dispatcher
                .terminal(&key, &stream_handle, "source-failed")
                .await
                .ok();
            dispatcher
                .fail_subscription_stream(&key, &stream_handle)
                .await;
        });
        let mut state = self.inner.lock().await;
        let Some(caller_state) = state.callers.get_mut(&caller_key(caller)) else {
            task.abort();
            drop(state);
            peripheral.unsubscribe(&characteristic).await.ok();
            return Err(DispatchError::new(
                "ownership.denied",
                "gatt",
                "tauri.subscribe-owner",
            ));
        };
        caller_state.subscriptions.insert(
            handle.clone(),
            SubscriptionResource {
                database_handle,
                peripheral,
                characteristic,
                task,
            },
        );
        Ok(object([("handle", string(handle))]))
    }

    async fn unsubscribe(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "subscriptionHandle", "tauri.unsubscribe")?;
        let subscription = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(&caller_key(caller)).ok_or_else(|| {
                DispatchError::new("ownership.denied", "gatt", "tauri.unsubscribe-owner")
            })?;
            caller_state.subscriptions.remove(&handle)
        };
        if let Some(subscription) = subscription {
            subscription.task.abort();
            subscription
                .peripheral
                .unsubscribe(&subscription.characteristic)
                .await
                .map_err(|error| {
                    DispatchError::new("gatt.subscribe-failed", "gatt", "tauri.unsubscribe")
                        .platform(error.to_string())
                })?;
        }
        Ok(released())
    }

    async fn read_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let (peripheral, descriptor) = self.descriptor(caller, &payload).await?;
        let value = peripheral
            .read_descriptor(&descriptor)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.read-failed", "gatt", "tauri.descriptor-read")
                    .platform(error.to_string())
            })?;
        Ok(object([("value", IpcValue::Bytes(value))]))
    }

    async fn write_descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        let bytes = bytes.ok_or_else(|| {
            DispatchError::new("bytes.invalid", "gatt", "tauri.descriptor-write-bytes")
        })?;
        let (peripheral, descriptor) = self.descriptor(caller, &payload).await?;
        peripheral
            .write_descriptor(&descriptor, &bytes)
            .await
            .map_err(|error| {
                DispatchError::new("gatt.write-failed", "gatt", "tauri.descriptor-write")
                    .platform(error.to_string())
            })?;
        Ok(object([
            ("commitState", string("committed")),
            ("outcome", string("succeeded")),
        ]))
    }

    async fn read_rssi(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "connectionHandle", "tauri.rssi")?;
        let peripheral = self.connection(caller, &handle).await?.peripheral;
        let rssi = peripheral.read_rssi().await.map_err(|error| {
            DispatchError::new("platform.failure", "connection", "tauri.rssi")
                .platform(error.to_string())
        })?;
        Ok(object([("rssi", number(i64::from(rssi)))]))
    }

    async fn maximum_write_length(
        &self,
        caller: &AuthenticatedCaller,
        payload: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let handle = required_string(&payload, "connectionHandle", "tauri.maximum-write-length")?;
        let peripheral = self.connection(caller, &handle).await?.peripheral;
        let bytes = peripheral.mtu().saturating_sub(3);
        Ok(object([("bytes", number(i64::from(bytes)))]))
    }

    async fn adapter(&self) -> Result<Adapter, DispatchError> {
        self.inner
            .lock()
            .await
            .adapter
            .clone()
            .ok_or_else(|| DispatchError::new("adapter.unavailable", "adapter", "tauri.adapter"))
    }

    async fn clear_peer_owner(&self, peer_id: &str, owner: &str) {
        let mut state = self.inner.lock().await;
        if state
            .peer_owners
            .get(peer_id)
            .is_some_and(|value| value == owner)
        {
            state.peer_owners.remove(peer_id);
        }
    }

    async fn connection(
        &self,
        caller: &AuthenticatedCaller,
        handle: &str,
    ) -> Result<ConnectionResource, DispatchError> {
        self.inner
            .lock()
            .await
            .callers
            .get(&caller_key(caller))
            .and_then(|caller_state| caller_state.connections.get(handle))
            .cloned()
            .ok_or_else(|| {
                DispatchError::new(
                    "connection.not-found",
                    "connection",
                    "tauri.connection-handle",
                )
            })
    }

    async fn characteristic(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<(Peripheral, Characteristic), DispatchError> {
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.characteristic-database")?;
        let characteristic_handle = required_string(
            payload,
            "characteristicHandle",
            "tauri.characteristic-handle",
        )?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.characteristic-owner")
        })?;
        let database = caller_state
            .databases
            .get(&database_handle)
            .ok_or_else(|| {
                DispatchError::new("gatt.stale-handle", "gatt", "tauri.characteristic-database")
            })?;
        let characteristic = database
            .characteristics
            .get(&characteristic_handle)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new("gatt.not-found", "gatt", "tauri.characteristic-handle")
            })?;
        let peripheral = caller_state
            .connections
            .get(&database.connection_handle)
            .map(|connection| connection.peripheral.clone())
            .ok_or_else(|| {
                DispatchError::new(
                    "connection.stale",
                    "connection",
                    "tauri.characteristic-connection",
                )
            })?;
        Ok((peripheral, characteristic))
    }

    async fn descriptor(
        &self,
        caller: &AuthenticatedCaller,
        payload: &BTreeMap<String, IpcValue>,
    ) -> Result<(Peripheral, Descriptor), DispatchError> {
        let database_handle =
            required_string(payload, "databaseHandle", "tauri.descriptor-database")?;
        let descriptor_handle =
            required_string(payload, "descriptorHandle", "tauri.descriptor-handle")?;
        let state = self.inner.lock().await;
        let caller_state = state.callers.get(&caller_key(caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "gatt", "tauri.descriptor-owner")
        })?;
        let database = caller_state
            .databases
            .get(&database_handle)
            .ok_or_else(|| {
                DispatchError::new("gatt.stale-handle", "gatt", "tauri.descriptor-database")
            })?;
        let descriptor = database
            .descriptors
            .get(&descriptor_handle)
            .cloned()
            .ok_or_else(|| {
                DispatchError::new("gatt.not-found", "gatt", "tauri.descriptor-handle")
            })?;
        let peripheral = caller_state
            .connections
            .get(&database.connection_handle)
            .map(|connection| connection.peripheral.clone())
            .ok_or_else(|| {
                DispatchError::new(
                    "connection.stale",
                    "connection",
                    "tauri.descriptor-connection",
                )
            })?;
        Ok((peripheral, descriptor))
    }

    async fn acknowledge(
        &self,
        caller: AuthenticatedCaller,
        request: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let event_id = required_string(&request, "eventId", "tauri.event-ack")?;
        let lease = required_lease(&request, "tauri.event-ack-lease")?;
        let mut state = self.inner.lock().await;
        let caller_state = state.callers.get_mut(&caller_key(&caller)).ok_or_else(|| {
            DispatchError::new("ownership.denied", "ipc", "tauri.event-ack-owner")
        })?;
        validate_lease(caller_state, &lease, "tauri.event-ack-lease")?;
        if !caller_state.pending_events.remove(&event_id) {
            return Err(DispatchError::new(
                "protocol.violation",
                "ipc",
                "tauri.event-ack-id",
            ));
        }
        Ok(object([("kind", string("event.ack"))]))
    }

    async fn release_request(
        &self,
        caller: AuthenticatedCaller,
        request: BTreeMap<String, IpcValue>,
    ) -> Result<IpcValue, DispatchError> {
        let lease = required_lease(&request, "tauri.release-lease")?;
        let key = caller_key(&caller);
        {
            let state = self.inner.lock().await;
            let caller_state = state.callers.get(&key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "ipc", "tauri.release-owner")
            })?;
            validate_lease(caller_state, &lease, "tauri.release-lease")?;
        }
        let cleanup = self.release(&key).await;
        Ok(object([("kind", string("release")), ("cleanup", cleanup)]))
    }

    async fn emit(
        &self,
        caller_key: &str,
        stream_id: &str,
        value: IpcValue,
    ) -> Result<(), DispatchError> {
        let (sink, lease_id, lease_generation, event_id) = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "stream", "tauri.event-owner")
            })?;
            if caller_state.pending_events.len() >= MAX_PENDING_EVENTS {
                return Err(DispatchError::new(
                    "stream.quota",
                    "stream",
                    "tauri.event-retention",
                ));
            }
            let event_id = self.id("event");
            caller_state.pending_events.insert(event_id.clone());
            (
                caller_state.event_sink.clone(),
                caller_state.lease_id.clone(),
                caller_state.lease_generation.clone(),
                event_id,
            )
        };
        sink.send(object([
            (
                "rendererLease",
                object([
                    ("leaseId", string(lease_id)),
                    ("generation", string(lease_generation)),
                ]),
            ),
            ("eventId", string(event_id)),
            ("streamId", string(stream_id)),
            (
                "item",
                object([("kind", string("value")), ("value", value)]),
            ),
        ]))
        .map_err(|error| {
            DispatchError::new("platform.transport", "stream", "tauri.event-send")
                .platform(error.to_string())
        })
    }

    async fn terminal(
        &self,
        caller_key: &str,
        stream_id: &str,
        reason: &str,
    ) -> Result<(), DispatchError> {
        let (sink, lease_id, lease_generation, event_id) = {
            let mut state = self.inner.lock().await;
            let caller_state = state.callers.get_mut(caller_key).ok_or_else(|| {
                DispatchError::new("ownership.denied", "stream", "tauri.terminal-owner")
            })?;
            let event_id = self.id("event-terminal");
            caller_state.pending_events.insert(event_id.clone());
            (
                caller_state.event_sink.clone(),
                caller_state.lease_id.clone(),
                caller_state.lease_generation.clone(),
                event_id,
            )
        };
        sink.send(object([
            (
                "rendererLease",
                object([
                    ("leaseId", string(lease_id)),
                    ("generation", string(lease_generation)),
                ]),
            ),
            ("eventId", string(event_id)),
            ("streamId", string(stream_id)),
            (
                "item",
                object([("kind", string("terminal")), ("reason", string(reason))]),
            ),
        ]))
        .map_err(|error| {
            DispatchError::new("platform.transport", "stream", "tauri.terminal-send")
                .platform(error.to_string())
        })
    }

    async fn fail_scan_stream(&self, caller_key: &str, stream_id: &str) {
        let should_stop = {
            let mut state = self.inner.lock().await;
            let scan = state
                .callers
                .get_mut(caller_key)
                .and_then(|caller| caller.scan.take());
            let owned = scan.as_ref().is_some_and(|scan| scan.handle == stream_id);
            if !owned {
                if let Some(scan) = scan {
                    if let Some(caller) = state.callers.get_mut(caller_key) {
                        caller.scan = Some(scan);
                    }
                }
                false
            } else {
                if state.scan_owner.as_deref() == Some(caller_key) {
                    state.scan_owner = None;
                }
                true
            }
        };
        if should_stop {
            if let Ok(adapter) = self.adapter().await {
                adapter.stop_scan().await.ok();
            }
        }
    }

    async fn fail_subscription_stream(&self, caller_key: &str, stream_id: &str) {
        let subscription = self
            .inner
            .lock()
            .await
            .callers
            .get_mut(caller_key)
            .and_then(|caller| caller.subscriptions.remove(stream_id));
        if let Some(subscription) = subscription {
            subscription
                .peripheral
                .unsubscribe(&subscription.characteristic)
                .await
                .ok();
        }
    }

    async fn release(&self, key: &str) -> IpcValue {
        let caller = self.inner.lock().await.callers.remove(key);
        let Some(mut caller) = caller else {
            return released();
        };
        let cleanup = self.settle_caller(key, &mut caller).await;
        if !is_released(&cleanup) {
            let mut state = self.inner.lock().await;
            state.callers.entry(key.to_owned()).or_insert(caller);
        }
        cleanup
    }

    fn is_revoked(&self, key: &str) -> bool {
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .contains_key(key)
    }

    async fn release_revoked(&self, key: String, revocation: u64) {
        if self
            .revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .get(&key)
            .copied()
            != Some(revocation)
        {
            return;
        }
        let cleanup = self.release(&key).await;
        if is_released(&cleanup) {
            let mut revoked = self
                .revoked_callers
                .lock()
                .expect("revocation mutex poisoned");
            if revoked.get(&key).copied() == Some(revocation) {
                revoked.remove(&key);
            }
        }
    }

    async fn settle_caller(&self, key: &str, caller: &mut CallerState) -> IpcValue {
        let mut failures = Vec::new();
        for cancellation in caller.operations.values() {
            cancellation.cancel();
        }
        caller.operations.clear();
        if let Some(scan) = caller.scan.as_ref() {
            scan.task.abort();
            match self.adapter().await {
                Ok(adapter) => match adapter.stop_scan().await {
                    Ok(()) => {
                        caller.scan.take();
                        let mut state = self.inner.lock().await;
                        if state.scan_owner.as_deref() == Some(key) {
                            state.scan_owner = None;
                        }
                    }
                    Err(error) => failures.push(cleanup_failure(
                        "scan",
                        "tauri.release.scan",
                        error.to_string(),
                    )),
                },
                Err(error) => failures.push(cleanup_failure(
                    "scan",
                    "tauri.release.scan-adapter",
                    error
                        .platform
                        .unwrap_or_else(|| "adapter unavailable".to_owned()),
                )),
            }
        }
        let subscription_handles = caller.subscriptions.keys().cloned().collect::<Vec<_>>();
        for handle in subscription_handles {
            let Some(subscription) = caller.subscriptions.get(&handle) else {
                continue;
            };
            subscription.task.abort();
            match subscription
                .peripheral
                .unsubscribe(&subscription.characteristic)
                .await
            {
                Ok(()) => {
                    caller.subscriptions.remove(&handle);
                }
                Err(error) => failures.push(cleanup_failure(
                    "subscription",
                    "tauri.release.subscription",
                    error.to_string(),
                )),
            }
        }
        let connection_handles = caller.connections.keys().cloned().collect::<Vec<_>>();
        for handle in connection_handles {
            let Some(connection) = caller.connections.get(&handle).cloned() else {
                continue;
            };
            match connection.peripheral.disconnect().await {
                Ok(()) => {
                    caller.connections.remove(&handle);
                    caller
                        .databases
                        .retain(|_, database| database.connection_handle != handle);
                    self.clear_peer_owner(&connection.peer_id, key).await;
                }
                Err(error) => failures.push(cleanup_failure(
                    "connection",
                    "tauri.release.connection",
                    error.to_string(),
                )),
            }
        }
        cleanup_record(failures)
    }
}

impl IpcDispatcher for BtleplugDispatcher {
    fn dispatch<'a>(
        &'a self,
        caller: AuthenticatedCaller,
        request: IpcValue,
        event_sink: IpcEventSink,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            self.dispatch_request(caller, request, event_sink)
                .await
                .unwrap_or_else(DispatchError::into_response)
        })
    }

    fn release_caller(&self, caller: AuthenticatedCaller) {
        let key = caller_key(&caller);
        let revocation = self.next_revocation.fetch_add(1, Ordering::Relaxed);
        self.revoked_callers
            .lock()
            .expect("revocation mutex poisoned")
            .insert(key.clone(), revocation);
        let dispatcher = self.clone();
        tauri::async_runtime::spawn(async move {
            dispatcher.release_revoked(key, revocation).await;
        });
    }
}

fn required_lease(
    request: &BTreeMap<String, IpcValue>,
    operation: &'static str,
) -> Result<(String, String), DispatchError> {
    let lease = into_object(
        required_value(request, "rendererLease", operation)?.clone(),
        operation,
    )?;
    Ok((
        required_string(&lease, "leaseId", operation)?,
        required_string(&lease, "generation", operation)?,
    ))
}

fn validate_lease(
    caller: &CallerState,
    lease: &(String, String),
    operation: &'static str,
) -> Result<(), DispatchError> {
    if caller.lease_id != lease.0 || caller.lease_generation != lease.1 {
        return Err(DispatchError::new("ownership.denied", "ipc", operation));
    }
    Ok(())
}

fn caller_key(caller: &AuthenticatedCaller) -> String {
    format!("{}\0{}", caller.app_identifier, caller.window_label)
}

fn peripheral_id(peripheral: &Peripheral) -> String {
    format!("{:?}", peripheral.id())
}

fn peripheral_observation(
    peripheral: &Peripheral,
    properties: btleplug::api::PeripheralProperties,
) -> IpcValue {
    let manufacturer_data = properties
        .manufacturer_data
        .into_iter()
        .map(|(company_id, data)| {
            object([
                ("companyId", number(i64::from(company_id))),
                ("data", IpcValue::Bytes(data)),
            ])
        })
        .collect();
    let service_data = properties
        .service_data
        .into_iter()
        .map(|(uuid, data)| {
            object([
                ("uuid", string(uuid.to_string())),
                ("data", IpcValue::Bytes(data)),
            ])
        })
        .collect();
    object([
        ("peerId", string(peripheral_id(peripheral))),
        (
            "localName",
            properties.local_name.map_or(IpcValue::Null, string),
        ),
        (
            "rssi",
            properties
                .rssi
                .map_or(IpcValue::Null, |value| number(i64::from(value))),
        ),
        (
            "txPowerLevel",
            properties
                .tx_power_level
                .map_or(IpcValue::Null, |value| number(i64::from(value))),
        ),
        (
            "serviceUuids",
            IpcValue::Array(
                properties
                    .services
                    .into_iter()
                    .map(|uuid| string(uuid.to_string()))
                    .collect(),
            ),
        ),
        ("manufacturerData", IpcValue::Array(manufacturer_data)),
        ("serviceData", IpcValue::Array(service_data)),
    ])
}

fn characteristic_properties(properties: CharPropFlags) -> IpcValue {
    let mut values = Vec::new();
    for (flag, name) in [
        (CharPropFlags::READ, "read"),
        (CharPropFlags::WRITE, "write"),
        (
            CharPropFlags::WRITE_WITHOUT_RESPONSE,
            "write-without-response",
        ),
        (CharPropFlags::NOTIFY, "notify"),
        (CharPropFlags::INDICATE, "indicate"),
    ] {
        if properties.contains(flag) {
            values.push(string(name));
        }
    }
    IpcValue::Array(values)
}

fn parse_uuid(value: &str, operation: &'static str) -> Result<Uuid, DispatchError> {
    let canonical = match value.len() {
        4 => format!("0000{value}-0000-1000-8000-00805f9b34fb"),
        8 => format!("{value}-0000-1000-8000-00805f9b34fb"),
        _ => value.to_owned(),
    };
    Uuid::parse_str(&canonical)
        .map_err(|_| DispatchError::new("scan.filter-invalid", "scan", operation))
}

struct ManufacturerFilter {
    company_id: u16,
    data_prefix: Option<Vec<u8>>,
}

fn manufacturer_filters(
    payload: &BTreeMap<String, IpcValue>,
) -> Result<Vec<ManufacturerFilter>, DispatchError> {
    let Some(value) = payload.get("manufacturerData") else {
        return Ok(Vec::new());
    };
    let IpcValue::Array(filters) = value else {
        return Err(DispatchError::new(
            "scan.filter-invalid",
            "scan",
            "tauri.scan-manufacturer-filter",
        ));
    };
    filters
        .iter()
        .map(|filter| {
            let IpcValue::Object(filter) = filter else {
                return Err(DispatchError::new(
                    "scan.filter-invalid",
                    "scan",
                    "tauri.scan-manufacturer-filter",
                ));
            };
            let company_id = match filter.get("companyId") {
                Some(IpcValue::Number(value)) => value
                    .as_u64()
                    .and_then(|value| u16::try_from(value).ok())
                    .ok_or_else(|| {
                        DispatchError::new(
                            "scan.filter-invalid",
                            "scan",
                            "tauri.scan-manufacturer-company",
                        )
                    })?,
                _ => {
                    return Err(DispatchError::new(
                        "scan.filter-invalid",
                        "scan",
                        "tauri.scan-manufacturer-company",
                    ))
                }
            };
            let data_prefix = match filter.get("dataPrefix") {
                None | Some(IpcValue::Null) => None,
                Some(IpcValue::Bytes(bytes)) => Some(bytes.clone()),
                _ => {
                    return Err(DispatchError::new(
                        "scan.filter-invalid",
                        "scan",
                        "tauri.scan-manufacturer-prefix",
                    ))
                }
            };
            Ok(ManufacturerFilter {
                company_id,
                data_prefix,
            })
        })
        .collect()
}

fn attachment_record(attachment: &Attachment) -> IpcValue {
    object([
        ("attachmentId", string(attachment.attachment_id.clone())),
        (
            "backendInstanceId",
            string(attachment.backend_instance_id.clone()),
        ),
        (
            "backendGeneration",
            string(attachment.backend_generation.clone()),
        ),
        (
            "adapter",
            object([
                ("adapterId", string(attachment.adapter_id.clone())),
                ("displayName", string(attachment.adapter_name.clone())),
                ("state", adapter_state(attachment)),
                (
                    "adapterGeneration",
                    string(attachment.adapter_generation.clone()),
                ),
                ("limitations", IpcValue::Array(Vec::new())),
            ]),
        ),
    ])
}

fn adapter_state(attachment: &Attachment) -> IpcValue {
    object([
        ("availability", string("available")),
        ("authorization", string("not-determined")),
        ("power", string("unknown")),
        (
            "backendGeneration",
            string(attachment.backend_generation.clone()),
        ),
        ("updatedAt", number(0)),
        (
            "safeReason",
            string(
                "Host APIs do not expose a portable pre-operation power/authorization snapshot.",
            ),
        ),
    ])
}

fn ipc_versions() -> IpcValue {
    object([
        ("backendContract", negotiated("backend-contract")),
        ("capabilitySchema", negotiated("capability-schema")),
        ("eventSchema", negotiated("event-schema")),
        ("traceFormat", negotiated("trace-format")),
        ("ipcProtocol", negotiated("ipc-protocol")),
    ])
}

fn negotiated(axis: &str) -> IpcValue {
    let range = object([("minimum", number(1)), ("maximum", number(1))]);
    object([
        ("axis", string(axis)),
        ("selected", number(1)),
        ("localRange", range.clone()),
        ("remoteRange", range),
    ])
}

fn route_response(payload: IpcValue) -> IpcValue {
    object([("kind", string("route")), ("payload", payload)])
}

fn released() -> IpcValue {
    object([
        ("state", string("released")),
        ("failures", IpcValue::Array(Vec::new())),
    ])
}

fn cleanup_record(failures: Vec<IpcValue>) -> IpcValue {
    object([
        (
            "state",
            string(if failures.is_empty() {
                "released"
            } else {
                "release-failed"
            }),
        ),
        ("failures", IpcValue::Array(failures)),
    ])
}

fn cleanup_failure(resource_kind: &str, operation: &str, message: String) -> IpcValue {
    object([
        ("resourceKind", string(resource_kind)),
        (
            "error",
            object([
                ("code", string("platform.failure")),
                ("domain", string("cleanup")),
                ("operation", string(operation)),
                (
                    "platform",
                    object([
                        ("domain", string("btleplug")),
                        ("code", string("cleanup-failed")),
                        ("safeMessage", string(message)),
                        ("metadata", object([])),
                    ]),
                ),
                ("retryability", string("caller-decides")),
            ]),
        ),
    ])
}

fn is_released(value: &IpcValue) -> bool {
    matches!(
        value,
        IpcValue::Object(record)
            if matches!(record.get("state"), Some(IpcValue::String(state)) if state == "released")
    )
}

fn object<const N: usize>(entries: [(&str, IpcValue); N]) -> IpcValue {
    IpcValue::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn string(value: impl Into<String>) -> IpcValue {
    IpcValue::String(value.into())
}

fn as_string(value: &IpcValue) -> Option<&str> {
    match value {
        IpcValue::String(value) => Some(value),
        _ => None,
    }
}

fn number(value: i64) -> IpcValue {
    IpcValue::Number(Number::from(value))
}

fn into_object(
    value: IpcValue,
    operation: impl Into<String>,
) -> Result<BTreeMap<String, IpcValue>, DispatchError> {
    match value {
        IpcValue::Object(object) => Ok(object),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

fn required_value<'a>(
    object: &'a BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<&'a IpcValue, DispatchError> {
    object
        .get(key)
        .ok_or_else(|| DispatchError::new("protocol.malformed", "ipc", operation))
}

fn required_string(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<String, DispatchError> {
    match object.get(key) {
        Some(IpcValue::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

fn optional_string_array(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<Vec<String>, DispatchError> {
    let operation = operation.into();
    match object.get(key) {
        None => Ok(Vec::new()),
        Some(IpcValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                IpcValue::String(value) => Ok(value.clone()),
                _ => Err(DispatchError::new(
                    "protocol.malformed",
                    "ipc",
                    operation.clone(),
                )),
            })
            .collect(),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

fn optional_nullable_string(
    object: &BTreeMap<String, IpcValue>,
    key: &str,
    operation: impl Into<String>,
) -> Result<Option<String>, DispatchError> {
    match object.get(key) {
        None | Some(IpcValue::Null) => Ok(None),
        Some(IpcValue::String(value)) => Ok(Some(value.clone())),
        _ => Err(DispatchError::new("protocol.malformed", "ipc", operation)),
    }
}

#[cfg(test)]
mod tests {
    use super::{characteristic_properties, negotiated, object, released, string};
    use btleplug::api::CharPropFlags;

    #[test]
    fn capability_projection_is_data_only() {
        assert_eq!(
            characteristic_properties(CharPropFlags::READ | CharPropFlags::NOTIFY),
            super::IpcValue::Array(vec![string("read"), string("notify")])
        );
    }

    #[test]
    fn release_receipt_and_version_offer_are_explicit() {
        assert!(matches!(released(), super::IpcValue::Object(_)));
        assert_eq!(
            negotiated("ipc-protocol"),
            object([
                ("axis", string("ipc-protocol")),
                ("selected", super::number(1)),
                (
                    "localRange",
                    object([("minimum", super::number(1)), ("maximum", super::number(1))])
                ),
                (
                    "remoteRange",
                    object([("minimum", super::number(1)), ("maximum", super::number(1))])
                )
            ])
        );
    }
}
