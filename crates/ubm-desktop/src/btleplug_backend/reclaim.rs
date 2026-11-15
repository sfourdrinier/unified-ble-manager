// crates/ubm-desktop/src/btleplug_backend/reclaim.rs
//! Exact source-owned recorder reclaim receipts.
use crate::boundary::{
    OrderedReclaimGraphAcquire, OrderedReclaimGraphCommit, OrderedReclaimGraphKey,
    OrderedReclaimPermitRequest,
};
use crate::errors::DesktopError;
use btleplug::platform::Peripheral;
use btleplug::ubm_reclaim_authority::{
    AuthorityError, GraphKey, GraphTarget, OpenStatus, PermitStatus, PermitTokenStatus,
    ReclaimAuthorityHandle,
};
use btleplug::ubm_reclaim_graph_binding::GraphBinding;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

pub(super) trait ReclaimPeer: Clone + Send + Sync {
    fn acquire_reclaim(
        &self,
        binding: &GraphBinding,
        key: GraphKey,
    ) -> Result<PermitStatus, AuthorityError>;
    fn commit_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError>;
    fn release_reclaim(
        &self,
        key: &GraphKey,
    ) -> impl Future<Output = Result<PermitStatus, AuthorityError>> + Send;
    fn forget_reclaim(&self, key: &GraphKey) -> Result<(), AuthorityError>;
}
impl ReclaimPeer for Peripheral {
    fn acquire_reclaim(
        &self,
        binding: &GraphBinding,
        key: GraphKey,
    ) -> Result<PermitStatus, AuthorityError> {
        Peripheral::acquire_reclaim(self, binding, key)
    }
    fn commit_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        Peripheral::commit_reclaim(self, key)
    }
    async fn release_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            Peripheral::release_reclaim(self, key)
        }
        #[cfg(target_os = "macos")]
        {
            Peripheral::release_reclaim(self, key).await
        }
    }
    fn forget_reclaim(&self, key: &GraphKey) -> Result<(), AuthorityError> {
        Peripheral::forget_reclaim(self, key)
    }
}

struct Open {
    source: ReclaimAuthorityHandle,
    receipt: u64,
    authority: Option<u64>,
    forgotten: bool,
}
struct Permit<P> {
    request: OrderedReclaimPermitRequest,
    source: ReclaimAuthorityHandle,
    peripheral: P,
    binding: GraphBinding,
    id: Option<u64>,
    forgotten: bool,
    release_gate: Arc<tokio::sync::Mutex<()>>,
}
pub(super) struct ReclaimState<P = Peripheral> {
    opens: HashMap<u64, Open>,
    refused_opens: HashSet<u64>,
    permits: HashMap<(u64, u64), Permit<P>>,
    refused: HashMap<(u64, u64), OrderedReclaimPermitRequest>,
    #[cfg(test)]
    lose_ack: bool,
}
impl<P> Default for ReclaimState<P> {
    fn default() -> Self {
        Self {
            opens: HashMap::new(),
            refused_opens: HashSet::new(),
            permits: HashMap::new(),
            refused: HashMap::new(),
            #[cfg(test)]
            lose_ack: false,
        }
    }
}
fn error(e: AuthorityError) -> DesktopError {
    DesktopError::subscribe_failed(format!("native reclaim authority: {e:?}"))
}
fn target(r: &OrderedReclaimPermitRequest) -> GraphTarget {
    GraphTarget {
        peer_id: r.peer_id.clone(),
        connection_generation: r.connection_generation.clone(),
        graph: (
            r.identity.owner.clone(),
            r.identity.attachment,
            r.identity.revision,
        ),
    }
}
fn native(k: &OrderedReclaimGraphKey) -> GraphKey {
    GraphKey {
        authority_id: k.authority_id,
        peer_id: k.peer_id.clone(),
        connection_generation: k.connection_generation.clone(),
        graph: (
            k.identity.owner.clone(),
            k.identity.attachment,
            k.identity.revision,
        ),
        permit_id: k.permit_id,
    }
}
fn no_effect() -> DesktopError {
    DesktopError::subscribe_failed("native reclaim request was not dispatched").with_outcome(
        Some(ubm_core::contracts::CommitState::NotDispatched),
        crate::Retryability::CallerDecides,
    )
}
// An acknowledged forget is retained separately from the acknowledgement so
// a lost response cannot send an unrelated operation to a replacement source.
fn retire<E>(
    forgotten: &mut bool,
    forget: impl FnOnce() -> Result<(), E>,
    ack: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    if !*forgotten {
        forget()?;
        *forgotten = true;
    }
    ack()
}
fn mismatch() -> DesktopError {
    DesktopError::subscribe_failed("unknown or conflicting native reclaim receipt")
}
impl<P: ReclaimPeer> ReclaimState<P> {
    pub fn open(
        &mut self,
        request: u64,
        source: ReclaimAuthorityHandle,
    ) -> Result<u64, DesktopError> {
        if self.refused_opens.contains(&request) {
            return Err(no_effect());
        }
        if !self.opens.contains_key(&request) {
            if self.opens.len() >= 64 || self.refused_opens.len() >= 64 {
                return Err(error(AuthorityError::Capacity));
            }
            let receipt = match source.reserve_open(request) {
                Ok(receipt) => receipt,
                Err(AuthorityError::Capacity) => {
                    self.refused_opens.insert(request);
                    return Err(no_effect());
                }
                Err(cause) => return Err(error(cause)),
            };
            self.opens.insert(
                request,
                Open {
                    source,
                    receipt: receipt.receipt_id,
                    authority: None,
                    forgotten: false,
                },
            );
        }
        let entry = self.opens.get_mut(&request).ok_or_else(mismatch)?;
        match entry
            .source
            .resolve_open(request, entry.receipt, true)
            .map_err(error)?
        {
            OpenStatus::Opened(id) => {
                entry.authority = Some(id);
                Ok(id)
            }
            OpenStatus::NotCreated => Err(no_effect()),
            _ => Err(mismatch()),
        }
    }
    pub fn forget_open(&mut self, request: u64) -> Result<(), DesktopError> {
        if self.refused_opens.remove(&request) {
            return Ok(());
        }
        let e = self.opens.get_mut(&request).ok_or_else(mismatch)?;
        retire(
            &mut e.forgotten,
            || e.source.forget_not_created(request, e.receipt),
            || e.source.ack_open_retirement(request, e.receipt).map(|_| ()),
        )
        .map_err(error)?;
        self.opens.remove(&request);
        Ok(())
    }
    fn open_mut(&mut self, authority: u64) -> Result<(u64, &mut Open), DesktopError> {
        self.opens
            .iter_mut()
            .find_map(|(request, entry)| {
                (entry.authority == Some(authority)).then_some((*request, entry))
            })
            .ok_or_else(mismatch)
    }
    pub fn close(&mut self, authority: u64) -> Result<(), DesktopError> {
        let (request, e) = self.open_mut(authority)?;
        e.source.close(request, authority).map_err(error)
    }
    pub fn forget_authority(&mut self, authority: u64) -> Result<(), DesktopError> {
        let (request, e) = self.open_mut(authority)?;
        retire(
            &mut e.forgotten,
            || e.source.forget_closed(request, authority),
            || e.source.ack_open_retirement(request, e.receipt).map(|_| ()),
        )
        .map_err(error)?;
        self.opens.remove(&request);
        Ok(())
    }
    pub fn refuse_request(
        &mut self,
        request: &OrderedReclaimPermitRequest,
    ) -> Result<u64, DesktopError> {
        let index = (request.authority_id, request.request_id);
        if let Some(existing) = self.refused.get(&index) {
            return if existing == request {
                Err(no_effect())
            } else {
                Err(mismatch())
            };
        }
        if self.refused.len() >= 256 {
            return Err(error(AuthorityError::Capacity));
        }
        self.refused.insert(index, request.clone());
        Err(no_effect())
    }
    pub fn has_request(&self, request: &OrderedReclaimPermitRequest) -> bool {
        self.permits
            .contains_key(&(request.authority_id, request.request_id))
            || self
                .refused
                .contains_key(&(request.authority_id, request.request_id))
    }
    pub fn reserve(
        &mut self,
        request: &OrderedReclaimPermitRequest,
        captured: Option<(P, GraphBinding)>,
    ) -> Result<u64, DesktopError> {
        let index = (request.authority_id, request.request_id);
        if let Some(existing) = self.refused.get(&index) {
            return if existing == request {
                Err(no_effect())
            } else {
                Err(mismatch())
            };
        }
        if !self.permits.contains_key(&index) {
            if self.permits.len() >= 256 {
                return self.refuse_request(request);
            }
            let (peripheral, binding) = captured.ok_or_else(mismatch)?;
            let (_, open) = self.open_mut(request.authority_id)?;
            let source = open.source.clone();
            match source.reserve_permit_token_bound(
                request.authority_id,
                request.request_id,
                target(request),
                &binding,
            ) {
                Ok(_) => {}
                Err(AuthorityError::Capacity) => return self.refuse_request(request),
                Err(cause) => return Err(error(cause)),
            }
            self.permits.insert(
                index,
                Permit {
                    request: request.clone(),
                    source,
                    peripheral,
                    binding,
                    id: None,
                    forgotten: false,
                    release_gate: Arc::new(tokio::sync::Mutex::new(())),
                },
            );
        }
        let e = self.permits.get_mut(&index).ok_or_else(mismatch)?;
        if e.request != *request {
            return Err(mismatch());
        }
        match e
            .source
            .resolve_permit_token_bound(
                request.authority_id,
                request.request_id,
                &target(request),
                &e.binding,
                true,
            )
            .map_err(error)?
        {
            PermitTokenStatus::Issued(id) => {
                e.id = Some(id);
                Ok(id)
            }
            PermitTokenStatus::NotCreated => Err(no_effect()),
            _ => Err(mismatch()),
        }
    }
    pub fn forget_request(
        &mut self,
        request: &OrderedReclaimPermitRequest,
    ) -> Result<(), DesktopError> {
        let index = (request.authority_id, request.request_id);
        if let Some(existing) = self.refused.get(&index) {
            if existing != request {
                return Err(mismatch());
            }
            self.refused.remove(&index);
            return Ok(());
        }
        let e = self.permits.get_mut(&index).ok_or_else(mismatch)?;
        if e.request != *request {
            return Err(mismatch());
        }
        let t = target(request);
        if !e.forgotten {
            e.source
                .forget_not_created_permit_token_bound(
                    request.authority_id,
                    request.request_id,
                    &t,
                    &e.binding,
                )
                .map_err(error)?;
            e.forgotten = true;
        }
        match e.source.ack_not_created_permit_token_bound(
            request.authority_id,
            request.request_id,
            &t,
            &e.binding,
        ) {
            Ok(_) | Err(AuthorityError::Unknown) if e.forgotten => {}
            Err(err) => return Err(error(err)),
            Ok(_) => {}
        }
        self.permits.remove(&index);
        Ok(())
    }
    fn permit(&mut self, k: &OrderedReclaimGraphKey) -> Result<&mut Permit<P>, DesktopError> {
        self.permits
            .values_mut()
            .find(|e| {
                e.id == Some(k.permit_id)
                    && e.request.authority_id == k.authority_id
                    && e.request.peer_id == k.peer_id
                    && e.request.connection_generation == k.connection_generation
                    && e.request.identity == k.identity
            })
            .ok_or_else(mismatch)
    }
    pub fn acquire(
        &mut self,
        k: &OrderedReclaimGraphKey,
    ) -> Result<OrderedReclaimGraphAcquire, DesktopError> {
        let e = self.permit(k)?;
        match e
            .peripheral
            .acquire_reclaim(&e.binding, native(k))
            .map_err(error)?
        {
            PermitStatus::Acquired => Ok(OrderedReclaimGraphAcquire::Acquired),
            PermitStatus::Busy => Ok(OrderedReclaimGraphAcquire::Busy),
            PermitStatus::Stale => Ok(OrderedReclaimGraphAcquire::Stale),
            _ => Err(mismatch()),
        }
    }
    pub fn commit(
        &mut self,
        k: &OrderedReclaimGraphKey,
    ) -> Result<OrderedReclaimGraphCommit, DesktopError> {
        let e = self.permit(k)?;
        match e.peripheral.commit_reclaim(&native(k)).map_err(error)? {
            PermitStatus::Committed => Ok(OrderedReclaimGraphCommit::Committed),
            PermitStatus::Invalidated => Ok(OrderedReclaimGraphCommit::Invalidated(mismatch())),
            _ => Err(mismatch()),
        }
    }
    pub async fn release_retained(
        state: &Mutex<Self>,
        k: &OrderedReclaimGraphKey,
    ) -> Result<(), DesktopError> {
        let gate = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.permit(k)?.release_gate.clone()
        };
        let _guard = gate.lock().await;
        // Recheck after waiting: another completed owner may have retired this key.
        let peer = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.permit(k)?.peripheral.clone()
        };
        match peer.release_reclaim(&native(k)).await.map_err(error)? {
            PermitStatus::Released => Ok(()),
            _ => Err(mismatch()),
        }
    }
    pub fn forget(&mut self, k: &OrderedReclaimGraphKey) -> Result<(), DesktopError> {
        let e = self.permit(k)?;
        retire(
            &mut e.forgotten,
            || e.peripheral.forget_reclaim(&native(k)),
            || {
                e.source
                    .ack_permit_retirement(&native(k), &e.binding)
                    .map(|_| ())
            },
        )
        .map_err(error)?;
        let index = (e.request.authority_id, e.request.request_id);
        #[cfg(test)]
        if std::mem::take(&mut self.lose_ack) {
            return Err(error(AuthorityError::Pending));
        }
        self.permits.remove(&index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::retire;
    use std::cell::Cell;
    #[test]
    fn retirement_retries_forget_and_ack_without_repeating_confirmed_forget() {
        let mut forgotten = false;
        let effects = Cell::new(0);
        let result = retire(
            &mut forgotten,
            || {
                effects.set(effects.get() + 1);
                Err("lost forget response")
            },
            || Ok(()),
        );
        assert_eq!(result, Err("lost forget response"));
        let result = retire(
            &mut forgotten,
            || {
                effects.set(effects.get() + 1);
                Ok(())
            },
            || Err("lost ack response"),
        );
        assert_eq!(result, Err("lost ack response"));
        let result = retire(
            &mut forgotten,
            || panic!("confirmed forget must not repeat"),
            || Ok::<(), &str>(()),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(effects.get(), 2);
    }
}

#[cfg(all(test, target_os = "linux", feature = "reclaim-test-support"))]
mod source_tests {
    use super::*;
    use crate::boundary::GattSnapshotIdentity;
    use btleplug::platform::ReclaimSourceFixture;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[derive(Clone)]
    struct Peer {
        fixture: Arc<ReclaimSourceFixture>,
        lose_forget: Arc<AtomicBool>,
    }
    impl ReclaimPeer for Peer {
        fn acquire_reclaim(
            &self,
            binding: &GraphBinding,
            key: GraphKey,
        ) -> Result<PermitStatus, AuthorityError> {
            self.fixture.acquire_reclaim(binding, key)
        }
        fn commit_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
            self.fixture.commit_reclaim(key)
        }
        async fn release_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
            self.fixture.release_reclaim(key)
        }
        fn forget_reclaim(&self, key: &GraphKey) -> Result<(), AuthorityError> {
            self.fixture.forget_reclaim(key)?;
            if self.lose_forget.swap(false, Ordering::SeqCst) {
                Err(AuthorityError::Pending)
            } else {
                Ok(())
            }
        }
    }
    #[tokio::test]
    async fn actual_source_replays_exact_retirement_after_lost_replies() {
        let fixture = Arc::new(ReclaimSourceFixture::new(
            "peer",
            "connection",
            ("owner".into(), 1, 2),
        ));
        let peer = Peer {
            fixture: fixture.clone(),
            lose_forget: Arc::new(AtomicBool::new(true)),
        };
        let mut state = ReclaimState::<Peer>::default();
        let authority = state.open(1, fixture.authority_handle()).unwrap();
        let request = OrderedReclaimPermitRequest {
            authority_id: authority,
            request_id: 1,
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            identity: GattSnapshotIdentity {
                owner: "owner".into(),
                attachment: 1,
                revision: 2,
            },
        };
        let permit = state
            .reserve(&request, Some((peer.clone(), fixture.binding())))
            .unwrap();
        assert_eq!(state.reserve(&request, None).unwrap(), permit);
        let mut key = OrderedReclaimGraphKey {
            authority_id: authority,
            peer_id: request.peer_id.clone(),
            connection_generation: request.connection_generation.clone(),
            identity: request.identity.clone(),
            permit_id: permit,
        };
        key.peer_id = "foreign".into();
        assert!(state.acquire(&key).is_err());
        key.peer_id = request.peer_id.clone();
        let foreign = ReclaimSourceFixture::new("peer", "connection", ("owner".into(), 1, 2));
        assert!(
            foreign
                .acquire_reclaim(&fixture.binding(), native(&key))
                .is_err()
        );
        assert!(matches!(
            state.acquire(&key).unwrap(),
            OrderedReclaimGraphAcquire::Acquired
        ));
        assert!(matches!(
            state.commit(&key).unwrap(),
            OrderedReclaimGraphCommit::Committed
        ));
        let state = Mutex::new(state);
        ReclaimState::release_retained(&state, &key).await.unwrap();
        let mut state = state.into_inner().unwrap();
        assert!(state.forget(&key).is_err());
        state.lose_ack = true;
        assert!(state.forget(&key).is_err());
        state.forget(&key).unwrap();
        state.close(authority).unwrap();
        state.forget_authority(authority).unwrap();
    }
    #[derive(Clone)]
    struct HeldPeer {
        fixture: Arc<ReclaimSourceFixture>,
        started: Arc<tokio::sync::Notify>,
        proceed: Arc<tokio::sync::Notify>,
        retry_started: Arc<tokio::sync::Notify>,
        submitted: Arc<AtomicBool>,
        result: tokio::sync::watch::Sender<Option<Result<PermitStatus, AuthorityError>>>,
    }
    impl ReclaimPeer for HeldPeer {
        fn acquire_reclaim(
            &self,
            binding: &GraphBinding,
            key: GraphKey,
        ) -> Result<PermitStatus, AuthorityError> {
            self.fixture.acquire_reclaim(binding, key)
        }
        fn commit_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
            self.fixture.commit_reclaim(key)
        }
        async fn release_reclaim(&self, key: &GraphKey) -> Result<PermitStatus, AuthorityError> {
            if !self.submitted.swap(true, Ordering::SeqCst) {
                let owner = self.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    owner.started.notify_one();
                    owner.proceed.notified().await;
                    owner
                        .result
                        .send_replace(Some(owner.fixture.release_reclaim(&key)));
                });
            } else {
                self.retry_started.notify_one();
            }
            let mut result = self.result.subscribe();
            loop {
                if let Some(value) = *result.borrow_and_update() {
                    return value;
                }
                result
                    .changed()
                    .await
                    .map_err(|_| AuthorityError::Unknown)?;
            }
        }
        fn forget_reclaim(&self, key: &GraphKey) -> Result<(), AuthorityError> {
            self.fixture.forget_reclaim(key)
        }
    }
    #[tokio::test]
    async fn held_release_survives_dropped_waiter_and_replays_exact_source() {
        let fixture = Arc::new(ReclaimSourceFixture::new(
            "peer",
            "connection",
            ("owner".into(), 1, 2),
        ));
        let (result, _) = tokio::sync::watch::channel(None);
        let peer = HeldPeer {
            fixture: fixture.clone(),
            started: Arc::new(tokio::sync::Notify::new()),
            proceed: Arc::new(tokio::sync::Notify::new()),
            retry_started: Arc::new(tokio::sync::Notify::new()),
            submitted: Arc::new(AtomicBool::new(false)),
            result,
        };
        let mut state = ReclaimState::<HeldPeer>::default();
        let authority = state.open(1, fixture.authority_handle()).unwrap();
        let request = OrderedReclaimPermitRequest {
            authority_id: authority,
            request_id: 1,
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            identity: GattSnapshotIdentity {
                owner: "owner".into(),
                attachment: 1,
                revision: 2,
            },
        };
        let permit = state
            .reserve(&request, Some((peer.clone(), fixture.binding())))
            .unwrap();
        let key = OrderedReclaimGraphKey {
            authority_id: authority,
            peer_id: request.peer_id,
            connection_generation: request.connection_generation,
            identity: request.identity,
            permit_id: permit,
        };
        assert!(matches!(
            state.acquire(&key).unwrap(),
            OrderedReclaimGraphAcquire::Acquired
        ));
        let state = Arc::new(Mutex::new(state));
        let task_state = state.clone();
        let task_key = key.clone();
        let first =
            tokio::spawn(
                async move { ReclaimState::release_retained(&task_state, &task_key).await },
            );
        peer.started.notified().await;
        assert!(
            state.try_lock().is_ok(),
            "state mutex must not be held during native release"
        );
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let task_state = state.clone();
        let task_key = key.clone();
        let retry =
            tokio::spawn(
                async move { ReclaimState::release_retained(&task_state, &task_key).await },
            );
        peer.retry_started.notified().await;
        assert!(
            !retry.is_finished(),
            "release must remain pending until the source drains"
        );
        peer.proceed.notify_one();
        retry.await.unwrap().unwrap();
        let mut state = state.lock().unwrap();
        state.forget(&key).unwrap();
        state.close(authority).unwrap();
        state.forget_authority(authority).unwrap();
    }
    #[test]
    fn issued_unacquired_receipt_is_retired_without_pinning_graph() {
        let fixture = Arc::new(ReclaimSourceFixture::new(
            "peer",
            "connection",
            ("owner".into(), 1, 2),
        ));
        let peer = Peer {
            fixture: fixture.clone(),
            lose_forget: Arc::new(AtomicBool::new(false)),
        };
        let mut state = ReclaimState::<Peer>::default();
        let authority = state.open(1, fixture.authority_handle()).unwrap();
        let request = OrderedReclaimPermitRequest {
            authority_id: authority,
            request_id: 1,
            peer_id: "peer".into(),
            connection_generation: "connection".into(),
            identity: GattSnapshotIdentity {
                owner: "owner".into(),
                attachment: 1,
                revision: 2,
            },
        };
        let permit = state
            .reserve(&request, Some((peer, fixture.binding())))
            .unwrap();
        let key = OrderedReclaimGraphKey {
            authority_id: authority,
            peer_id: request.peer_id,
            connection_generation: request.connection_generation,
            identity: request.identity,
            permit_id: permit,
        };
        state.forget(&key).unwrap();
        state.close(authority).unwrap();
        state.forget_authority(authority).unwrap();
    }
}
