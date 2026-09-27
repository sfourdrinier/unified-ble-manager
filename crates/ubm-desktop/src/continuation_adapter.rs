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
        let mut lifecycle = central.lifecycle_events();
        let mut adapter = central.adapter_events();
        let engine = crate::continuation::NativeContinuation::new(Arc::new(
            DesktopContinuationHost::with_runtime(central, runtime.clone()),
        ));
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
                let more = session.collect().await;
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
                        if let Err(overflow) = self.outbox.push_data(record) {
                            if !self.outbox.is_sealed() {
                                Some(
                                    json!({"t":"stream-end","consumer":route.consumer,"reason":"overflow","droppedItems":1,"droppedBytes":overflow.bytes}),
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
        let ctl = || OpControl::budget_ms(args["budgetMs"].as_u64().unwrap_or(10000));
        match op {
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
            "session.reconcile" => {
                self.collect().await;
                let peers = self.central.peer_records().await;
                let links:Vec<Value> = peers.iter().filter(|peer| peer.connection_state == Some(ConnectionState::Connected)).map(|peer| json!({"peerId":peer.peer_id,"state":"connected","databaseState":if peer.database_state == Some(DatabaseState::Current) {"current"} else {"stale"}})).collect();
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
