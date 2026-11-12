// vendor/btleplug/src/bluez/peer_source.rs
//! Connection-scoped peer state and synchronous ordered notification ingress.

use super::peripheral::{publish_gatt_locked, value_notification_from_graph, PublishedGatt};
use crate::api::NotificationEvent;
use crate::common::util::{
    NotificationFaults, SourceGroupConsumer, SourceGroupError, SourceGroupHandle,
    SourceWriteReceipt, SourceWriteRequest, SourceWriteStart,
};
use crate::ubm_peer_notification_ingress::{Membership, NotificationRoute};
use crate::ubm_reclaim_graph_gate::{Admission, DrainItem, GateError, GraphGate};
use crate::ubm_reclaim_authority::{AuthorityError, GraphKey, PermitStatus, ReclaimAuthorities};
use crate::ubm_reclaim_graph_binding::GraphBinding;
use bluez_async::{BluetoothSession, DeviceId, PeerObserverEvent, PeerObserverLease};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};

pub(crate) struct PeerRegistry {
    peers: Arc<Mutex<RegistryEntries>>,
    authorities: Arc<Mutex<ReclaimAuthorities>>,
    lifecycle: tokio::sync::broadcast::Sender<PeerLifecycleEvent>,
    failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
}
#[derive(Default)]
struct RegistryEntries {
    peers: HashMap<String, Weak<PeerSource>>,
    retained: HashMap<String, Arc<PeerSource>>,
}
#[derive(Clone, Debug)]
pub(super) enum PeerLifecycleEvent {
    Uncertain(DeviceId, Option<(String, u64, u64)>),
    GraphRetired(DeviceId, (String, u64, u64)),
    EventsLost(u64),
}
impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new(None)
    }
}
impl PeerRegistry {
    pub(crate) fn new(
        failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    ) -> Self {
        let (lifecycle, _) = tokio::sync::broadcast::channel(256);
        Self {
            peers: Arc::new(Mutex::new(RegistryEntries::default())),
            authorities: Arc::new(Mutex::new(ReclaimAuthorities::default())),
            lifecycle,
            failure_evidence_store,
        }
    }
}
impl std::fmt::Debug for PeerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRegistry").finish_non_exhaustive()
    }
}
impl PeerRegistry {
    pub(crate) fn reclaim_authority_handle(&self) -> crate::ubm_reclaim_authority::ReclaimAuthorityHandle {
        crate::ubm_reclaim_authority::ReclaimAuthorityHandle::from_owner(self.authorities.clone())
    }
    pub(crate) fn get(&self, device: &DeviceId) -> crate::Result<Arc<PeerSource>> {
        let peer = self.get_by_peer(&device.to_string())?;
        peer.state.lock().expect("peer source lock poisoned").device_id = Some(device.clone());
        Ok(peer)
    }
    pub(super) fn lifecycle_events(&self) -> tokio::sync::broadcast::Receiver<PeerLifecycleEvent> { self.lifecycle.subscribe() }
    pub(crate) fn get_by_peer(&self, peer_id: &str) -> crate::Result<Arc<PeerSource>> {
        let mut entries = self.peers.lock().expect("peer registry lock poisoned");
        if let Some(peer) = entries.peers.get(peer_id).and_then(Weak::upgrade) {
            return Ok(peer);
        }
        // A disposable peripheral is not the owner of an unresolved native
        // effect. Retain those peers until a daemon fence resolves them; idle
        // peers without outstanding obligations can be collected.
        entries.retained.retain(|_, peer| peer.must_retain());
        entries.peers.retain(|_, peer| peer.strong_count() != 0);
        if entries.peers.len() >= 4096 {
            return Err(crate::Error::Other("BlueZ peer owner capacity exhausted".into()));
        }
        let peer = Arc::new(PeerSource::with_registry(
            self.authorities.clone(),
            self.lifecycle.clone(),
            Some((Arc::downgrade(&self.peers), peer_id.to_owned())),
            self.failure_evidence_store.clone(),
        ));
        entries.peers.insert(peer_id.to_owned(), Arc::downgrade(&peer));
        Ok(peer)
    }
}
#[derive(Default)]
struct State {
    generation: Option<u64>,
    observer: Option<PeerObserverLease>,
    daemon: Option<String>,
    watch_id: u64,
    watching: bool,
    connected: bool,
    resolved: bool,
    revision: u64,
    graph_revision: Option<u64>,
    reclaim_binding: crate::ubm_reclaim_graph_binding::GraphBindingOwner,
    graph_operations: usize,
    uncertain_owners: HashSet<String>,
    uncertain_unknown_owner: bool,
    callback_uncertain: bool,
    callbacks_inflight: usize,
    reclaim_gate: Option<GraphGate<DeferredPeerEvent>>,
    reclaim_graph: Option<(String, u64, u64)>,
    device_id: Option<DeviceId>,
    lifecycle_queue: VecDeque<(PeerLifecycleEvent, usize)>,
    lifecycle_bytes: usize,
    lifecycle_running: bool,
    lifecycle_lost: u64,
}
impl State {
    fn observe_daemon(&mut self, daemon: String) {
        // A call pinned to a retired unique D-Bus owner cannot later affect a
        // different owner. Unknown or same-owner outcomes remain uncertain.
        self.uncertain_owners.retain(|owner| owner == &daemon);
        self.daemon = Some(daemon);
    }
}
enum DeferredPeerEvent {
    Observer { device: DeviceId, watch_id: u64, event: PeerObserverEvent },
    Close { watch_id: u64 },
    ActivationFailure { watch: u64, revision: u64, generation: u64, activation: u64, route: NotificationRoute, error: crate::PlatformError },
}
#[derive(Clone)]
pub(super) enum GraphOperationScope {
    Observation,
    Effect(Option<String>),
    SingleReplyEffect(Option<String>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GraphOperationSettlement { Confirmed, Rejected, ObservationFailed, Indeterminate }
pub(super) fn graph_operation_settlement<T>(scope: &GraphOperationScope, result: &crate::Result<T>) -> GraphOperationSettlement {
    if result.is_ok() { return GraphOperationSettlement::Confirmed; }
    if matches!(scope, GraphOperationScope::Observation) { return GraphOperationSettlement::ObservationFailed; }
    if let (GraphOperationScope::SingleReplyEffect(_), Err(crate::Error::Platform(error))) = (scope, result) {
        if error.domain == "bluez-dbus" && matches!(error.code.as_str(),
            "org.bluez.Error.NotAuthorized" | "org.bluez.Error.NotSupported" |
            "org.bluez.Error.InvalidArguments" | "org.bluez.Error.NotReady") {
            return GraphOperationSettlement::Rejected;
        }
    }
    GraphOperationSettlement::Indeterminate
}
pub(super) fn lease_release_settlement(scope: &GraphOperationScope, result: &crate::Result<bluez_async::LeLeaseReleaseReceipt>) -> GraphOperationSettlement {
    if matches!(result, Ok(receipt) if receipt.scope == bluez_async::LeLeaseReleaseScope::LeaseReleasedIndeterminate) {
        GraphOperationSettlement::Indeterminate
    } else {
        graph_operation_settlement(scope, result)
    }
}
pub(crate) struct PeerSource {
    pub(super) services: Arc<Mutex<PublishedGatt>>,
    faults: NotificationFaults,
    state: Mutex<State>,
    watch_gate: tokio::sync::Mutex<()>,
    watch_changed: tokio::sync::Notify,
    authorities: Arc<Mutex<ReclaimAuthorities>>,
    lifecycle: tokio::sync::broadcast::Sender<PeerLifecycleEvent>,
    registry: Option<(Weak<Mutex<RegistryEntries>>, String)>,
}
impl std::fmt::Debug for PeerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerSource").finish_non_exhaustive()
    }
}

#[cfg(feature = "reclaim-test-support")]
#[path = "peer_source_fixture.rs"]
mod fixture;
#[cfg(feature = "reclaim-test-support")]
pub use fixture::ReclaimSourceFixture;
impl Default for PeerSource {
    fn default() -> Self {
        Self::with_authorities(Arc::new(Mutex::new(ReclaimAuthorities::default())))
    }
}
impl PeerSource {
    fn must_retain(&self) -> bool {
        let state = self.state.lock().expect("peer source lock poisoned");
        state.graph_operations != 0 || state.callback_uncertain || state.uncertain_unknown_owner || !state.uncertain_owners.is_empty()
            || state.reclaim_gate.as_ref().is_some_and(GraphGate::is_busy)
    }
    fn retain_obligation(&self) {
        if let Some((registry, peer_id)) = &self.registry {
            if let Some(registry) = registry.upgrade() {
                let mut entries = registry.lock().expect("peer registry lock poisoned");
                if let Some(peer) = entries.peers.get(peer_id).and_then(Weak::upgrade) {
                    entries.retained.insert(peer_id.clone(), peer);
                }
            }
        }
    }
    #[cfg(test)]
    pub(super) fn marker_reclaim_snapshot(&self) -> (usize, bool) {
        let state = self.state.lock().expect("peer source lock poisoned");
        (state.graph_operations, !state.uncertain_owners.is_empty() || state.uncertain_unknown_owner)
    }
    fn emit_source_uncertain(&self, graph: Option<(String, u64, u64)>) {
        let start = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            let Some(id) = state.device_id.clone() else { return; };
            Self::queue_lifecycle(&mut state, PeerLifecycleEvent::Uncertain(id, graph))
        };
        if start { self.drain_lifecycle(); }
    }
    fn queue_lifecycle(state: &mut State, event: PeerLifecycleEvent) -> bool {
        let bytes = match &event {
            PeerLifecycleEvent::Uncertain(id, graph) => id.to_string().len().saturating_add(graph.as_ref().map_or(0, |value| value.0.capacity())),
            PeerLifecycleEvent::GraphRetired(id, graph) => id.to_string().len().saturating_add(graph.0.capacity()),
            PeerLifecycleEvent::EventsLost(_) => 0,
        }.saturating_add(std::mem::size_of::<PeerLifecycleEvent>());
        if state.lifecycle_lost != 0 || state.lifecycle_queue.len() >= 256 || bytes > 524_288usize.saturating_sub(state.lifecycle_bytes) {
            state.lifecycle_lost = state.lifecycle_lost.saturating_add(1);
        } else { state.lifecycle_bytes += bytes; state.lifecycle_queue.push_back((event, bytes)); }
        if state.lifecycle_running { false } else { state.lifecycle_running = true; true }
    }
    fn drain_lifecycle(&self) {
        loop {
            let event = {
                let mut state = self.state.lock().expect("peer source lock poisoned");
                if let Some((event, bytes)) = state.lifecycle_queue.pop_front() { state.lifecycle_bytes -= bytes; event }
                else if state.lifecycle_lost != 0 { PeerLifecycleEvent::EventsLost(std::mem::take(&mut state.lifecycle_lost)) }
                else { state.lifecycle_running = false; return; }
            };
            let _ = self.lifecycle.send(event);
        }
    }
    fn with_authorities(authorities: Arc<Mutex<ReclaimAuthorities>>) -> Self {
        let (lifecycle, _) = tokio::sync::broadcast::channel(256);
        Self::with_registry(authorities, lifecycle, None, None)
    }
    fn with_registry(
        authorities: Arc<Mutex<ReclaimAuthorities>>,
        lifecycle: tokio::sync::broadcast::Sender<PeerLifecycleEvent>,
        registry: Option<(Weak<Mutex<RegistryEntries>>, String)>,
        failure_evidence_store: Option<Arc<dyn crate::ubm_peer_notification_ingress::FailureEvidenceStore>>,
    ) -> Self {
        Self {
            services: Arc::new(Mutex::new(PublishedGatt::default())),
            faults: NotificationFaults::new_with_failure_evidence_store(256, failure_evidence_store),
            state: Mutex::new(State::default()),
            watch_gate: tokio::sync::Mutex::new(()),
            watch_changed: tokio::sync::Notify::new(),
            authorities,
            lifecycle,
            registry,
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum PeerSourceError {
    #[error("notification source state: {0:?}")]
    Source(SourceGroupError),
    #[error("BlueZ observer: {0}")]
    Observer(#[from] bluez_async::PeerObserverError),
    #[error("BlueZ device: {0}")]
    Device(#[from] bluez_async::BluetoothError),
}
impl PeerSource {
    /// Own a graph-changing operation until its native future settles, even if
    /// the initiating caller abandons its waiter. Indeterminate physical
    /// effects remain fenced for subsequent reclaim admission.
    pub(super) async fn own_graph_operation<T, F>(self: &Arc<Self>, scope: GraphOperationScope, future: F) -> crate::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = crate::Result<T>> + Send + 'static,
    {
        self.own_classified_graph_operation(scope, future, graph_operation_settlement).await
    }
    pub(super) async fn own_classified_graph_operation<T, F, C>(self: &Arc<Self>, scope: GraphOperationScope, future: F, classify: C) -> crate::Result<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = crate::Result<T>> + Send + 'static,
        C: FnOnce(&GraphOperationScope, &crate::Result<T>) -> GraphOperationSettlement + Send + 'static,
    {
        {
            let mut state = self.state.lock().map_err(|_| crate::Error::Other("BlueZ peer state lock poisoned".into()))?;
            if state.reclaim_gate.as_ref().is_some_and(GraphGate::is_busy) {
                return Err(crate::Error::Other("BlueZ graph is held by a reclaim permit".into()));
            }
            if state.graph_operations >= 256 {
                return Err(crate::Error::Other("BlueZ graph operation capacity exhausted".into()));
            }
            state.graph_operations = state.graph_operations.checked_add(1)
                .ok_or_else(|| crate::Error::Other("BlueZ graph operation capacity exhausted".into()))?;
        }
        let guard = GraphOperationGuard { peer: self.clone(), scope, settlement: GraphOperationSettlement::Indeterminate };
        tokio::spawn(async move {
            let mut guard = guard;
            let result = future.await;
            guard.settlement = classify(&guard.scope, &result);
            result
        }).await.map_err(|error| crate::Error::Other(format!("BlueZ graph operation task failed: {error}").into()))?
    }
    #[cfg(test)]
    pub(super) fn acquire_reclaim_gate(&self, binding: &GraphBinding, key: GraphKey) -> Result<(), GateError> {
        let _graph = self.services.lock().expect("peer graph lock poisoned");
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if !state.reclaim_binding.owns(binding) { return Err(GateError::Invalidated); }
        if state.graph_operations != 0 || state.callbacks_inflight != 0 || state.callback_uncertain || state.uncertain_unknown_owner || !state.uncertain_owners.is_empty() { return Err(GateError::Busy); }
        if !state.watching || !state.connected || !state.resolved || state.graph_revision != Some(state.revision)
            || !state.reclaim_binding.validates(binding, &key.peer_id, &key.connection_generation, &key.graph) {
            return Err(GateError::Invalidated);
        }
        state.reclaim_gate.get_or_insert_with(|| GraphGate::new(256, 524_288)).acquire(key.clone())?;
        state.reclaim_graph = Some(key.graph);
        drop(state);
        drop(_graph);
        self.retain_obligation();
        Ok(())
    }
    /// Atomically record the source receipt and acquire the native peer gate.
    /// Registry precedes graph and peer state in the source lock order.
    pub(super) fn acquire_reclaim(&self, binding: &GraphBinding, key: GraphKey) -> Result<PermitStatus, AuthorityError> {
        let mut authorities = self.authorities.lock().expect("reclaim authority lock poisoned");
        let _graph = self.services.lock().expect("peer graph lock poisoned");
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if !state.reclaim_binding.owns(binding) { return Err(AuthorityError::Conflict); }
        let busy = state.graph_operations != 0 || state.callbacks_inflight != 0 || state.callback_uncertain || state.uncertain_unknown_owner || !state.uncertain_owners.is_empty()
            || state.reclaim_gate.as_ref().is_some_and(GraphGate::is_busy);
        let valid = state.watching && state.connected && state.resolved && state.graph_revision == Some(state.revision)
            && state.reclaim_binding.validates(binding, &key.peer_id, &key.connection_generation, &key.graph);
        let status = authorities.acquire_bound(key.clone(), binding, valid, busy)?;
        if status == PermitStatus::Acquired {
            state.reclaim_gate.get_or_insert_with(|| GraphGate::new(256, 524_288)).acquire(key.clone())
                .map_err(|_| AuthorityError::Conflict)?;
            state.reclaim_graph = Some(key.graph);
        }
        drop(state);
        drop(_graph);
        drop(authorities);
        if status == PermitStatus::Acquired { self.retain_obligation(); }
        Ok(status)
    }
    pub(super) fn commit_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        let mut authorities = self.authorities.lock().expect("reclaim authority lock poisoned");
        let mut state = self.state.lock().expect("peer source lock poisoned");
        authorities.authenticate_permit_owner(key, &state.reclaim_binding)?;
        match state.reclaim_gate.as_mut().ok_or(AuthorityError::Unknown)?.commit(key) {
            Ok(()) => authorities.commit(key),
            Err(GateError::Invalidated) => authorities.invalidate_before_commit(key),
            Err(_) => Err(AuthorityError::Conflict),
        }
    }
    pub(super) fn release_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        let (status, watch_id) = {
            let mut authorities = self.authorities.lock().expect("reclaim authority lock poisoned");
            let mut state = self.state.lock().expect("peer source lock poisoned");
            authorities.authenticate_permit_owner(key, &state.reclaim_binding)?;
            let current = authorities.permit_status(key)?;
            if current == PermitStatus::Released {
                return if state.reclaim_gate.as_ref().is_some_and(GraphGate::is_draining) {
                    Err(AuthorityError::Pending)
                } else { Ok(current) };
            }
            if !matches!(current, PermitStatus::Acquired | PermitStatus::Committed | PermitStatus::Invalidated) { return Err(AuthorityError::Conflict); }
            state.reclaim_gate.as_mut().ok_or(AuthorityError::Unknown)?.release(key).map_err(|_| AuthorityError::Conflict)?;
            let status = authorities.release(key)?;
            (status, state.watch_id)
        };
        self.drain_reclaim_gate(watch_id).map_err(|_| AuthorityError::Conflict)?;
        Ok(status)
    }
    pub(super) fn forget_reclaim(&self, key: &GraphKey) -> Result<(), AuthorityError> {
        let mut authorities = self.authorities.lock().expect("reclaim authority lock poisoned");
        let state = self.state.lock().expect("peer source lock poisoned");
        match authorities.authenticate_permit_owner(key, &state.reclaim_binding) {
            Ok(()) | Err(AuthorityError::Unknown) => {},
            Err(error) => return Err(error),
        }
        if state.reclaim_gate.as_ref().is_some_and(GraphGate::is_draining) {
            return Err(AuthorityError::Pending);
        }
        authorities.forget_permit(key)
    }
    #[cfg(test)]
    fn commit_reclaim_gate(&self, key: &GraphKey) -> Result<(), GateError> {
        self.state.lock().expect("peer source lock poisoned").reclaim_gate.as_mut().ok_or(GateError::NotCurrent)?.commit(key)
    }
    #[cfg(test)]
    fn release_reclaim_gate(&self, key: &GraphKey) -> Result<(), GateError> {
        let watch_id = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            state.reclaim_gate.as_mut().ok_or(GateError::NotCurrent)?.release(key)?;
            state.watch_id
        };
        self.drain_reclaim_gate(watch_id)
    }
    fn drain_reclaim_gate(&self, watch_id: u64) -> Result<(), GateError> {
        loop {
            let item = {
                let mut state = self.state.lock().expect("peer source lock poisoned");
                let item = state.reclaim_gate.as_mut().ok_or(GateError::NotCurrent)?.next()?;
                if matches!(&item, DrainItem::Finished) { state.reclaim_graph = None; }
                item
            };
            match item {
                DrainItem::Event(DeferredPeerEvent::Observer { device, watch_id, event }) => self.event_now(&device, watch_id, event),
                DrainItem::Event(DeferredPeerEvent::Close { watch_id }) => self.invalidate(watch_id),
                DrainItem::Event(DeferredPeerEvent::ActivationFailure { watch, revision, generation, activation, route, error }) => self.activation_failed_now(watch, revision, generation, activation, route, error),
                DrainItem::Overflow => {
                    let graph = self.state.lock().expect("peer source lock poisoned").reclaim_graph.clone();
                    self.fail_uncertain(watch_id);
                    self.emit_source_uncertain(graph);
                },
                DrainItem::Barrier { .. } | DrainItem::BarrierOverflow => {
                    log::error!("Unexpected BlueZ reclaim barrier; failing source");
                    let graph = self.state.lock().expect("peer source lock poisoned").reclaim_graph.clone();
                    self.fail_uncertain(watch_id);
                    self.emit_source_uncertain(graph);
                },
                DrainItem::Finished => return Ok(()),
            }
        }
    }
    fn invalidate(&self, watch_id: u64) {
        let _wake_scope = self.faults.defer_wakes();
        // All paths that need both locks take graph before state.
        let mut graph = self.services.lock().expect("peer graph lock poisoned");
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if !state.watching || state.watch_id != watch_id {
            return;
        }
        state.watching = false;
        state.connected = false;
        state.daemon = None;
        state.resolved = false;
        state.graph_revision = None;
        state.reclaim_binding.disconnect();
        state.revision = state.revision.saturating_add(1);
        *graph = PublishedGatt::default();
        if let Some(generation) = state.generation.take() {
            let _ = self.faults.close_source_generation(generation);
        }
        let observer = state.observer.take();
        drop(state);
        drop(graph);
        self.watch_changed.notify_waiters();
        drop(observer);
    }
    fn fail_uncertain(&self, watch_id: u64) {
        let _wake_scope = self.faults.defer_wakes();
        let mut graph = self.services.lock().expect("peer graph lock poisoned");
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if !state.watching || state.watch_id != watch_id { return; }
        state.graph_revision = None;
        state.reclaim_binding.invalidate_graph();
        state.callback_uncertain = true;
        state.revision = state.revision.saturating_add(1);
        *graph = PublishedGatt::default();
        if let Some(generation) = state.generation.take() {
            let failure = crate::PlatformError::new("bluez", "reclaim-callback-overflow", "BlueZ reclaim callback queue overflow");
            if let Err(error) = self.faults.fail_source_generation(generation, failure) {
                state.uncertain_unknown_owner = true;
                log::warn!("BlueZ reclaim overflow source fencing failed: {error:?}");
            }
        }
        drop(state);
        drop(graph);
        self.retain_obligation();
    }
    fn invalidate_graph(&self, watch_id: u64, unresolved: bool, connection_unknown: bool) {
        let _wake_scope = self.faults.defer_wakes();
        let mut graph = self.services.lock().expect("peer graph lock poisoned");
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if !state.watching || state.watch_id != watch_id {
            return;
        }
        state.revision = state.revision.saturating_add(1);
        if unresolved {
            state.resolved = false;
        }
        if connection_unknown {
            state.connected = false;
        }
        state.graph_revision = None;
        if connection_unknown { state.reclaim_binding.disconnect(); } else { state.reclaim_binding.invalidate_graph(); }
        *graph = PublishedGatt::default();
        if let Some(generation) = state.generation.take() {
            let _ = self.faults.close_source_generation(generation);
        }
    }

    pub(super) fn current_watch(&self) -> Option<u64> {
        let state = self.state.lock().expect("peer source lock poisoned");
        state.watching.then_some(state.watch_id)
    }
    pub(super) fn close_watch(&self, watch_id: u64) {
        self.invalidate(watch_id);
    }
    fn resolved(&self, watch_id: u64) {
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if state.watching && state.watch_id == watch_id && !state.resolved {
            state.revision = state.revision.saturating_add(1);
            state.resolved = true;
        }
    }
    pub(super) fn activation_snapshot(
        &self,
        characteristic: &crate::api::Characteristic,
    ) -> crate::Result<(u64, u64, String, bluez_async::CharacteristicId)> {
        let graph = self.services.lock().map_err(Into::<crate::Error>::into)?;
        let state = self
            .state
            .lock()
            .map_err(|_| crate::Error::Other("BlueZ peer state lock poisoned".into()))?;
        if !state.watching
            || !state.connected
            || !state.resolved
            || state.graph_revision != Some(state.revision)
        {
            return Err(crate::Error::NotConnected);
        }
        Ok((
            state.watch_id,
            state.revision,
            state.daemon.clone().ok_or(crate::Error::NotConnected)?,
            super::peripheral::route_characteristic_id(&graph, characteristic)?,
        ))
    }
    pub(super) fn activation_current(&self, watch: u64, revision: u64) -> bool {
        let state = self.state.lock().expect("peer source lock poisoned");
        state.watching
            && state.connected
            && state.resolved
            && state.watch_id == watch
            && state.revision == revision
            && state.graph_revision == Some(revision)
    }
    pub(super) async fn wait_link_end(&self, watch: u64) {
        loop {
            let notified = self.watch_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.activation_link_ended(watch) {
                return;
            }
            notified.await;
        }
    }
    fn route(characteristic: &crate::api::Characteristic) -> NotificationRoute {
        NotificationRoute {
            service_uuid: characteristic.service_uuid,
            service_instance: characteristic.service_instance,
            characteristic_uuid: characteristic.uuid,
            characteristic_instance: characteristic.instance,
        }
    }
    pub(super) fn start_activation(
        &self,
        watch: u64,
        revision: u64,
        characteristic: &crate::api::Characteristic,
        activation: u64,
        expected_generation: Option<u64>,
    ) -> Result<Option<u64>, SourceGroupError> {
        let _graph = self
            .services
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        let state = self
            .state
            .lock()
            .map_err(|_| SourceGroupError::SourceStateUnavailable)?;
        if !state.watching
            || state.watch_id != watch
            || state.revision != revision
            || state.graph_revision != Some(revision)
        {
            return Err(SourceGroupError::StaleGeneration);
        }
        if expected_generation.is_some() && state.generation != expected_generation {
            return Err(SourceGroupError::StaleGeneration);
        }
        if let Some(generation) = state.generation {
            self.faults.start_source_activation(
                generation,
                Self::route(characteristic),
                activation,
            )?;
            Ok(Some(generation))
        } else {
            Ok(None)
        }
    }
    pub(super) fn retire_activation(
        &self,
        generation: u64,
        characteristic: &crate::api::Characteristic,
        activation: u64,
    ) -> Result<(), SourceGroupError> {
        match self.faults.retire_source_activation(
            generation,
            Self::route(characteristic),
            activation,
        ) {
            Err(
                SourceGroupError::StaleGeneration
                | SourceGroupError::Ingress(
                    crate::ubm_peer_notification_ingress::IngressError::NotCurrent,
                ),
            ) => Ok(()),
            result => result,
        }
    }
    pub(super) fn activation_failed(
        &self,
        watch: u64,
        revision: u64,
        generation: u64,
        activation: u64,
        characteristic: &crate::api::Characteristic,
        error: crate::PlatformError,
    ) {
        let bytes = error.code.len().saturating_add(error.message.len()).saturating_add(
            error.metadata.iter().fold(0usize, |sum, (_, value)| sum.saturating_add(value.len()).saturating_add(std::mem::size_of::<(&'static str, String)>()))
        );
        let event = DeferredPeerEvent::ActivationFailure { watch, revision, generation, activation, route: Self::route(characteristic), error };
        let admission = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            let admission = match state.reclaim_gate.as_mut() {
                Some(gate) => gate.admit(event, bytes, false),
                None => Admission::Apply(event),
            };
            if matches!(&admission, Admission::Apply(_)) { state.callbacks_inflight += 1; }
            admission
        };
        if let Admission::Apply(DeferredPeerEvent::ActivationFailure { watch, revision, generation, activation, route, error }) = admission {
            let _callback = CallbackGuard(self);
            self.activation_failed_now(watch, revision, generation, activation, route, error);
        }
    }
    fn activation_failed_now(&self, watch: u64, revision: u64, generation: u64, activation: u64, route: NotificationRoute, error: crate::PlatformError) {
        let _wake_scope = self.faults.defer_wakes();
        let _graph = self.services.lock().expect("peer graph lock poisoned");
        let state = self.state.lock().expect("peer source lock poisoned");
        if !state.watching
            || state.watch_id != watch
            || state.revision != revision
            || state.graph_revision != Some(revision)
            || state.generation != Some(generation)
        {
            return;
        }
        let value = crate::api::ValueNotification {
            uuid: route.characteristic_uuid,
            instance: route.characteristic_instance,
            service_uuid: route.service_uuid,
            service_instance: route.service_instance,
            value: Vec::new(),
            source_failure: Some(error),
            lost_before: 0,
        };
        let _ = self.faults.publish_source_value_for_activation(
            generation,
            route,
            activation,
            value,
        );
    }
    pub(super) fn activation_link_ended(&self, watch: u64) -> bool {
        let state = self.state.lock().expect("peer source lock poisoned");
        !state.watching || state.watch_id != watch
    }

    pub(super) fn close_current(&self) {
        let admission = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            let event = DeferredPeerEvent::Close { watch_id: state.watch_id };
            let admission = match state.reclaim_gate.as_mut() {
                Some(gate) => gate.admit(event, 0, true),
                None => Admission::Apply(event),
            };
            if matches!(&admission, Admission::Apply(_)) { state.callbacks_inflight += 1; }
            admission
        };
        if let Admission::Apply(DeferredPeerEvent::Close { watch_id }) = admission {
            let _callback = CallbackGuard(self);
            self.invalidate(watch_id);
        }
    }
    fn event(&self, device: &DeviceId, watch_id: u64, event: PeerObserverEvent) {
        let invalidates = matches!(&event, PeerObserverEvent::GattGraphChanged | PeerObserverEvent::Closed(_))
            || matches!(&event, PeerObserverEvent::Event(bluez_async::BluetoothEvent::Device { id, event: bluez_async::DeviceEvent::ConnectionUnknown | bluez_async::DeviceEvent::ServicesUnresolved }) if id == device);
        let bytes = bluez_async::peer_observer_event_bytes(&event).saturating_add(device.to_string().len());
        let deferred = DeferredPeerEvent::Observer { device: device.clone(), watch_id, event };
        let admission = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            let admission = match state.reclaim_gate.as_mut() {
                Some(gate) => gate.admit(deferred, bytes, invalidates),
                None => Admission::Apply(deferred),
            };
            if matches!(&admission, Admission::Apply(_)) {
                state.callbacks_inflight = state.callbacks_inflight.checked_add(1).expect("BlueZ callback count exhausted");
            }
            admission
        };
        if let Admission::Apply(DeferredPeerEvent::Observer { device, watch_id, event }) = admission {
            let _callback = CallbackGuard(self);
            self.event_now(&device, watch_id, event);
        }
    }
    fn event_now(&self, device: &DeviceId, watch_id: u64, event: PeerObserverEvent) {
        let _wake_scope = self.faults.defer_wakes();
        match event {
            PeerObserverEvent::Event(event) => {
                if matches!(&event, bluez_async::BluetoothEvent::Device { id, event: bluez_async::DeviceEvent::ConnectionUnknown } if id == device)
                {
                    self.invalidate_graph(watch_id, false, true);
                    return;
                }
                if matches!(&event, bluez_async::BluetoothEvent::Device { id, event: bluez_async::DeviceEvent::ServicesUnresolved } if id == device)
                {
                    self.invalidate_graph(watch_id, true, false);
                    return;
                }
                if matches!(&event, bluez_async::BluetoothEvent::Device { id, event: bluez_async::DeviceEvent::ServicesResolved } if id == device)
                {
                    self.resolved(watch_id);
                    return;
                }
                let graph = self.services.lock().expect("peer graph lock poisoned");
                let state = self.state.lock().expect("peer source lock poisoned");
                if state.watching
                    && state.watch_id == watch_id
                    && state.graph_revision == Some(state.revision)
                {
                    if let Some(generation) = state.generation {
                        if let Some(value) = value_notification_from_graph(event, device, &graph) {
                            let _ = self
                                .faults
                                .publish_source_event_with_provenance(
                                    generation,
                                    NotificationEvent::Value(value),
                                    crate::ubm_peer_notification_ingress::CallbackProvenance::ReadOrNotification,
                                );
                        }
                    }
                }
            }
            PeerObserverEvent::GattGraphChanged => self.invalidate_graph(watch_id, false, false),
            PeerObserverEvent::Closed(_) => self.invalidate(watch_id),
        }
    }
    pub(super) async fn ensure_watch(
        self: &Arc<Self>,
        session: &BluetoothSession,
        device: &DeviceId,
    ) -> Result<u64, PeerSourceError> {
        let _gate = self.watch_gate.lock().await;
        let existing = {
            let state = self.state.lock().expect("peer source lock poisoned");
            (state.watching && state.observer.is_some()).then_some((
                state.watch_id,
                state.revision,
                state.connected,
                state.resolved,
            ))
        };
        if let Some((watch_id, revision, connected, resolved)) = existing {
            if connected && resolved {
                return Ok(revision);
            }
            let info = session.get_device_info(device).await?;
            self.apply_bootstrap(watch_id, revision, info.connected, info.services_resolved)?;
            return Ok(revision);
        }
        let watch_id = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            if state.watching && state.connected {
                return if state.resolved {
                    Ok(state.revision)
                } else {
                    Err(PeerSourceError::Source(SourceGroupError::StaleGeneration))
                };
            }
            state.watch_id = state
                .watch_id
                .checked_add(1)
                .ok_or(PeerSourceError::Source(
                    SourceGroupError::SourceStateUnavailable,
                ))?;
            state.watching = true;
            state.connected = false;
            state.watch_id
        };
        let mut guard = WatchGuard {
            peer: self.clone(),
            watch_id,
            armed: true,
        };
        let peer = Arc::downgrade(self);
        let device_owned = device.clone();
        let observer = session
            .observe_peer(device, move |event| {
                if let Some(peer) = peer.upgrade() {
                    peer.event(&device_owned, watch_id, event);
                }
            })
            .await?;
        let revision = {
            let mut state = self.state.lock().expect("peer source lock poisoned");
            if !state.watching || state.watch_id != watch_id {
                return Err(PeerSourceError::Source(SourceGroupError::StaleGeneration));
            }
            state.observe_daemon(observer.daemon_owner().to_owned());
            state.observer = Some(observer);
            state.revision
        };
        let info = session.get_device_info(device).await?;
        self.apply_bootstrap(watch_id, revision, info.connected, info.services_resolved)?;
        guard.armed = false;
        Ok(revision)
    }
    fn apply_bootstrap(
        &self,
        watch_id: u64,
        revision: u64,
        connected: bool,
        resolved: bool,
    ) -> Result<(), PeerSourceError> {
        let mut state = self.state.lock().expect("peer source lock poisoned");
        if !connected
            || !resolved
            || !state.watching
            || state.watch_id != watch_id
            || state.revision != revision
        {
            return Err(PeerSourceError::Source(SourceGroupError::StaleGeneration));
        }
        if !state.connected { state.reclaim_binding.attach(); }
        state.connected = true;
        state.resolved = true;
        Ok(())
    }

    pub(super) fn commit_graph(
        &self,
        revision: u64,
        candidate: PublishedGatt,
    ) -> crate::Result<()> {
        let _wake_scope = self.faults.defer_wakes();
        // Graph lock serializes the state check and publication against invalidation.
        let mut graph = self.services.lock().map_err(Into::<crate::Error>::into)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| crate::Error::Other("BlueZ peer state lock poisoned".into()))?;
        if !state.watching || !state.connected || !state.resolved || state.revision != revision {
            return Err(crate::Error::Other(
                "BlueZ connection changed during service discovery".into(),
            ));
        }
        let previous = graph.accepted_token.as_ref().map(|token| (token.daemon_owner.clone(), token.attachment, token.revision));
        publish_gatt_locked(&mut graph, candidate)?;
        if state.graph_revision.is_some() {
            state.revision = state.revision.saturating_add(1);
            if let Some(generation) = state.generation.take() {
                let _ = self.faults.close_source_generation(generation);
            }
        }
        state.graph_revision = Some(state.revision);
        if let Some(token) = &graph.accepted_token {
            let _ = state.reclaim_binding.publish((token.daemon_owner.clone(), token.attachment, token.revision));
            if state.reclaim_gate.as_ref().is_none_or(|gate| !gate.is_busy()) {
                state.callback_uncertain = false;
            }
        } else {
            state.reclaim_binding.invalidate_graph();
        }
        let current = graph.accepted_token.as_ref().map(|token| (token.daemon_owner.clone(), token.attachment, token.revision));
        let retired = previous.filter(|old| Some(old) != current.as_ref());
        let start = match (state.device_id.clone(), retired) {
            (Some(id), Some(retired)) => Self::queue_lifecycle(&mut state, PeerLifecycleEvent::GraphRetired(id, retired)),
            _ => false,
        };
        drop(state);
        drop(graph);
        drop(_wake_scope);
        if start { self.drain_lifecycle(); }
        Ok(())
    }
}
struct GraphOperationGuard {
    peer: Arc<PeerSource>,
    scope: GraphOperationScope,
    settlement: GraphOperationSettlement,
}
struct CallbackGuard<'a>(&'a PeerSource);
impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("peer source lock poisoned");
        state.callbacks_inflight -= 1;
    }
}
impl Drop for GraphOperationGuard {
    fn drop(&mut self) {
        let mut state = self.peer.state.lock().expect("peer source lock poisoned");
        state.graph_operations -= 1;
        if self.settlement == GraphOperationSettlement::Indeterminate {
            match &self.scope {
                GraphOperationScope::Observation => {},
                GraphOperationScope::Effect(Some(owner)) | GraphOperationScope::SingleReplyEffect(Some(owner)) if owner.starts_with(':') => {
                    if state.daemon.as_ref().is_none_or(|current| current == owner) {
                        if state.uncertain_owners.len() < 256 { state.uncertain_owners.insert(owner.clone()); }
                        else { state.uncertain_unknown_owner = true; }
                    }
                }
                GraphOperationScope::Effect(_) | GraphOperationScope::SingleReplyEffect(_) => state.uncertain_unknown_owner = true,
            }
        }
        let retain = state.uncertain_unknown_owner || !state.uncertain_owners.is_empty();
        drop(state);
        if retain { self.peer.retain_obligation(); }
    }
}
impl Drop for PeerSource {
    fn drop(&mut self) {
        self.close_current();
    }
}
struct WatchGuard {
    peer: Arc<PeerSource>,
    watch_id: u64,
    armed: bool,
}
impl Drop for WatchGuard {
    fn drop(&mut self) {
        if self.armed {
            self.peer.invalidate(self.watch_id);
        }
    }
}

#[path = "peer_source_peripheral.rs"]
mod peripheral_source;

#[cfg(test)]
#[path = "peer_source_private_bus_tests.rs"]
mod private_bus_tests;
#[cfg(test)]
#[path = "peer_source_tests.rs"]
mod tests;
