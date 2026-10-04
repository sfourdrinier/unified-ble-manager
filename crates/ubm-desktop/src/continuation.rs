//! Process-owned native continuation. Native hosts share this owner; no
//! JavaScript session is necessary to connect, subscribe, or retain values.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::continuation_journal::{ContinuationJournal, JournalQuota, JournalRegistry};
use crate::continuation_outbox::{Observation, RecordMatcher, decode_base64, encode_base64};
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub type Result<T> = std::result::Result<T, Value>;
pub type ContinuationFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

pub trait ContinuationSession: Send + Sync {
    fn seal_collection(&self) -> Result<()> {
        Err(failure(
            "capability.unsupported",
            "session cannot seal native collection",
        ))
    }
    fn collection_sealed(&self) -> bool {
        false
    }
    fn attach_journal(&self, _: Arc<ContinuationJournal>, _: Value) -> Result<()> {
        Err(failure(
            "capability.unsupported",
            "session cannot collect durably",
        ))
    }
    fn register_journal_consumer(&self, _: &str, _: Value) -> Result<()> {
        Err(failure(
            "capability.unsupported",
            "session cannot register durable consumer metadata",
        ))
    }
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a>;
    fn drain(&self, max_items: u32, max_bytes: u32) -> ContinuationFuture<'_>;
    fn observe(&self, _: &str, _: RecordMatcher) -> Result<Observation> {
        Err(failure(
            "capability.unsupported",
            "session cannot observe setup acknowledgements",
        ))
    }
}

pub trait ContinuationHost: Send + Sync {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>>;
    /// Host identities are opaque by default. Only the owning platform may
    /// canonicalize a representation it explicitly defines as equivalent.
    fn canonical_peer(&self, peer: &str) -> String {
        peer.to_owned()
    }
}

fn failure(code: &str, detail: &str) -> Value {
    json!({"code":code,"domain":"restoration","operation":"continuation",
        "detail":detail})
}

/// Safe public error identity for durable storage. Never includes a path,
/// SQL text, declaration contents, or sensor bytes.
pub fn recording_failure(error: crate::continuation_journal::JournalError) -> Value {
    let mut metadata = json!({"storageKind":error.kind,"operation":error.operation});
    if let Some(code) = error.sqlite_extended_code {
        metadata["sqliteExtendedCode"] = json!(code);
    }
    if let Some(code) = error.sqlite_code {
        metadata["sqliteCode"] = json!(code);
    }
    json!({"code":if error.kind=="argument.invalid" {"argument.invalid"} else {"platform.failure"},"domain":"platform","operation":"continuation.recording","detail":error.detail,"platform":{"domain":"sqlite","code":error.kind,"message":error.detail,"metadata":metadata}})
}

fn invalid(detail: &str) -> Value {
    failure("argument.invalid", detail)
}
fn busy() -> Value {
    failure(
        "lifecycle.invalid-state",
        "continuation execution or handoff is in progress",
    )
}

const MAX_SETUP_FAILURE_PEERS: usize = 4096;

#[derive(Default)]
struct SetupFailures {
    peers: std::collections::HashMap<String, SetupFailure>,
}

struct SetupFailure {
    generation: Value,
    error: Value,
}

impl SetupFailures {
    fn check(&mut self, peer: &str, generation: &Value) -> Result<()> {
        if let Some(previous) = self.peers.get(peer) {
            if &previous.generation == generation {
                return Err(previous.error.clone());
            }
            // Only this peer's authoritative generation change retires its
            // uncertainty. A claim or another peer's execution cannot do so.
            self.peers.remove(peer);
        }
        if self.peers.len() >= MAX_SETUP_FAILURE_PEERS {
            let mut error = failure(
                "lifecycle.invalid-state",
                "setup failure history is full; an authoritative generation change is required",
            );
            error["retryability"] = json!("never");
            return Err(error);
        }
        Ok(())
    }

    fn record(&mut self, peer: &str, generation: Value, error: Value) {
        // check and record share the executor's state lock. Reserve capacity
        // before any write, never evict an unresolved physical generation.
        self.peers
            .insert(peer.to_owned(), SetupFailure { generation, error });
    }
}

#[derive(Default)]
pub(crate) struct State {
    session: Option<Arc<dyn ContinuationSession>>,
    peer: Option<String>,
    selectors: Vec<Value>,
    history: Vec<Value>,
    consumers: Vec<ActiveConsumer>,
    admission: u64,
    token: u64,
    prepared: Option<Prepared>,
    sealed: bool,
    execution_declaration: Option<Value>,
    setup_generation: Option<Value>,
    setup_complete: bool,
    setup_failures: SetupFailures,
    link_generation: Option<String>,
    link_outcome: Option<Value>,
    link_failure: Option<Value>,
    recording: Option<String>,
    recording_attached: bool,
}

#[derive(Default)]
struct DeclarationAuthority {
    committed: Option<Value>,
    reservation: Option<DeclarationReservation>,
    sequence: u64,
}

struct DeclarationReservation {
    token: String,
    declaration: Value,
}

struct ActiveConsumer {
    selector_index: usize,
    consumer: String,
}

struct Prepared {
    claim: Value,
    complete: bool,
    acknowledged: bool,
    receipt: Option<Value>,
}

/// One bounded native standing order per process. Public operations fail promptly
/// when another operation owns admission; the internal supervisor waits fairly.
#[derive(Clone)]
pub struct NativeContinuation {
    host: Arc<dyn ContinuationHost>,
    state: Arc<Mutex<State>>,
    // Published only while holding state admission, alongside session install
    // or confirmed disposal. Idle OS events must not contend for that lock.
    session_owned: Arc<AtomicBool>,
    recovery_requested: Arc<AtomicBool>,
    recovering: Arc<AtomicBool>,
    recovery_stopped: Arc<AtomicBool>,
    last_recovery: Arc<std::sync::Mutex<Option<Value>>>,
    recovery_wake: Arc<tokio::sync::Notify>,
    recovery_idle: Arc<tokio::sync::Notify>,
    declarations: Arc<std::sync::Mutex<DeclarationAuthority>>,
    recovery_runtime: Arc<std::sync::OnceLock<tokio::runtime::Handle>>,
    recordings: Arc<JournalRegistry>,
    recording_sessions: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Weak<dyn ContinuationSession>>,
        >,
    >,
}

impl NativeContinuation {
    async fn check_recording_admission(&self, state: &State) -> Result<()> {
        // An attempted journal open can fail before attachment. Its retry must
        // reach open again; only an attached collection has a status to fence.
        if !state.recording_attached {
            return Ok(());
        }
        let Some(id) = state.recording.clone() else {
            return Ok(());
        };
        let executor = self.clone();
        let session = state.session.clone();
        crate::continuation_journal::run_blocking_result(move || {
            let status = executor.recording_status(&id)?;
            if status["accepting"] != true {
                if let Some(session) = &session {
                    session.seal_collection()?;
                }
                executor.stop_recovery();
                return Err(failure(
                    "lifecycle.invalid-state",
                    "native recording collection is stopped",
                ));
            }
            Ok(())
        })
        .await
    }
    pub fn configure_recording_directory(&self, path: &std::path::Path) -> Result<Value> {
        self.recordings
            .configure_directory(path)
            .map_err(recording_failure)
    }
    pub fn recording_status(&self, id: &str) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.status())
            .map_err(recording_failure)
    }
    pub fn recording_prepare(&self, id: &str, max_items: u32, max_bytes: u32) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.prepare(max_items, max_bytes))
            .map_err(recording_failure)
    }
    pub fn recording_acknowledge(&self, id: &str, token: &str) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.acknowledge(token))
            .map_err(recording_failure)
    }
    pub fn recording_stop(&self, id: &str) -> Result<Value> {
        let sessions = self
            .recording_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let journal = self.recordings.get(id).map_err(recording_failure)?;
        if let Some(session) = sessions.get(id).and_then(std::sync::Weak::upgrade) {
            session.seal_collection()?;
            self.stop_recovery();
        }
        let mut receipt = journal.stop().map_err(recording_failure)?;
        receipt["radioRelease"] = json!("not-requested");
        Ok(receipt)
    }
    pub fn recording_clear(&self, id: &str) -> Result<Value> {
        self.recordings
            .get(id)
            .and_then(|journal| journal.clear())
            .map_err(recording_failure)
    }
    pub fn new(host: Arc<dyn ContinuationHost>) -> Self {
        Self::new_with_recording_registry(host, Arc::default())
    }
    pub fn recording_registry(&self) -> Arc<JournalRegistry> {
        self.recordings.clone()
    }
    /// Inject a process-owned registry before publishing this executor. There
    /// is deliberately no setter that could split already cloned authority.
    pub fn new_with_recording_registry(
        host: Arc<dyn ContinuationHost>,
        recordings: Arc<JournalRegistry>,
    ) -> Self {
        Self {
            host,
            state: Arc::default(),
            session_owned: Arc::default(),
            recovery_requested: Arc::default(),
            recovering: Arc::default(),
            recovery_stopped: Arc::default(),
            last_recovery: Arc::default(),
            recovery_wake: Arc::default(),
            recovery_idle: Arc::default(),
            declarations: Arc::default(),
            recovery_runtime: Arc::default(),
            recordings,
            recording_sessions: Arc::default(),
        }
    }

    /// Validate and pin a declaration before native execution has any effect.
    fn parse(&self, peer: &str, text: &str) -> Result<Vec<Value>> {
        if peer.is_empty() || peer.len() > 512 || text.len() > 1 << 20 {
            return Err(invalid("invalid peer or oversized declaration"));
        }
        let root: Value =
            serde_json::from_str(text).map_err(|_| invalid("malformed declaration"))?;
        let object = root
            .as_object()
            .ok_or_else(|| invalid("declaration must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "onAppearance" | "peerId" | "resubscribe" | "setup" | "link" | "recording"
            )
        }) || root["onAppearance"] != "native"
        {
            return Err(invalid(
                "native declaration contains invalid fields or strategy",
            ));
        }
        if object.contains_key("peerId")
            && !root["peerId"].as_str().is_some_and(|declared| {
                self.host.canonical_peer(declared) == self.host.canonical_peer(peer)
            })
        {
            return Err(invalid("wake peer does not match declaration"));
        }
        let selectors = match object.get("resubscribe") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| invalid("resubscribe must be an array"))?
                .clone(),
        };
        if selectors.len() > 64 {
            return Err(invalid("too many selectors"));
        }
        selectors
            .into_iter()
            .map(|mut selector| {
                let entry = selector
                    .as_object_mut()
                    .ok_or_else(|| invalid("selector must be an object"))?;
                if entry.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "serviceUuid"
                            | "serviceOccurrence"
                            | "characteristicUuid"
                            | "characteristicOccurrence"
                    )
                }) {
                    return Err(invalid("unknown selector field"));
                }
                for field in ["serviceUuid", "characteristicUuid"] {
                    let uuid = entry
                        .get(field)
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("missing UUID"))?;
                    if uuid.len() != 36
                        || !uuid.bytes().enumerate().all(|(index, byte)| {
                            if [8, 13, 18, 23].contains(&index) {
                                byte == b'-'
                            } else {
                                byte.is_ascii_hexdigit()
                            }
                        })
                    {
                        return Err(invalid("UUID must be canonical"));
                    }
                    entry.insert(field.to_owned(), json!(uuid.to_ascii_lowercase()));
                }
                for field in ["serviceOccurrence", "characteristicOccurrence"] {
                    let occurrence = match entry.get(field) {
                        None => 1,
                        Some(value) => value
                            .as_u64()
                            .ok_or_else(|| invalid("invalid occurrence"))?,
                    };
                    if occurrence == 0 || occurrence > (1 << 53) - 1 {
                        return Err(invalid("invalid occurrence"));
                    }
                    entry.insert(field.to_owned(), json!(occurrence));
                }
                Ok(selector)
            })
            .collect()
    }

    fn setup(&self, root: &Value, subscription_count: usize) -> Result<Vec<Value>> {
        let Some(input) = root.get("setup") else {
            return Ok(Vec::new());
        };
        let steps = input
            .as_array()
            .ok_or_else(|| invalid("setup must be an array"))?;
        if steps.len() > 16 {
            return Err(invalid("too many setup steps"));
        }
        let mut total = 0;
        let mut normalized = Vec::new();
        for step in steps {
            exact(step, &["selector", "value", "timeoutMs", "response"])?;
            let timeout = integer(&step["timeoutMs"], 1, 20000)?;
            total += timeout;
            if total > 60000 {
                return Err(invalid("setup deadline exceeds 60000ms"));
            }
            bytes(&step["value"])?;
            let selector = self
                .parse(
                    "setup",
                    &json!({"onAppearance":"native","resubscribe":[step["selector"].clone()]})
                        .to_string(),
                )?
                .remove(0);
            if let Some(response) = step.get("response") {
                exact(
                    response,
                    &[
                        "subscriptionIndex",
                        "prefix",
                        "minLength",
                        "maxLength",
                        "status",
                        "trailing",
                    ],
                )?;
                if subscription_count == 0 {
                    return Err(invalid("setup response has no subscription"));
                }
                integer(
                    &response["subscriptionIndex"],
                    0,
                    subscription_count as u64 - 1,
                )?;
                let prefix = bytes(&response["prefix"])?;
                let min = integer(&response["minLength"], prefix.len() as u64, 512)?;
                integer(&response["maxLength"], min, 512)?;
                exact(&response["status"], &["offset", "accepted"])?;
                integer(
                    &response["status"]["offset"],
                    prefix.len() as u64,
                    min.saturating_sub(1),
                )?;
                let accepted = response["status"]["accepted"]
                    .as_array()
                    .ok_or_else(|| invalid("missing accepted setup statuses"))?;
                if accepted.is_empty() || accepted.len() > 256 {
                    return Err(invalid("invalid accepted setup statuses"));
                }
                let mut seen = std::collections::HashSet::new();
                for status in accepted {
                    if !seen.insert(integer(status, 0, 255)?) {
                        return Err(invalid("duplicate setup status"));
                    }
                }
                if let Some(trailing) = response.get("trailing") {
                    exact(trailing, &["offset", "accepted"])?;
                    integer(&trailing["offset"], min, min)?;
                    integer(&response["maxLength"], min + 1, min + 1)?;
                    let accepted = trailing["accepted"]
                        .as_array()
                        .ok_or_else(|| invalid("missing trailing accepted bytes"))?;
                    if accepted.is_empty() || accepted.len() > 256 {
                        return Err(invalid("invalid trailing accepted bytes"));
                    }
                    let mut seen = std::collections::HashSet::new();
                    for byte in accepted {
                        if !seen.insert(integer(byte, 0, 255)?) {
                            return Err(invalid("duplicate trailing byte"));
                        }
                    }
                }
            }
            let mut step = step.clone();
            step["selector"] = selector;
            normalized.push(step);
        }
        Ok(normalized)
    }

    /// Refuse replacement while any old generation still owns records/resources.
    pub async fn validate_replacement(&self, peer: &str, declaration: &str) -> Result<Value> {
        let peer = self.host.canonical_peer(peer);
        let selectors = self.parse(&peer, declaration)?;
        let identity = self.declaration_identity(declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        if state.session.is_some()
            && (state.peer.as_deref() != Some(peer.as_str())
                || state.selectors != selectors
                || state.execution_declaration.as_ref() != Some(&identity))
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        Ok(json!({"state":"accepted"}))
    }

    pub async fn execute(&self, peer: &str, declaration: &str) -> Result<Value> {
        let result = self.execute_once(peer, declaration).await;
        if result
            .as_ref()
            .is_err_and(|error| error["retryability"] == "caller-decides")
        {
            self.request_recovery(&tokio::runtime::Handle::current());
        }
        result
    }

    async fn execute_once(&self, peer: &str, declaration: &str) -> Result<Value> {
        let peer = self.host.canonical_peer(peer);
        let selectors = self.parse(&peer, declaration)?;
        let identity = self.declaration_identity(declaration)?;
        let mut state = self.state.try_lock().map_err(|_| busy())?;
        self.execute_locked(&peer, selectors, identity, &mut state)
            .await
    }

    /// The supervisor already owns admission when it reads its standing
    /// order. Keep that same guard through execution: a diagnostic reader
    /// must not slip between a recovery snapshot and its second admission.
    async fn execute_locked(
        &self,
        peer: &str,
        selectors: Vec<Value>,
        identity: Value,
        state: &mut State,
    ) -> Result<Value> {
        self.check_recording_admission(state).await?;
        {
            let declarations = self
                .declarations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if declarations.reservation.is_some() {
                return Err(busy());
            }
            if declarations
                .committed
                .as_ref()
                .is_some_and(|current| current != &identity)
            {
                return Err(failure(
                    "lifecycle.invalid-state",
                    "wake captured a replaced continuation declaration",
                ));
            }
        }
        if (state.sealed
            || state
                .session
                .as_ref()
                .is_some_and(|session| session.collection_sealed()))
            && state.session.is_some()
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation handoff is sealed",
            ));
        }
        if state.session.is_some()
            && (state.peer.as_deref() != Some(peer)
                || state.selectors != selectors
                || state.execution_declaration.as_ref() != Some(&identity))
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        let mut current_database = false;
        let setup = identity["setup"].as_array().cloned().unwrap_or_default();
        if state.session.is_none() {
            state.session = Some(self.host.open_session()?);
            self.session_owned.store(true, Ordering::SeqCst);
            state.prepared = None;
            state.sealed = false;
            self.recovery_stopped.store(false, Ordering::SeqCst);
            state.peer = Some(peer.to_owned());
            state.selectors = selectors.clone();
            state.execution_declaration = Some(identity.clone());
            state.setup_generation = None;
            state.setup_complete = false;
            state.link_generation = None;
            state.link_outcome = None;
            state.link_failure = None;
            state.recording = identity["recording"]["id"].as_str().map(str::to_owned);
            state.recording_attached = false;
        } else {
            let snapshot = invoke(state, "session.reconcile", json!({})).await?;
            let connected = snapshot["links"].as_array().is_some_and(|links| {
                links.iter().any(|link| {
                    link["peerId"] == peer
                        && link["state"] == "connected"
                        && link["databaseState"] == "current"
                })
            });
            current_database = connected;
            let subscriptions = snapshot["subscriptions"].as_array().ok_or_else(|| {
                failure(
                    "platform.failure",
                    "missing authoritative subscription snapshot",
                )
            })?;
            state.consumers.retain(|active| {
                connected
                    && subscriptions
                        .iter()
                        .any(|item| item["consumer"] == active.consumer && item["state"] == "live")
            });
            if connected
                && state.consumers.len() == selectors.len()
                && setup.is_empty()
                && identity.get("link").is_none()
                && (state.recording.is_none() || state.recording_attached)
                && !state
                    .session
                    .as_ref()
                    .is_some_and(|session| session.collection_sealed())
            {
                return Ok(completed(peer, selectors.len(), state));
            }
        }
        if let Some(id) = state.recording.clone()
            && !state.recording_attached
        {
            let executor = self.clone();
            let identity = identity.clone();
            let peer = peer.to_owned();
            let session = state.session.as_ref().expect("owned session").clone();
            crate::continuation_journal::run_blocking_result(move || {
                let options = &identity["recording"];
                let journal = executor
                    .recordings
                    .open(
                        &id,
                        &identity,
                        JournalQuota {
                            max_bytes: options["maxBytes"]
                                .as_u64()
                                .expect("validated recording quota"),
                            max_records: options["maxRecords"]
                                .as_u64()
                                .expect("validated recording quota"),
                        },
                    )
                    .map_err(recording_failure)?;
                let mut sessions = executor
                    .recording_sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if journal.status().map_err(recording_failure)?["accepting"] != true {
                    return Err(failure(
                        "lifecycle.invalid-state",
                        "recording is not accepting native collection",
                    ));
                }
                session.attach_journal(journal, json!({"peerId":peer}))?;
                sessions.retain(|_, session| session.strong_count() > 0);
                sessions.insert(id, std::sync::Arc::downgrade(&session));
                Ok(())
            })
            .await?;
            state.recording_attached = true;
        }
        if state.history.len() + selectors.len() - state.consumers.len() > 4096 {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation selector history is full; claim the retained backlog",
            ));
        }
        if !current_database {
            admitted(state, "connection.connect", json!({"peerId":peer,"lease":"continuation-lease",
            "operationId":"continuation-connect","intent":"direct","transport":"auto","preferredPhy":[],"budgetMs":15000})).await?;
            admitted(
                state,
                "gatt.discover",
                json!({"peerId":peer,"lease":"continuation-lease",
            "operationId":"continuation-discover","budgetMs":20000}),
            )
            .await?;
        }
        if let Some(link) = identity.get("link") {
            let snapshot = invoke(state, "session.reconcile", json!({})).await?;
            let generation = snapshot["links"]
                .as_array()
                .and_then(|links| {
                    links
                        .iter()
                        .find(|link| link["peerId"] == peer && link["state"] == "connected")
                })
                .and_then(|link| link["connectionGeneration"].as_str())
                .ok_or_else(|| {
                    failure(
                        "platform.failure",
                        "MTU prerequisite requires authoritative connection generation",
                    )
                })?
                .to_owned();
            if state.link_generation.as_ref() != Some(&generation) {
                state.link_generation = Some(generation);
                state.link_outcome = None;
                state.link_failure = None;
                let mtu = &link["mtu"];
                let request = admitted(
                    state,
                    "connection.request-mtu",
                    json!({"peerId":peer,"lease":"continuation-lease","mtu":mtu["requested"],"budgetMs":mtu["timeoutMs"],"operationId":"continuation-request-mtu"}),
                );
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(
                        mtu["timeoutMs"].as_u64().expect("validated MTU timeout"),
                    ),
                    request,
                )
                .await
                .unwrap_or_else(|_| {
                    Err(failure(
                        "operation.timed-out",
                        "MTU prerequisite deadline elapsed",
                    ))
                });
                let outcome=match result {
                    Ok(value)=>{
                        value["mtu"].as_u64().filter(|mtu|(23..=517).contains(mtu))
                            .map(|actual|json!({"mtu":{"requested":mtu["requested"],"outcome":"negotiated","mtu":actual}}))
                            .ok_or_else(||failure("platform.failure","MTU prerequisite returned no valid negotiated MTU"))
                    },
                    Err(mut error) if error["code"]=="capability.unsupported" && mtu["onUnsupported"]=="continue" => {
                        take_retryability(&mut error);
                        Ok(json!({"mtu":{"requested":mtu["requested"],"outcome":"unsupported","error":error}}))
                    },
                    Err(error)=>Err(error),
                };
                match outcome {
                    Ok(outcome) => state.link_outcome = Some(outcome),
                    Err(mut error) => {
                        error["retryability"] = json!("never");
                        state.link_failure = Some(error);
                    }
                }
            }
            if let Some(error) = &state.link_failure {
                return Err(error.clone());
            }
        }
        for (selector_index, selector) in selectors.iter().enumerate() {
            if state
                .consumers
                .iter()
                .any(|active| active.selector_index == selector_index)
            {
                continue;
            }
            let consumer = format!("ubm-continuation-{}", state.history.len());
            let mut wire_selector = selector.clone();
            for field in ["serviceOccurrence", "characteristicOccurrence"] {
                wire_selector[field] =
                    json!(selector[field].as_u64().expect("validated occurrence") - 1);
            }
            let operation = format!("continuation-subscribe-{}", state.history.len());
            if state.recording_attached {
                let snapshot = invoke(state, "session.reconcile", json!({})).await?;
                let link = snapshot["links"]
                    .as_array()
                    .and_then(|links| {
                        links.iter().find(|link| {
                            link["peerId"] == peer
                                && link["state"] == "connected"
                                && link["databaseState"] == "current"
                        })
                    })
                    .ok_or_else(|| {
                        failure(
                            "lifecycle.invalid-state",
                            "durable subscription requires a current database",
                        )
                    })?;
                if !link["connectionGeneration"].is_string()
                    || !link["databaseGeneration"].is_string()
                {
                    return Err(failure(
                        "platform.failure",
                        "durable metadata requires authoritative generations",
                    ));
                }
                let metadata = json!({"consumer":consumer,"peerId":peer,"connectionGeneration":link["connectionGeneration"],"databaseGeneration":link["databaseGeneration"],"selector":selector});
                let session = state.session.as_ref().expect("owned session").clone();
                let consumer = consumer.clone();
                crate::continuation_journal::run_blocking_result(move || {
                    session.register_journal_consumer(&consumer, metadata)
                })
                .await?;
            }
            // Reserve immutable identity before dispatch: native enable may
            // emit a value before returning a refused completion.
            state.history.push(selector.clone());
            admitted(
                state,
                "gatt.subscribe",
                json!({"peerId":peer,"selector":wire_selector,
                "consumer":consumer,"operationId":operation,"budgetMs":10000}),
            )
            .await?;
            state.consumers.push(ActiveConsumer {
                selector_index,
                consumer,
            });
        }
        if !setup.is_empty() {
            let snapshot = invoke(state, "session.reconcile", json!({})).await?;
            let link = snapshot["links"]
                .as_array()
                .and_then(|links| {
                    links.iter().find(|link| {
                        link["peerId"] == peer
                            && link["state"] == "connected"
                            && link["databaseState"] == "current"
                    })
                })
                .ok_or_else(|| {
                    failure(
                        "lifecycle.invalid-state",
                        "setup requires a current database",
                    )
                })?;
            if !link["connectionGeneration"].is_string() || !link["databaseGeneration"].is_string()
            {
                return Err(failure(
                    "platform.failure",
                    "setup requires authoritative generations",
                ));
            }
            let generation = json!([link["connectionGeneration"], link["databaseGeneration"]]);
            if state.setup_generation.as_ref() != Some(&generation) {
                state.setup_generation = Some(generation.clone());
                state.setup_complete = false;
            }
            state.setup_failures.check(peer, &generation)?;
            if !state.setup_complete {
                if let Err(mut error) = run_setup(self, state, peer, &setup).await {
                    // An old reply cannot be correlated safely with a repeated
                    // command. Only authoritative generation change clears this.
                    error["retryability"] = json!("never");
                    state.setup_failures.record(peer, generation, error.clone());
                    return Err(error);
                }
                state.setup_complete = true;
            }
        }
        self.check_recording_admission(state).await?;
        if state
            .session
            .as_ref()
            .is_some_and(|session| session.collection_sealed())
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "native collection was explicitly stopped",
            ));
        }
        Ok(completed(peer, selectors.len(), state))
    }

    async fn observe_state(&self) -> Result<tokio::sync::MutexGuard<'_, State>> {
        match self.state.try_lock() {
            Ok(state) => Ok(state),
            // Autonomous retry must not make foreground observation depend on
            // catching the backoff gap. Join the same FIFO admission queue,
            // allowing the current bounded operation to settle first.
            // Explicit caller executions retain their existing busy contract.
            Err(_) if self.recovering.load(Ordering::SeqCst) => Ok(self.state.lock().await),
            Err(_) => Err(busy()),
        }
    }

    pub async fn prepare_claim(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        if max_items == 0 || max_bytes == 0 {
            return Err(invalid("claim bounds must be positive"));
        }
        let mut state = self.observe_state().await?;
        if let Some(prepared) = &state.prepared {
            let mut claim = prepared.claim.clone();
            if prepared.acknowledged {
                claim["batches"] = json!([]);
            }
            drop(state);
            self.await_stopped_recovery().await;
            return Ok(claim);
        }
        if state.session.is_none() {
            return Ok(
                json!({"consumerCount":0,"batches":[],"disposed":false,"disposeFailure":null,
                "afterCutoffLoss":{"items":0,"bytes":0},"selectors":[]}),
            );
        }
        let cutoff = invoke(&state, "session.quiesce", json!({})).await?;
        if cutoff["state"] != "sealed" {
            return Err(failure("platform.failure", "quiesce did not seal"));
        }
        state.sealed = true;
        self.recovery_stopped.store(true, Ordering::SeqCst);
        self.recovery_wake.notify_one();
        let loss = cutoff_loss(&cutoff)?;
        let mut batches = Vec::new();
        let mut complete = false;
        let mut drain_failure = None;
        for _ in 0..32 {
            let session = state.session.as_ref().expect("session retained").clone();
            let batch = if state.recording.is_some() {
                let runtime = tokio::runtime::Handle::current();
                crate::continuation_journal::run_blocking_result(move || {
                    Ok(runtime.block_on(session.drain(max_items, max_bytes)))
                })
                .await?
            } else {
                session.drain(max_items, max_bytes).await
            };
            let record: Value = match serde_json::from_str(&batch) {
                Ok(record) => record,
                Err(error) => {
                    drain_failure = Some(format!("invalid continuation drain: {error}"));
                    break;
                }
            };
            let valid = record.as_object().is_some_and(|object| {
                object
                    .keys()
                    .all(|key| matches!(key.as_str(), "more" | "records" | "controlLost"))
            }) && record["records"].is_array()
                && record["controlLost"].as_u64().is_some();
            let Some(more) = record["more"].as_bool().filter(|_| valid) else {
                drain_failure =
                    Some("invalid continuation drain shape; session retained".to_owned());
                break;
            };
            complete = !more;
            batches.push(batch);
            if complete {
                break;
            }
        }
        state.token += 1;
        let mut claim = json!({"consumerCount":state.history.len(),"batches":batches,"disposed":false,
            "disposeFailure":if complete {None} else {Some(drain_failure.unwrap_or_else(|| "continuation claim has a retained unread tail".to_owned()))},
            "afterCutoffLoss":loss,"selectors":state.history,"claimToken":format!("continuation-{}",state.token)});
        if let Some(id) = &state.recording {
            claim["recording"] = json!({"id":id});
        }
        state.prepared = Some(Prepared {
            claim: claim.clone(),
            complete,
            acknowledged: false,
            receipt: None,
        });
        drop(state);
        self.await_stopped_recovery().await;
        Ok(claim)
    }

    pub async fn acknowledge_claim(&self, token: &str) -> Result<Value> {
        let mut state = self.state.try_lock().map_err(|_| busy())?;
        let prepared = state
            .prepared
            .as_mut()
            .ok_or_else(|| invalid("no prepared claim"))?;
        if prepared.claim["claimToken"] != token {
            return Err(invalid("claim token mismatch"));
        }
        if let Some(receipt) = &prepared.receipt {
            return Ok(receipt.clone());
        }
        prepared.acknowledged = true;
        if !prepared.complete {
            let result = json!({"disposed":false,"afterCutoffLoss":prepared.claim["afterCutoffLoss"],"disposeFailure":prepared.claim["disposeFailure"]});
            state.prepared = None;
            return Ok(result);
        }
        let disposal = invoke(&state, "session.continuation-dispose", json!({})).await?;
        let released = disposal["state"] == "released";
        let receipt = json!({"disposed":released,"afterCutoffLoss":cutoff_loss(&disposal)?,
            "disposeFailure":if released {None} else {Some(format!("continuation disposal remains owned: {disposal}"))}});
        if released {
            state.session = None;
            self.session_owned.store(false, Ordering::SeqCst);
            // The registry retains the durable journal independently. Only
            // this released radio generation's recording attachment is retired.
            state.recording = None;
            state.recording_attached = false;
            state.peer = None;
            state.selectors.clear();
            state.history.clear();
            state.consumers.clear();
            state
                .prepared
                .as_mut()
                .expect("prepared handoff retained")
                .receipt = Some(receipt.clone());
        }
        Ok(receipt)
    }
}

impl NativeContinuation {
    // A published handoff must not race the stopped supervisor for admission.
    // Quiescence already acquired the state lock, so this waits only for the
    // worker to observe its stop signal, never for an in-flight radio operation.
    async fn await_stopped_recovery(&self) {
        loop {
            let notified = self.recovery_idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.recovering.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn declaration_identity(&self, text: &str) -> Result<Value> {
        if text.len() > 1 << 20 {
            return Err(invalid("oversized declaration"));
        }
        let root: Value =
            serde_json::from_str(text).map_err(|_| invalid("malformed declaration"))?;
        let object = root
            .as_object()
            .ok_or_else(|| invalid("declaration must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "onAppearance"
                    | "peerId"
                    | "resubscribe"
                    | "headlessTaskName"
                    | "foregroundService"
                    | "setup"
                    | "link"
                    | "recording"
            )
        }) {
            return Err(invalid("unknown declaration field"));
        }
        if root["onAppearance"] == "native" {
            let peer = root
                .get("peerId")
                .map_or(Ok("continuation-any-peer"), |value| {
                    value
                        .as_str()
                        .ok_or_else(|| invalid("peerId must be a string"))
                })?;
            let selectors = self.parse(peer, text)?;
            let setup = self.setup(&root, selectors.len())?;
            let mut identity = json!({"onAppearance":"native","resubscribe":selectors});
            if let Some(recording) = root.get("recording") {
                exact(recording, &["id", "maxBytes", "maxRecords"])?;
                let id = recording["id"]
                    .as_str()
                    .ok_or_else(|| invalid("recording id required"))?;
                if id.is_empty()
                    || id.len() > 64
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                {
                    return Err(invalid("invalid recording id"));
                }
                integer(&recording["maxBytes"], 1 << 20, 1 << 30)?;
                integer(&recording["maxRecords"], 1, 1_000_000)?;
                identity["recording"] = recording.clone();
            }
            if let Some(link) = root.get("link") {
                exact(link, &["mtu"])?;
                let mtu = &link["mtu"];
                exact(mtu, &["requested", "timeoutMs", "onUnsupported"])?;
                integer(&mtu["requested"], 23, 517)?;
                integer(&mtu["timeoutMs"], 1, 20000)?;
                if !matches!(mtu["onUnsupported"].as_str(), Some("continue" | "fail")) {
                    return Err(invalid("invalid unsupported MTU policy"));
                }
                identity["link"] = link.clone();
            }
            if !setup.is_empty() {
                identity["setup"] = json!(setup);
            }
            if object.contains_key("peerId") {
                identity["peerId"] = json!(self.host.canonical_peer(peer));
            }
            Ok(identity)
        } else if matches!(
            root["onAppearance"].as_str(),
            Some("record-only" | "headless-task" | "foreground-service")
        ) || !object.contains_key("onAppearance")
        {
            if object.contains_key("setup")
                || object.contains_key("link")
                || object.contains_key("recording")
            {
                return Err(invalid("setup and link require native continuation"));
            }
            Ok(root)
        } else {
            Err(invalid("unknown declaration strategy"))
        }
    }

    /// Reserve authority before the platform writes persistent configuration.
    /// While reserved, no old or new captured wake may create native work.
    pub fn reserve_declaration(&self, declaration: &str) -> Result<Value> {
        let identity = self.declaration_identity(declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if declarations.reservation.is_some() {
            return Err(busy());
        }
        if state.session.is_some() && state.execution_declaration.as_ref() != Some(&identity) {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        declarations.sequence = declarations.sequence.checked_add(1).ok_or_else(|| {
            failure(
                "lifecycle.invalid-state",
                "declaration generation exhausted",
            )
        })?;
        let token = format!("continuation-declaration-{}", declarations.sequence);
        declarations.reservation = Some(DeclarationReservation {
            token: token.clone(),
            declaration: identity,
        });
        Ok(json!({"reservationToken":token}))
    }

    pub fn commit_declaration(&self, token: &str) -> Result<Value> {
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !declarations
            .reservation
            .as_ref()
            .is_some_and(|reservation| reservation.token == token)
        {
            return Err(invalid("declaration reservation token mismatch"));
        }
        declarations.committed = declarations
            .reservation
            .take()
            .map(|reservation| reservation.declaration);
        drop(declarations);
        if let Some(runtime) = self.recovery_runtime.get() {
            self.request_recovery(runtime);
        }
        Ok(json!({"state":"committed"}))
    }

    pub fn cancel_declaration(&self, token: &str) -> Result<Value> {
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !declarations
            .reservation
            .as_ref()
            .is_some_and(|reservation| reservation.token == token)
        {
            return Err(invalid("declaration reservation token mismatch"));
        }
        declarations.reservation = None;
        drop(declarations);
        if let Some(runtime) = self.recovery_runtime.get() {
            self.request_recovery(runtime);
        }
        Ok(json!({"state":"cancelled"}))
    }

    /// Cold-start seeding cannot overwrite a declaration already committed by
    /// another native caller, even when no session has been opened yet.
    pub fn seed_declaration(&self, declaration: &str) -> Result<Value> {
        let identity = self.declaration_identity(declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        let mut declarations = self
            .declarations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if declarations.reservation.is_some() {
            return Err(busy());
        }
        if declarations
            .committed
            .as_ref()
            .is_some_and(|current| current != &identity)
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "persisted declaration is older than native authority",
            ));
        }
        if state.session.is_some() && state.execution_declaration.as_ref() != Some(&identity) {
            return Err(failure(
                "lifecycle.invalid-state",
                "native session owns a different declaration",
            ));
        }
        declarations.committed = Some(identity);
        Ok(json!({"state":"seeded"}))
    }
    /// Stops automatic retry at authoritative owner shutdown, without claiming
    /// successful resource cleanup or deleting a retained handoff.
    pub fn stop_recovery(&self) {
        self.recovery_stopped.store(true, Ordering::SeqCst);
        self.recovery_wake.notify_one();
    }
    /// Called after the host has reconciled a lifecycle/adapter event. One
    /// worker coalesces events; radio refusal receives bounded backoff and is
    /// retained in diagnostics, never represented as successful recovery.
    pub fn request_recovery(&self, runtime: &tokio::runtime::Handle) {
        self.recovery_runtime.get_or_init(|| runtime.clone());
        if self.recovery_stopped.load(Ordering::SeqCst)
            || !self.session_owned.load(Ordering::SeqCst)
        {
            return;
        }
        self.recovery_requested.store(true, Ordering::SeqCst);
        if self.recovering.swap(true, Ordering::SeqCst) {
            self.recovery_wake.notify_one();
            return;
        }
        let executor = self.clone();
        runtime.spawn(async move {
            loop {
                executor.recovery_requested.store(false, Ordering::SeqCst);
                let mut attempt = 0_u32;
                loop {
                    if executor.recovery_stopped.load(Ordering::SeqCst) || !executor.session_owned.load(Ordering::SeqCst) { break; }
                    let result = {
                        // The internal worker waits fairly for admission. Public
                        // callers still fail promptly, but repeated status reads
                        // cannot starve an already-requested native recovery.
                        let mut state = tokio::select! {
                            biased;
                            _ = executor.recovery_wake.notified() => { continue; },
                            state = executor.state.lock() => state,
                        };
                        if executor.recovery_stopped.load(Ordering::SeqCst) || state.session.is_none() || state.sealed { None }
                        else if let Some((peer, identity)) = state.peer.clone().zip(state.execution_declaration.clone()) {
                            let selectors = state.selectors.clone();
                            Some(executor.execute_locked(&peer, selectors, identity, &mut state).await)
                        } else {
                            Some(Err(failure("lifecycle.invalid-state", "owned continuation has no declaration identity")))
                        }
                    };
                    let Some(result) = result else { break; };
                    let retry = match &result {
                        Ok(_) => false,
                        Err(error) => error["retryability"] == "caller-decides",
                    };
                    *executor.last_recovery.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(match result {
                        Ok(mut outcome) => { outcome["attempt"] = json!(attempt.saturating_add(1)); outcome },
                        Err(mut error) => {
                            let retryability = take_retryability(&mut error);
                            json!({"event":"continuation.failed","strategy":"native","error":error,"retryability":retryability,"attempt":attempt.saturating_add(1)})
                        },
                    });
                    if !retry { break; }
                    tokio::select! {
                        _ = executor.recovery_wake.notified() => {},
                        _ = tokio::time::sleep(std::time::Duration::from_millis((100_u64 << attempt.min(9)).min(30_000))) => {},
                    }
                    attempt = attempt.saturating_add(1);
                }
                executor.recovering.store(false, Ordering::SeqCst);
                if !executor.recovery_requested.swap(false, Ordering::SeqCst) || executor.recovering.swap(true, Ordering::SeqCst) { break; }
            }
            executor.recovery_idle.notify_waiters();
        });
    }

    /// A synchronous, nonblocking declaration admission check for persistence.
    /// No live state means the platform validator remains authoritative.
    pub fn declaration_replacement_failure(&self, declaration: &str) -> Option<String> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(_) => return Some("continuation execution or handoff is in progress".to_owned()),
        };
        state.session.as_ref()?;
        match self.declaration_identity(declaration) {
            Ok(identity) if state.execution_declaration.as_ref() == Some(&identity) => None,
            _ => Some("claim the pinned continuation before replacing its declaration".to_owned()),
        }
    }

    pub async fn describe_backlog(&self) -> Result<Value> {
        let state = self.observe_state().await?;
        if state.session.is_none() {
            Ok(Value::Null)
        } else {
            let mut counters = invoke(&state, "counters.describe", json!({})).await?;
            counters["continuationOutcome"] = self
                .last_recovery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or(Value::Null);
            Ok(counters)
        }
    }
}

pub fn envelope(result: Result<Value>) -> String {
    match result {
        Ok(value) => json!({"ok":true,"value":value}).to_string(),
        Err(mut error) => {
            let retryability = take_retryability(&mut error);
            json!({"ok":false,"error":error,"commit":null,"retryability":retryability}).to_string()
        }
    }
}

fn take_retryability(error: &mut Value) -> Value {
    error
        .as_object_mut()
        .and_then(|error| error.remove("retryability"))
        .unwrap_or(json!("never"))
}

fn completed(peer: &str, count: usize, state: &State) -> Value {
    let mut result = json!({"event":"continuation.completed","strategy":"native","peerAddress":peer,"resubscribed":count});
    if let Some(link) = &state.link_outcome {
        result["link"] = link.clone();
    }
    result
}

fn exact(value: &Value, keys: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("setup object required"))?;
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err(invalid("unknown setup field"));
    }
    Ok(())
}
fn integer(value: &Value, min: u64, max: u64) -> Result<u64> {
    value
        .as_u64()
        .filter(|value| *value >= min && *value <= max)
        .ok_or_else(|| invalid("invalid setup integer"))
}
fn bytes(value: &Value) -> Result<Vec<u8>> {
    let bytes = value
        .as_array()
        .ok_or_else(|| invalid("setup bytes must be an array"))?;
    if bytes.is_empty() || bytes.len() > 512 {
        return Err(invalid("invalid setup byte length"));
    }
    bytes
        .iter()
        .map(|value| integer(value, 0, 255).map(|byte| byte as u8))
        .collect()
}

async fn run_setup(
    executor: &NativeContinuation,
    state: &mut State,
    peer: &str,
    steps: &[Value],
) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        let timeout = std::time::Duration::from_millis(
            step["timeoutMs"].as_u64().expect("validated setup timeout"),
        );
        let deadline = tokio::time::Instant::now() + timeout;
        // The step budget starts before observer admission, which can wait
        // behind durable journal I/O. A late observer must never dispatch a
        // setup write after the caller's deadline has already elapsed.
        let result = tokio::time::timeout_at(deadline, async {
        executor.check_recording_admission(state).await?;
        let mut observation = if let Some(response) = step.get("response") {
            let subscription = response["subscriptionIndex"]
                .as_u64()
                .expect("validated subscription index") as usize;
            let consumer = &state
                .consumers
                .iter()
                .find(|consumer| consumer.selector_index == subscription)
                .ok_or_else(|| {
                    failure(
                        "lifecycle.invalid-state",
                        "setup response subscription is not live",
                    )
                })?
                .consumer;
            let prefix = bytes(&response["prefix"])?;
            let matcher: RecordMatcher = Arc::new(move |record| {
                // Malformed internal bytes are a correlated boundary failure,
                // never silently ignored until timeout.
                record["valueB64"]
                    .as_str()
                    .and_then(|value| decode_base64(value, 512).ok())
                    .is_none_or(|value| value.starts_with(&prefix))
            });
            let session = state.session.as_ref().expect("owned session").clone();
            let consumer = consumer.clone();
            Some(if state.recording.is_some() {
                crate::continuation_journal::run_blocking_result(move || {
                    session.observe(&consumer, matcher)
                })
                .await?
            } else {
                session.observe(&consumer, matcher)?
            })
        } else {
            None
        };
        let mut selector = step["selector"].clone();
        for field in ["serviceOccurrence", "characteristicOccurrence"] {
            selector[field] = json!(selector[field].as_u64().expect("validated occurrence") - 1);
        }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(failure("operation.timed-out", &format!("setup step {index} expired before its write")));
            }
            let mut args = json!({"peerId":peer,"selector":selector,"valueB64":encode_base64(&bytes(&step["value"])?),"mode":"with-response","operationId":format!("continuation-setup-{index}"),"budgetMs":remaining.as_millis().max(1)});
            state.admission += 1;
            args["admission"] = json!(state.admission);
            invoke_with_deadline(state, "gatt.write", args, Some(deadline.into_std())).await?;
            if let Some(observation) = &mut observation {
                let record = (&mut observation.receiver).await.map_err(|_|failure("stream.closed", "setup acknowledgement observation closed"))?;
                if record["t"] == "stream-end" {
                    if let Some(error)=record.get("error") {return Err(error.clone());}
                    return Err(failure("stream.closed", &format!("setup acknowledgement stream ended: {}",record["reason"])));
                }
                let value = record["valueB64"].as_str().and_then(|value|decode_base64(value,512).ok()).ok_or_else(||failure("platform.failure", "malformed setup acknowledgement bytes"))?;
                let response = &step["response"];
                let min = response["minLength"].as_u64().expect("validated minimum") as usize;
                let max = response["maxLength"].as_u64().expect("validated maximum") as usize;
                if value.len()<min || value.len()>max {return Err(failure("platform.failure", "correlated setup acknowledgement has invalid length"));}
                let status = value[response["status"]["offset"].as_u64().expect("validated offset") as usize];
                if !response["status"]["accepted"].as_array().expect("validated statuses").contains(&json!(status)) {
                    return Err(failure("platform.failure", &format!("setup step {index} rejected with application status {status}")));
                }
                if let Some(trailing) = response.get("trailing") {
                    let offset = trailing["offset"].as_u64().expect("validated trailing offset") as usize;
                    if let Some(byte) = value.get(offset)
                        && !trailing["accepted"].as_array().expect("validated trailing values").contains(&json!(byte)) {
                            return Err(failure("platform.failure", &format!("setup step {index} rejected trailing acknowledgement byte {byte}")));
                    }
                }
            }
            Ok(())
        }).await.map_err(|_|failure("operation.timed-out", &format!("setup step {index} exceeded its command deadline")))?;
        result?;
    }
    Ok(())
}

fn cutoff_loss(value: &Value) -> Result<Value> {
    Ok(
        json!({"items":value["afterCutoffItems"].as_u64().ok_or_else(|| failure("platform.failure", "missing cutoff items"))?,
        "bytes":value["afterCutoffBytes"].as_u64().ok_or_else(|| failure("platform.failure", "missing cutoff bytes"))?}),
    )
}

async fn admitted(state: &mut State, op: &str, mut args: Value) -> Result<Value> {
    state.admission += 1;
    args["admission"] = json!(state.admission);
    invoke(state, op, args).await
}

async fn invoke(state: &State, op: &str, args: Value) -> Result<Value> {
    invoke_with_deadline(state, op, args, None).await
}

fn setup_write_budget(args: &mut Value, deadline: std::time::Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(failure(
            "operation.timed-out",
            "setup write deadline elapsed before native dispatch",
        ));
    }
    // Round a sub-millisecond remainder up so the native call has a bounded,
    // nonzero budget; never reuse the original full step budget after admission.
    args["budgetMs"] = json!(remaining.as_millis().max(1));
    Ok(())
}

async fn invoke_with_deadline(
    state: &State,
    op: &str,
    mut args: Value,
    deadline: Option<std::time::Instant>,
) -> Result<Value> {
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| failure("lifecycle.invalid-state", "no continuation session"))?;
    if session.collection_sealed()
        && matches!(
            op,
            "connection.connect"
                | "connection.request-mtu"
                | "gatt.discover"
                | "gatt.subscribe"
                | "gatt.write"
        )
    {
        return Err(failure(
            "lifecycle.invalid-state",
            "native collection was explicitly stopped",
        ));
    }
    let reply = if state.recording.is_some() {
        let session = session.clone();
        let op = op.to_owned();
        let runtime = tokio::runtime::Handle::current();
        crate::continuation_journal::run_blocking_result(move || {
            if let Some(deadline) = deadline {
                setup_write_budget(&mut args, deadline)?;
            }
            Ok(runtime.block_on(session.call(&op, &args.to_string())))
        })
        .await?
    } else {
        if let Some(deadline) = deadline {
            setup_write_budget(&mut args, deadline)?;
        }
        session.call(op, &args.to_string()).await
    };
    let envelope: Value = serde_json::from_str(&reply)
        .map_err(|_| failure("platform.failure", "invalid core reply"))?;
    if envelope["ok"] == true && envelope.get("value").is_some() {
        Ok(envelope["value"].clone())
    } else if envelope["ok"] == false && envelope["error"]["code"].is_string() {
        let mut error = envelope["error"].clone();
        // Preserve the operation's decision. A code alone cannot authorize a
        // reconnect; adapters that omit retryability are conservatively never.
        error["retryability"] = match envelope["retryability"].as_str() {
            Some("caller-decides") => json!("caller-decides"),
            _ => json!("never"),
        };
        Err(error)
    } else {
        Err(failure(
            "platform.failure",
            "malformed continuation operation envelope",
        ))
    }
}

#[cfg(test)]
mod idle_recovery_tests {
    use super::*;

    #[test]
    fn setup_uncertainty_survives_other_peers_and_only_its_generation_change_clears_it() {
        let mut failures = SetupFailures::default();
        let generation = json!(["connection-1", "database-1"]);
        let original = failure("operation.timed-out", "original STOP deadline");
        failures.check("peer-a", &generation).unwrap();
        failures.record("peer-a", generation.clone(), original.clone());
        failures.check("peer-b", &generation).unwrap();
        failures.record(
            "peer-b",
            generation.clone(),
            failure("platform.failure", "B"),
        );
        assert_eq!(failures.check("peer-a", &generation), Err(original.clone()));
        failures
            .check("peer-b", &json!(["connection-2", "database-2"]))
            .unwrap();
        assert_eq!(failures.check("peer-a", &generation), Err(original));
        failures
            .check("peer-a", &json!(["connection-2", "database-2"]))
            .unwrap();
        assert!(failures.peers.is_empty());
    }

    #[test]
    fn setup_uncertainty_capacity_never_evicts_a_live_generation() {
        let mut failures = SetupFailures::default();
        let generation = json!(["connection-1", "database-1"]);
        let original = failure("operation.timed-out", "original STOP deadline");
        for index in 0..MAX_SETUP_FAILURE_PEERS {
            let peer = format!("peer-{index}");
            failures.check(&peer, &generation).unwrap();
            failures.record(&peer, generation.clone(), original.clone());
        }
        assert_eq!(
            failures.check("new-peer", &generation).unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        assert_eq!(failures.check("peer-0", &generation), Err(original));
        failures
            .check("peer-0", &json!(["connection-2", "database-2"]))
            .unwrap();
        failures.check("new-peer", &generation).unwrap();
        assert_eq!(failures.peers.len(), MAX_SETUP_FAILURE_PEERS - 1);
    }

    #[derive(Default)]
    struct Host(Arc<Session>);
    #[derive(Default)]
    struct Session {
        release: AtomicBool,
        hold_connect: AtomicBool,
        connect_entered: tokio::sync::Notify,
        connected: tokio::sync::Notify,
    }

    impl ContinuationHost for Host {
        fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
            Ok(self.0.clone())
        }
    }
    impl ContinuationSession for Session {
        fn call<'a>(&'a self, op: &'a str, _: &'a str) -> ContinuationFuture<'a> {
            Box::pin(async move {
                if op == "connection.connect" && self.hold_connect.load(Ordering::SeqCst) {
                    self.connect_entered.notify_one();
                    self.connected.notified().await;
                }
                envelope(Ok(match op {
                    "session.quiesce" => {
                        json!({"state":"sealed","afterCutoffItems":0,"afterCutoffBytes":0})
                    }
                    "session.continuation-dispose" => {
                        json!({"state":if self.release.load(Ordering::SeqCst) {"released"} else {"release-failed"},"afterCutoffItems":0,"afterCutoffBytes":0})
                    }
                    "counters.describe" => json!({"queuedData":1,"lastError":null}),
                    "session.reconcile" => json!({"links":[],"subscriptions":[]}),
                    _ => json!({}),
                }))
            })
        }
        fn drain(&self, _: u32, _: u32) -> ContinuationFuture<'_> {
            Box::pin(async { json!({"records":[],"more":false,"controlLost":0}).to_string() })
        }
    }

    #[tokio::test]
    async fn backlog_status_joins_autonomous_recovery_but_explicit_execution_remains_busy() {
        use std::{future::Future, task::Poll};
        let host = Arc::new(Host::default());
        host.0.release.store(true, Ordering::SeqCst);
        let engine = NativeContinuation::new(host.clone());
        engine
            .execute("peer", r#"{"onAppearance":"native"}"#)
            .await
            .unwrap();
        host.0.hold_connect.store(true, Ordering::SeqCst);
        engine.request_recovery(&tokio::runtime::Handle::current());
        host.0.connect_entered.notified().await;
        let status = engine.describe_backlog();
        tokio::pin!(status);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(status.as_mut().poll(cx).is_pending())).await,
            "autonomous recovery must not make retained backlog status a timing-dependent busy refusal"
        );
        engine.stop_recovery();
        host.0.connected.notify_one();
        assert_eq!(status.await.unwrap()["queuedData"], 1);
        engine.await_stopped_recovery().await;
        let explicit = engine.state.lock().await;
        assert_eq!(
            engine.describe_backlog().await.unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        drop(explicit);
        let prepared = engine.prepare_claim(8, 1024).await.unwrap();
        assert_eq!(
            engine
                .acknowledge_claim(prepared["claimToken"].as_str().unwrap())
                .await
                .unwrap()["disposed"],
            true
        );
    }

    #[tokio::test]
    async fn idle_adapter_event_cannot_compete_with_first_wake_admission() {
        let engine = NativeContinuation::new(Arc::new(Host::default()));
        // Deterministically hold the idle inspection point. A power-on event
        // must not enqueue a supervisor that will acquire this admission lock
        // ahead of the first genuine restoration wake.
        let idle = engine.state.lock().await;
        engine.request_recovery(&tokio::runtime::Handle::current());
        assert!(
            !engine.recovering.load(Ordering::SeqCst),
            "no owned session means no recovery worker"
        );
        assert!(!engine.recovery_requested.load(Ordering::SeqCst));
        drop(idle);
        let declaration = r#"{"onAppearance":"native"}"#;
        engine.seed_declaration(declaration).unwrap();
        assert_eq!(
            engine.execute("peer", declaration).await.unwrap()["event"],
            "continuation.completed"
        );
        // Actual live admission still fails promptly rather than being hidden
        // by an asynchronous retry in the public/native wake boundary.
        let active = engine.state.lock().await;
        assert_eq!(
            engine.seed_declaration(declaration).unwrap_err()["code"],
            "lifecycle.invalid-state"
        );
        drop(active);
        engine.stop_recovery();
    }

    #[tokio::test]
    async fn recovery_ownership_survives_held_install_and_failed_disposal() {
        use std::{future::Future, task::Poll};
        let host = Arc::new(Host::default());
        host.0.hold_connect.store(true, Ordering::SeqCst);
        let engine = NativeContinuation::new(host.clone());
        let execute = engine.execute("peer", r#"{"onAppearance":"native"}"#);
        tokio::pin!(execute);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(execute.as_mut().poll(cx).is_pending())).await
        );
        assert!(engine.session_owned.load(Ordering::SeqCst));
        engine.request_recovery(&tokio::runtime::Handle::current());
        assert!(
            engine.recovering.load(Ordering::SeqCst),
            "event during accepted execution remains queued"
        );
        // Stop this test's queued worker before draining; the obligation flag
        // must remain true independently of the retry-policy stop flag.
        engine.stop_recovery();
        host.0.connected.notify_one();
        execute.await.unwrap();
        let claim = engine.prepare_claim(8, 1024).await.unwrap();
        let token = claim["claimToken"].as_str().unwrap();
        assert_eq!(
            engine.acknowledge_claim(token).await.unwrap()["disposed"],
            false
        );
        assert!(engine.session_owned.load(Ordering::SeqCst));
        assert!(engine.state.lock().await.session.is_some());
        host.0.release.store(true, Ordering::SeqCst);
        assert_eq!(
            engine.acknowledge_claim(token).await.unwrap()["disposed"],
            true
        );
        assert!(!engine.session_owned.load(Ordering::SeqCst));
        assert!(engine.state.lock().await.session.is_none());
    }
}
