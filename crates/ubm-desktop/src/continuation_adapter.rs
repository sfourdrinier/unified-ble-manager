//! Continuation session adapter over the host's existing central. This owns
//! only a lease, consumer routes and their shared bounded outbox, never a radio.
use crate::continuation::{ContinuationFuture, ContinuationHost, ContinuationSession, Result};
use crate::continuation_outbox::{Outbox, WakeSink, encode_base64};
use crate::{
    ConnectionState, DatabaseState, DesktopCentral, DesktopError, NotificationPoll, OpControl,
    PathSelector, RadioBoundary,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::Mutex;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
mod ingress_tests {
    use super::*;
    struct ThreadWake(std::sync::Mutex<Vec<std::thread::ThreadId>>);
    impl WakeSink for ThreadWake {
        fn wake(&self, _: u64) {
            self.0.lock().unwrap().push(std::thread::current().id());
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn journal_attachment_during_suspended_ingress_stays_off_runtime() {
        let runtime_thread = std::thread::current().id();
        let central = DesktopCentral::open(crate::FakeRadio::new(), "attachment-race")
            .await
            .unwrap();
        let wake = Arc::new(ThreadWake(std::sync::Mutex::new(Vec::new())));
        let session = Arc::new(Session {
            central,
            lease: "test".into(),
            peer: Mutex::new(None),
            routes: Mutex::new(vec![Route {
                peer: "absent".into(),
                consumer: "c".into(),
                live: true,
                delivery: "unknown",
                selector: PathSelector {
                    service_uuid: "180d".into(),
                    service_occurrence: None,
                    characteristic_uuid: Some("2a37".into()),
                    characteristic_occurrence: None,
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                },
            }]),
            outbox: Outbox::new(1, wake.clone()),
            last_error: Mutex::new(None),
        });
        let routes = session.routes.lock().await;
        let pass = session.collect_pass();
        tokio::pin!(pass);
        tokio::select! { biased;
            result = &mut pass => panic!("held routes unexpectedly completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        let directory = std::env::temp_dir().join(format!(
            "ubm-attachment-race-{}-{}",
            std::process::id(),
            NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let journal = Arc::new(
            crate::continuation_journal::ContinuationJournal::open(
                &directory.join("recording.sqlite"),
                "race",
                &json!({}),
                crate::continuation_journal::JournalQuota {
                    max_bytes: 1024 * 1024,
                    max_records: 10,
                },
            )
            .unwrap(),
        );
        session
            .outbox
            .attach_journal(journal, json!({"epoch":"test"}))
            .unwrap();
        drop(routes);
        pass.await.unwrap();
        let threads = wake.0.lock().unwrap();
        assert!(
            !threads.is_empty(),
            "terminal ingress must really be delivered"
        );
        assert!(
            threads.iter().all(|thread| *thread != runtime_thread),
            "a pass admitted before attachment committed on the runtime thread"
        );
    }
}

/// Process-host owner. Dropping a renderer does not drop this object; host
/// shutdown stops supervision and the central remains authoritative for release.
pub struct DesktopContinuation {
    pub engine: crate::continuation::NativeContinuation,
    monitor: tokio::task::JoinHandle<()>,
}

impl DesktopContinuation {
    pub fn new<B: RadioBoundary>(
        central: DesktopCentral<B>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self::new_with_recording_registry(central, runtime, Arc::default())
    }
    pub fn new_with_recording_registry<B: RadioBoundary>(
        central: DesktopCentral<B>,
        runtime: tokio::runtime::Handle,
        recordings: Arc<crate::continuation_journal::JournalRegistry>,
    ) -> Self {
        let mut lifecycle = central.lifecycle_events();
        let mut adapter = central.adapter_events();
        let engine = crate::continuation::NativeContinuation::new_with_recording_registry(
            Arc::new(DesktopContinuationHost::with_runtime(
                central,
                runtime.clone(),
            )),
            recordings,
        );
        let recovery = engine.clone();
        let monitor_runtime = runtime.clone();
        let monitor = runtime.spawn(async move {
            loop {
                let closed = tokio::select! {
                    result = lifecycle.recv() => matches!(result, Err(tokio::sync::broadcast::error::RecvError::Closed)),
                    result = adapter.recv() => matches!(result, Err(tokio::sync::broadcast::error::RecvError::Closed)),
                };
                if closed { break; }
                recovery.request_recovery(&monitor_runtime);
            }
        });
        Self { engine, monitor }
    }
}

impl Drop for DesktopContinuation {
    fn drop(&mut self) {
        self.engine.stop_recovery();
        self.monitor.abort();
    }
}

pub struct DesktopContinuationHost<B: RadioBoundary> {
    central: DesktopCentral<B>,
    runtime: tokio::runtime::Handle,
}
impl<B: RadioBoundary> DesktopContinuationHost<B> {
    pub fn new(central: DesktopCentral<B>) -> Self {
        Self::with_runtime(central, tokio::runtime::Handle::current())
    }
    pub fn with_runtime(central: DesktopCentral<B>, runtime: tokio::runtime::Handle) -> Self {
        Self { central, runtime }
    }
}

struct NoWake;
impl WakeSink for NoWake {
    fn wake(&self, _: u64) {}
}

#[derive(Clone)]
struct Route {
    peer: String,
    selector: PathSelector,
    consumer: String,
    live: bool,
    delivery: &'static str,
}
struct Session<B: RadioBoundary> {
    central: DesktopCentral<B>,
    lease: String,
    peer: Mutex<Option<String>>,
    routes: Mutex<Vec<Route>>,
    outbox: Outbox,
    last_error: Mutex<Option<Value>>,
}

impl<B: RadioBoundary> ContinuationHost for DesktopContinuationHost<B> {
    fn open_session(&self) -> Result<Arc<dyn ContinuationSession>> {
        if self.central.is_shut_down() {
            return Err(failure("lifecycle.destroyed", "central is shut down"));
        }
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(Session {
            central: self.central.clone(),
            lease: format!("ubm-native-continuation-{id}"),
            peer: Mutex::new(None),
            routes: Mutex::new(Vec::new()),
            outbox: Outbox::new(id, Arc::new(NoWake)),
            last_error: Mutex::new(None),
        });
        let weak = Arc::downgrade(&session);
        let mut wakes = self.central.native_wakes();
        self.runtime.spawn(async move {
            loop {
                let Some(session) = weak.upgrade() else {
                    break;
                };
                if session.central.is_shut_down() {
                    break;
                }
                // One awaited pass per session: native bounded hubs retain
                // backpressure; no extra writer queue or detached disk work.
                let more = match session.collect_pass().await {
                    Ok(more) => more,
                    Err(error) => {
                        *session.last_error.lock().await = Some(error);
                        let owned = session.clone();
                        let result = crate::continuation_journal::run_blocking_result(move || {
                            owned.outbox.fail_collection_worker();
                            Ok(())
                        })
                        .await;
                        if let Err(error) = result {
                            *session.last_error.lock().await = Some(error);
                        }
                        break;
                    }
                };
                drop(session);
                if more {
                    tokio::task::yield_now().await;
                } else if matches!(
                    wakes.recv().await,
                    Err(tokio::sync::broadcast::error::RecvError::Closed)
                ) {
                    break;
                }
            }
        });
        Ok(session)
    }
}

fn failure(code: &str, detail: &str) -> Value {
    json!({"code":code,"domain":"restoration","operation":"continuation","detail":detail})
}

fn error(error: DesktopError) -> Value {
    let platform = error.platform().map(|platform| {
        let metadata:serde_json::Map<String, Value> = platform.metadata.iter().map(|(key,value)| {
            let value = match value { crate::PlatformValue::Int(value) => json!(value), crate::PlatformValue::Text(value) => json!(value), crate::PlatformValue::Bool(value) => json!(value) };
            (key.clone(), value)
        }).collect();
        json!({"domain":platform.domain,"code":platform.code,"message":platform.message.as_deref().or(error.detail()),"metadata":metadata})
    });
    json!({"code":error.code_str(),"domain":error.domain().as_str(),"operation":error.operation(),
        "detail":error.detail(),"retryability":error.retryability().as_str(),"platform":platform})
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| failure("argument.invalid", key))
}

impl<B: RadioBoundary> Session<B> {
    async fn collect_pass(self: &Arc<Self>) -> Result<bool> {
        // Attachment may occur while this pass awaits a route or native poll.
        // A pre-await has_journal snapshot cannot authorize inline execution.
        let owned = self.clone();
        let runtime = tokio::runtime::Handle::current();
        crate::continuation_journal::run_blocking_result(move || {
            Ok(runtime.block_on(owned.collect()))
        })
        .await
    }
    async fn collect(&self) -> bool {
        let mut more = false;
        let mut routes = self.routes.lock().await;
        for route in routes.iter_mut().filter(|route| route.live) {
            // Fair bounded work: a busy consumer cannot monopolize the native task.
            for index in 0..64 {
                let result = self
                    .central
                    .poll_notification(&route.peer, &route.selector, &route.consumer)
                    .await;
                let terminal = match result {
                    Ok(NotificationPoll::Value(bytes)) => {
                        let record = json!({"t":"value","consumer":route.consumer,"valueB64":encode_base64(&bytes),"delivery":route.delivery});
                        if let Err(failure) = self.outbox.push_data(record) {
                            if !self.outbox.is_sealed() {
                                let (reason, bytes) = match failure {
                                    crate::continuation_outbox::DataIngressFailure::Stopped {
                                        bytes,
                                    } => ("closed", bytes),
                                    crate::continuation_outbox::DataIngressFailure::Overflow {
                                        bytes,
                                    } => ("overflow", bytes),
                                    crate::continuation_outbox::DataIngressFailure::Storage {
                                        bytes,
                                        error,
                                    } => {
                                        *self.last_error.lock().await =
                                            Some(crate::continuation::recording_failure(error));
                                        ("closed", bytes)
                                    }
                                };
                                Some(
                                    json!({"t":"stream-end","consumer":route.consumer,"reason":reason,"droppedItems":1,"droppedBytes":bytes}),
                                )
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    Ok(NotificationPoll::Empty) => break,
                    Ok(NotificationPoll::Terminal(terminal)) => {
                        self.outbox.note_after_cutoff_loss(
                            terminal.dropped_items(),
                            terminal.dropped_bytes(),
                        );
                        Some(
                            json!({"t":"stream-end","consumer":route.consumer,"reason":terminal.reason(),"droppedItems":terminal.dropped_items(),"droppedBytes":terminal.dropped_bytes()}),
                        )
                    }
                    Ok(NotificationPoll::Invalidated(_)) => Some(
                        json!({"t":"stream-end","consumer":route.consumer,"reason":"invalidated","droppedItems":0,"droppedBytes":0}),
                    ),
                    Ok(NotificationPoll::Closed) => Some(
                        json!({"t":"stream-end","consumer":route.consumer,"reason":"closed","droppedItems":0,"droppedBytes":0}),
                    ),
                    Err(error) => {
                        *self.last_error.lock().await = Some(self::error(error));
                        Some(
                            json!({"t":"stream-end","consumer":route.consumer,"reason":"closed","droppedItems":0,"droppedBytes":0}),
                        )
                    }
                };
                if let Some(terminal) = terminal {
                    self.outbox.push_control(terminal);
                    route.live = false;
                    break;
                }
                if index == 63 {
                    more = true;
                }
            }
        }
        more
    }

    async fn invoke(&self, op: &str, args: Value) -> Result<Value> {
        let ctl = || {
            OpControl::budget_ms(args["budgetMs"].as_u64().unwrap_or(10000))
                .with_connection_lease(self.lease.clone())
        };
        match op {
            "connection.request-mtu" => Err(failure(
                "capability.unsupported",
                "desktop native MTU negotiation is OS-managed",
            )),
            "connection.connect" => {
                let peer = text(&args, "peerId")?;
                self.central
                    .connect(peer, &self.lease, ctl())
                    .await
                    .map_err(error)?;
                // The central owns compensation for failed acquisition. Only
                // a successful result transfers a lease to this session.
                *self.peer.lock().await = Some(peer.to_owned());
                Ok(json!({"state":"connected"}))
            }
            "gatt.discover" => {
                self.central
                    .discover(text(&args, "peerId")?, &self.lease, ctl())
                    .await
                    .map_err(error)?;
                Ok(json!({"state":"current"}))
            }
            "gatt.subscribe" => {
                let peer = text(&args, "peerId")?;
                let consumer = text(&args, "consumer")?;
                let selector = &args["selector"];
                let selector = DesktopCentral::<B>::selector(
                    text(selector, "serviceUuid")?,
                    selector["serviceOccurrence"].as_u64(),
                    Some(text(selector, "characteristicUuid")?),
                    selector["characteristicOccurrence"].as_u64(),
                    None,
                    None,
                )
                .map_err(error)?;
                {
                    let mut routes = self.routes.lock().await;
                    if !routes.iter().any(|route| route.consumer == consumer) {
                        routes.push(Route {
                            peer: peer.into(),
                            selector: selector.clone(),
                            consumer: consumer.into(),
                            live: false,
                            delivery: "unknown",
                        });
                    }
                }
                // Keep provisional ownership, but never block collection of
                // existing routes behind an OS enable completion.
                let delivery = self
                    .central
                    .subscribe(peer, &selector, consumer, None, ctl())
                    .await
                    .map_err(error)?;
                {
                    let mut routes = self.routes.lock().await;
                    if let Some(route) = routes.iter_mut().find(|route| route.consumer == consumer)
                    {
                        route.live = true;
                        route.delivery = delivery.as_str();
                    }
                }
                // An early value wake may have been consumed while this route
                // was provisional; drain again after publishing its delivery.
                self.collect().await;
                Ok(json!({"delivery":delivery.as_str()}))
            }
            "gatt.write" => {
                let selector = &args["selector"];
                let selector = DesktopCentral::<B>::selector(
                    text(selector, "serviceUuid")?,
                    selector["serviceOccurrence"].as_u64(),
                    Some(text(selector, "characteristicUuid")?),
                    selector["characteristicOccurrence"].as_u64(),
                    None,
                    None,
                )
                .map_err(error)?;
                let value =
                    crate::continuation_outbox::decode_base64(text(&args, "valueB64")?, 512)
                        .map_err(|_| failure("argument.invalid", "invalid setup write bytes"))?;
                self.central
                    .write(
                        text(&args, "peerId")?,
                        &selector,
                        value,
                        "with-response",
                        ctl(),
                    )
                    .await
                    .map_err(error)?;
                Ok(json!({"state":"written"}))
            }
            "session.reconcile" => {
                self.collect().await;
                let peers = self.central.peer_records().await;
                let links:Vec<Value> = peers.iter().filter(|peer| peer.connection_state == Some(ConnectionState::Connected)).map(|peer| json!({"peerId":peer.peer_id,"state":"connected","connectionGeneration":peer.connection_generation,"databaseGeneration":peer.database_generation,"databaseState":if peer.database_state == Some(DatabaseState::Current) {"current"} else {"stale"}})).collect();
                let routes = self.routes.lock().await;
                let subscriptions:Vec<Value> = routes.iter().map(|route| json!({"consumer":route.consumer,"state":if route.live {"live"} else {"closed"}})).collect();
                Ok(json!({"links":links,"subscriptions":subscriptions}))
            }
            "session.quiesce" => {
                self.collect().await;
                let loss = self.outbox.seal();
                Ok(
                    json!({"state":"sealed","afterCutoffItems":loss.items,"afterCutoffBytes":loss.bytes}),
                )
            }
            "session.continuation-dispose" => {
                self.collect().await;
                let routes = self.routes.lock().await.clone();
                let mut failures = Vec::new();
                for route in routes {
                    let drain = |observation| {
                        match observation {
                            NotificationPoll::Value(bytes) => {
                                // The sealed outbox accounts for every accepted
                                // post-cutoff value, including the disable tail.
                                let _ = self.outbox.push_data(json!({"t":"value","consumer":route.consumer,"valueB64":encode_base64(&bytes),"delivery":route.delivery}));
                            }
                            NotificationPoll::Terminal(terminal) => {
                                self.outbox.note_after_cutoff_loss(
                                    terminal.dropped_items(),
                                    terminal.dropped_bytes(),
                                );
                                self.outbox.push_control(json!({"t":"stream-end","consumer":route.consumer,"reason":terminal.reason(),"droppedItems":terminal.dropped_items(),"droppedBytes":terminal.dropped_bytes()}));
                            }
                            _ => {}
                        }
                    };
                    match self
                        .central
                        .unsubscribe_draining(
                            &route.peer,
                            &route.selector,
                            &route.consumer,
                            ctl(),
                            Some(&drain),
                        )
                        .await
                    {
                        Ok(_) => self
                            .routes
                            .lock()
                            .await
                            .retain(|owned| owned.consumer != route.consumer),
                        Err(failure) => {
                            failures.push(error(failure));
                        }
                    }
                    self.collect().await;
                }
                let mut peer = self.peer.lock().await;
                if let Some(peer_id) = peer.as_ref() {
                    match self
                        .central
                        .release_connection_lease(peer_id, &self.lease, ctl())
                        .await
                    {
                        Ok(physical_released) => {
                            if physical_released {
                                self.collect().await;
                                self.routes.lock().await.clear();
                                failures.clear();
                            }
                            *peer = None;
                        }
                        Err(failure) => failures.push(error(failure)),
                    }
                }
                let loss = self.outbox.after_cutoff_loss();
                Ok(
                    json!({"state":if failures.is_empty() {"released"} else {"release-failed"},"failures":failures,"afterCutoffItems":loss.items,"afterCutoffBytes":loss.bytes}),
                )
            }
            "counters.describe" => Ok(
                json!({"queuedData":self.outbox.queued_data(),"lastError":*self.last_error.lock().await}),
            ),
            _ => Err(failure("capability.unsupported", op)),
        }
    }
}

impl<B: RadioBoundary> ContinuationSession for Session<B> {
    fn seal_collection(&self) -> Result<()> {
        self.outbox.seal();
        Ok(())
    }
    fn collection_sealed(&self) -> bool {
        self.outbox.is_sealed()
    }
    fn attach_journal(
        &self,
        journal: Arc<crate::continuation_journal::ContinuationJournal>,
        mut context: Value,
    ) -> Result<()> {
        context["sessionId"] = json!(self.lease);
        context["backendInstanceId"] =
            json!(self.central.attachment().backend_instance_id().as_str());
        context["sessionStartedAtUnixNs"] = json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| failure("platform.failure", "recording session clock unavailable"))?
                .as_nanos()
                .to_string()
        );
        context["sessionEpoch"] = json!(format!(
            "{}:{}:{}",
            context["backendInstanceId"]
                .as_str()
                .expect("backend instance"),
            self.lease,
            context["sessionStartedAtUnixNs"]
                .as_str()
                .expect("session clock")
        ));
        self.outbox
            .attach_journal(journal, context)
            .map_err(crate::continuation::recording_failure)
    }
    fn register_journal_consumer(&self, consumer: &str, metadata: Value) -> Result<()> {
        self.outbox
            .register_journal_consumer(consumer, metadata)
            .map_err(crate::continuation::recording_failure)
    }
    fn observe(
        &self,
        consumer: &str,
        matcher: crate::continuation_outbox::RecordMatcher,
    ) -> Result<crate::continuation_outbox::Observation> {
        self.outbox
            .observe(consumer, matcher)
            .map_err(|detail| failure("lifecycle.invalid-state", detail))
    }
    fn call<'a>(&'a self, op: &'a str, args: &'a str) -> ContinuationFuture<'a> {
        Box::pin(async move {
            let result = match serde_json::from_str(args) {
                Ok(args) => self.invoke(op, args).await,
                Err(_) => Err(failure("argument.invalid", "invalid operation JSON")),
            };
            match result { Ok(value) => json!({"ok":true,"value":value}), Err(error) => json!({"ok":false,"retryability":error.get("retryability").cloned().unwrap_or(json!("never")),"error":error}) }.to_string()
        })
    }
    fn drain(&self, max_items: u32, max_bytes: u32) -> ContinuationFuture<'_> {
        Box::pin(async move {
            self.outbox
                .drain(max_items as usize, max_bytes as usize)
                .to_string()
        })
    }
}
