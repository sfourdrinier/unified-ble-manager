//! Process-owned native continuation. Native hosts share this owner; no
//! JavaScript session is necessary to connect, subscribe, or retain values.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use tokio::sync::Mutex;

pub type Result<T> = std::result::Result<T, Value>;
pub type ContinuationFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

pub trait ContinuationSession: Send + Sync {
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a>;
    fn drain(&self, max_items: u32, max_bytes: u32) -> ContinuationFuture<'_>;
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

fn invalid(detail: &str) -> Value {
    failure("argument.invalid", detail)
}
fn busy() -> Value {
    failure(
        "lifecycle.invalid-state",
        "continuation execution or handoff is in progress",
    )
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
    recovery_requested: Arc<AtomicBool>,
    recovering: Arc<AtomicBool>,
    recovery_stopped: Arc<AtomicBool>,
    last_recovery: Arc<std::sync::Mutex<Option<Value>>>,
    recovery_wake: Arc<tokio::sync::Notify>,
    recovery_idle: Arc<tokio::sync::Notify>,
    declarations: Arc<std::sync::Mutex<DeclarationAuthority>>,
    recovery_runtime: Arc<std::sync::OnceLock<tokio::runtime::Handle>>,
}

impl NativeContinuation {
    pub fn new(host: Arc<dyn ContinuationHost>) -> Self {
        Self {
            host,
            state: Arc::default(),
            recovery_requested: Arc::default(),
            recovering: Arc::default(),
            recovery_stopped: Arc::default(),
            last_recovery: Arc::default(),
            recovery_wake: Arc::default(),
            recovery_idle: Arc::default(),
            declarations: Arc::default(),
            recovery_runtime: Arc::default(),
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
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "onAppearance" | "peerId" | "resubscribe"))
            || root["onAppearance"] != "native"
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

    /// Refuse replacement while any old generation still owns records/resources.
    pub async fn validate_replacement(&self, peer: &str, declaration: &str) -> Result<Value> {
        let peer = self.host.canonical_peer(peer);
        let selectors = self.parse(&peer, declaration)?;
        let state = self.state.try_lock().map_err(|_| busy())?;
        if state.session.is_some()
            && (state.peer.as_deref() != Some(peer.as_str()) || state.selectors != selectors)
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
        if state.sealed && state.session.is_some() {
            return Err(failure(
                "lifecycle.invalid-state",
                "continuation handoff is sealed",
            ));
        }
        if state.session.is_some()
            && (state.peer.as_deref() != Some(peer) || state.selectors != selectors)
        {
            return Err(failure(
                "lifecycle.invalid-state",
                "claim the pinned continuation before replacing its declaration",
            ));
        }
        let mut current_database = false;
        if state.session.is_none() {
            state.session = Some(self.host.open_session()?);
            state.prepared = None;
            state.sealed = false;
            self.recovery_stopped.store(false, Ordering::SeqCst);
            state.peer = Some(peer.to_owned());
            state.selectors = selectors.clone();
            state.execution_declaration = Some(identity);
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
            if connected && state.consumers.len() == selectors.len() {
                return Ok(completed(peer, selectors.len()));
            }
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
            state.history.push(selector.clone());
        }
        Ok(completed(peer, selectors.len()))
    }

    pub async fn prepare_claim(&self, max_items: u32, max_bytes: u32) -> Result<Value> {
        if max_items == 0 || max_bytes == 0 {
            return Err(invalid("claim bounds must be positive"));
        }
        let mut state = self.state.try_lock().map_err(|_| busy())?;
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
            let batch = state
                .session
                .as_ref()
                .expect("session retained")
                .drain(max_items, max_bytes)
                .await;
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
        let claim = json!({"consumerCount":state.history.len(),"batches":batches,"disposed":false,
            "disposeFailure":if complete {None} else {Some(drain_failure.unwrap_or_else(|| "continuation claim has a retained unread tail".to_owned()))},
            "afterCutoffLoss":loss,"selectors":state.history,"claimToken":format!("continuation-{}",state.token)});
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
            let mut identity = json!({"onAppearance":"native","resubscribe":selectors});
            if object.contains_key("peerId") {
                identity["peerId"] = json!(self.host.canonical_peer(peer));
            }
            Ok(identity)
        } else if matches!(
            root["onAppearance"].as_str(),
            Some("record-only" | "headless-task" | "foreground-service")
        ) || !object.contains_key("onAppearance")
        {
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
        if self.recovery_stopped.load(Ordering::SeqCst) {
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
                    if executor.recovery_stopped.load(Ordering::SeqCst) { break; }
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
        let peer = state.peer.as_deref().unwrap_or("");
        match self.parse(peer, declaration) {
            Ok(selectors) if state.selectors == selectors => None,
            _ => Some("claim the pinned continuation before replacing its declaration".to_owned()),
        }
    }

    pub async fn describe_backlog(&self) -> Result<Value> {
        match self.state.try_lock() {
            Err(_) => Err(busy()),
            Ok(state) if state.session.is_none() => Ok(Value::Null),
            Ok(state) => {
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

fn completed(peer: &str, count: usize) -> Value {
    json!({"event":"continuation.completed","strategy":"native","peerAddress":peer,"resubscribed":count})
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
    let session = state
        .session
        .as_ref()
        .ok_or_else(|| failure("lifecycle.invalid-state", "no continuation session"))?;
    let envelope: Value = serde_json::from_str(&session.call(op, &args.to_string()).await)
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
