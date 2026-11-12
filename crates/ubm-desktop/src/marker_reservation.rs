// crates/ubm-desktop/src/marker_reservation.rs
//! Session-owned, bounded reservations for ordered marker/write operations.
use crate::boundary::{OrderedMarkerWritePhase, OrderedMarkerWriteStatus, RadioBoundary};
use crate::central::DesktopCentral;
use crate::errors::DesktopError;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::{Mutex, Notify};
use ubm_core::central::PathSelector;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

fn error(code: BleErrorCode) -> DesktopError {
    DesktopError::new(
        code,
        BleErrorDomain::Gatt,
        "gatt.ordered.marker-reservation",
    )
}

fn status_rank(status: &OrderedMarkerWriteStatus) -> u8 {
    match status {
        OrderedMarkerWriteStatus::Pending => 0,
        OrderedMarkerWriteStatus::Admitted {
            write: OrderedMarkerWritePhase::Initiating,
            ..
        } => 1,
        OrderedMarkerWriteStatus::Admitted {
            write: OrderedMarkerWritePhase::Started(_),
            ..
        } => 2,
        OrderedMarkerWriteStatus::NotAdmitted(_)
        | OrderedMarkerWriteStatus::Rejected(_)
        | OrderedMarkerWriteStatus::Admitted {
            write: OrderedMarkerWritePhase::NotStarted(_) | OrderedMarkerWritePhase::Settled(_),
            ..
        } => 3,
    }
}
fn retain_newer(entry: &mut Entry, status: OrderedMarkerWriteStatus) {
    if status_rank(&status) >= status_rank(&entry.status) {
        entry.status = status;
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Input {
    selector: PathSelector,
    value: Vec<u8>,
    with_response: bool,
}
struct Entry {
    handle: u64,
    reservation_key: String,
    input: Option<Input>,
    marker: Option<u64>,
    status: OrderedMarkerWriteStatus,
    cancelled: bool,
    publishing: bool,
    cancel_worker: bool,
    intent: Arc<AtomicU8>,
}
pub(crate) struct MarkerReservationState {
    next: Option<u64>,
    retired: Option<u64>,
    entry: Option<Entry>,
    pub(crate) gate: Arc<AtomicU8>,
    wake: Arc<Notify>,
}

impl MarkerReservationState {
    pub(crate) fn new() -> Self {
        Self {
            next: Some(0),
            retired: None,
            entry: None,
            gate: Arc::new(AtomicU8::new(0)),
            wake: Arc::new(Notify::new()),
        }
    }
}

/// Retain one reservation for one authenticated group owner. The host must keep
/// this object alive until the group and its retained transaction are retired.
pub struct MarkerReservationOwner<B: RadioBoundary> {
    central: DesktopCentral<B>,
    peer: String,
    collection: u64,
    token: Option<u64>,
    state: Arc<Mutex<MarkerReservationState>>,
}
impl<B: RadioBoundary + 'static> MarkerReservationOwner<B> {
    pub(crate) fn new_with_state(
        central: DesktopCentral<B>,
        peer: String,
        collection: u64,
        token: Option<u64>,
        state: Arc<Mutex<MarkerReservationState>>,
    ) -> Self {
        Self {
            central,
            peer,
            collection,
            token,
            state,
        }
    }
    /// Only one reservation is live; duplicate reserve returns that reservation.
    pub async fn reserve(&self, reservation_key: &str) -> Result<u64, DesktopError> {
        if reservation_key.is_empty() || reservation_key.len() > 128 {
            return Err(error(BleErrorCode::ArgumentInvalid));
        }
        let mut state = self.state.lock().await;
        if let Some(entry) = &state.entry {
            return if entry.reservation_key == reservation_key {
                Ok(entry.handle)
            } else {
                Err(error(BleErrorCode::OwnershipDenied))
            };
        }
        if state
            .gate
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(error(BleErrorCode::LifecycleInvalidState));
        }
        let Some(handle) = state.next else {
            state.gate.store(0, Ordering::Release);
            return Err(error(BleErrorCode::CapabilityLimited));
        };
        state.next = handle.checked_add(1);
        state.entry = Some(Entry {
            handle,
            reservation_key: reservation_key.to_owned(),
            input: None,
            marker: None,
            status: OrderedMarkerWriteStatus::Pending,
            cancelled: false,
            publishing: false,
            cancel_worker: false,
            intent: Arc::new(AtomicU8::new(0)),
        });
        Ok(handle)
    }
    /// Dispatch continues after the requesting future disappears. Repeated starts
    /// must carry exactly the same input and never initiate a second operation.
    pub async fn start(
        &self,
        handle: u64,
        selector: PathSelector,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let identity_bytes = selector
            .service_uuid
            .len()
            .saturating_add(selector.characteristic_uuid.as_ref().map_or(0, String::len))
            .saturating_add(selector.descriptor_uuid.as_ref().map_or(0, String::len));
        if value.len() > crate::boundary::ATT_MAX_ATTRIBUTE_VALUE as usize
            || identity_bytes.saturating_add(value.len())
                > ubm_core::contracts::MAX_OPERATION_BYTES as usize
        {
            return Err(error(BleErrorCode::ArgumentInvalid));
        }
        let input = Input {
            selector,
            value,
            with_response,
        };
        let mut state = self.state.lock().await;
        let entry = state
            .entry
            .as_mut()
            .filter(|e| e.handle == handle)
            .ok_or_else(|| error(BleErrorCode::OwnershipDenied))?;
        if let Some(old) = &entry.input {
            if old != &input {
                return Err(error(BleErrorCode::OwnershipDenied));
            }
            return Ok(entry.status.clone());
        }
        if entry.cancelled {
            return Ok(entry.status.clone());
        }
        entry.input = Some(input.clone());
        let intent = Arc::clone(&entry.intent);
        let state = Arc::clone(&self.state);
        let central = self.central.clone();
        let peer = self.peer.clone();
        let collection = self.collection;
        let token = self.token;
        tokio::spawn(async move {
            let prepared = async {
                let (id, occupied) = central
                    .ordered_marker_next_id(&peer, collection, token)
                    .await?;
                if occupied {
                    return Err(error(BleErrorCode::OwnershipDenied));
                }
                let id = id.ok_or_else(|| error(BleErrorCode::CapabilityLimited))?;
                central
                    .prepare_ordered_marker_request(
                        &peer,
                        collection,
                        token,
                        id,
                        &input.selector,
                        input.value,
                        input.with_response,
                    )
                    .await
            }
            .await;
            let request = match prepared {
                Ok(request) => request,
                Err(cause) => {
                    let mut state = state.lock().await;
                    if let Some(e) = state.entry.as_mut().filter(|e| e.handle == handle) {
                        e.status = OrderedMarkerWriteStatus::NotAdmitted(Some(cause));
                    }
                    state.wake.notify_waiters();
                    return;
                }
            };
            {
                let mut state = state.lock().await;
                let Some(entry) = state.entry.as_mut().filter(|e| e.handle == handle) else {
                    return;
                };
                if entry.cancelled {
                    entry.status = OrderedMarkerWriteStatus::NotAdmitted(None);
                    state.wake.notify_waiters();
                    return;
                }
                entry.marker = Some(request.marker_id);
                entry.publishing = true;
            }
            let request_id = request.marker_id;
            let answer = central
                .start_ordered_marker_write_with_intent(&peer, collection, token, request, intent)
                .await;
            let mut published = true;
            let mut status = match answer {
                Ok(status) => status,
                Err(cause) => match central
                    .ordered_marker_next_id(&peer, collection, token)
                    .await
                {
                    Ok((_, false)) => {
                        published = false;
                        OrderedMarkerWriteStatus::NotAdmitted(Some(cause))
                    }
                    _ => OrderedMarkerWriteStatus::Pending,
                },
            };
            let should_cancel = {
                let state = state.lock().await;
                state
                    .entry
                    .as_ref()
                    .filter(|e| e.handle == handle)
                    .is_some_and(|e| e.cancelled)
            };
            if published && should_cancel {
                if let Ok(current) = central
                    .cancel_ordered_marker_write_by_id(&peer, collection, token, request_id)
                    .await
                {
                    status = current;
                }
            }
            let mut state = state.lock().await;
            if let Some(entry) = state.entry.as_mut().filter(|e| e.handle == handle) {
                retain_newer(entry, status);
                entry.publishing = false;
                if !published {
                    entry.marker = None;
                }
            }
            state.wake.notify_waiters();
        });
        Ok(OrderedMarkerWriteStatus::Pending)
    }
    pub async fn status(&self, handle: u64) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let state = self.state.lock().await;
        let entry = state
            .entry
            .as_ref()
            .filter(|e| e.handle == handle)
            .ok_or_else(|| error(BleErrorCode::OwnershipDenied))?;
        let marker = entry.marker;
        let cached = entry.status.clone();
        let publishing = entry.publishing;
        drop(state);
        if marker.is_none() {
            return Ok(cached);
        }
        let answer = self
            .central
            .ordered_marker_write_status_by_id(
                &self.peer,
                self.collection,
                self.token,
                marker.unwrap(),
            )
            .await;
        let status = match answer {
            Ok(status) => status,
            Err(_) if publishing => return Ok(cached),
            Err(cause) => return Err(cause),
        };
        let mut state = self.state.lock().await;
        if let Some(entry) = state.entry.as_mut().filter(|e| e.handle == handle) {
            retain_newer(entry, status.clone());
            return Ok(entry.status.clone());
        }
        Ok(status)
    }
    #[cfg(test)]
    pub(crate) async fn cancel_latched(&self, handle: u64) -> bool {
        self.state
            .lock()
            .await
            .entry
            .as_ref()
            .filter(|entry| entry.handle == handle)
            .is_some_and(|entry| entry.intent.load(Ordering::Acquire) == 1)
    }
    /// Inspect the retained reservation for authenticated host cleanup without its payload.
    pub async fn active(&self) -> Result<Option<(u64, OrderedMarkerWriteStatus)>, DesktopError> {
        let handle = self
            .state
            .lock()
            .await
            .entry
            .as_ref()
            .map(|entry| entry.handle);
        match handle {
            Some(handle) => self
                .status(handle)
                .await
                .map(|status| Some((handle, status))),
            None => Ok(None),
        }
    }
    pub async fn cancel(&self, handle: u64) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let mut state = self.state.lock().await;
        let entry = state
            .entry
            .as_mut()
            .filter(|e| e.handle == handle)
            .ok_or_else(|| error(BleErrorCode::OwnershipDenied))?;
        entry.cancelled = true;
        let _ = entry
            .intent
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        if entry.input.is_none() {
            entry.status = OrderedMarkerWriteStatus::NotAdmitted(None);
            return Ok(entry.status.clone());
        }
        let marker = entry.marker;
        let publishing = entry.publishing;
        let cached = entry.status.clone();
        let launch = publishing && marker.is_some() && !entry.cancel_worker;
        if launch {
            entry.cancel_worker = true;
        }
        let local_wake = Arc::clone(&state.wake);
        drop(state);
        if launch {
            let central = self.central.clone();
            let peer = self.peer.clone();
            let collection = self.collection;
            let token = self.token;
            let shared = Arc::clone(&self.state);
            let worker_wake = Arc::clone(&local_wake);
            let marker_id = marker.expect("launch requires marker");
            tokio::spawn(async move {
                let publication = match central
                    .ordered_marker_publication_wake(&peer, collection, token)
                    .await
                {
                    Ok(wake) => wake,
                    Err(_) => {
                        let mut state = shared.lock().await;
                        if let Some(entry) = state.entry.as_mut().filter(|e| e.handle == handle) {
                            entry.cancel_worker = false;
                        }
                        state.wake.notify_waiters();
                        return;
                    }
                };
                loop {
                    let published = publication.notified();
                    tokio::pin!(published);
                    published.as_mut().enable();
                    let completed = worker_wake.notified();
                    tokio::pin!(completed);
                    completed.as_mut().enable();
                    let in_flight = {
                        let state = shared.lock().await;
                        state
                            .entry
                            .as_ref()
                            .filter(|e| e.handle == handle)
                            .is_some_and(|e| e.publishing)
                    };
                    match central
                        .ordered_marker_next_id(&peer, collection, token)
                        .await
                    {
                        Ok((Some(id), true)) if id == marker_id => {
                            if let Ok(status) = central
                                .cancel_ordered_marker_write_by_id(
                                    &peer, collection, token, marker_id,
                                )
                                .await
                            {
                                let mut state = shared.lock().await;
                                if let Some(entry) =
                                    state.entry.as_mut().filter(|e| e.handle == handle)
                                {
                                    retain_newer(entry, status);
                                }
                            }
                            break;
                        }
                        _ if !in_flight => break,
                        Err(_) => break,
                        _ => tokio::select! { _ = &mut published => {}, _ = &mut completed => {} },
                    }
                }
                let mut state = shared.lock().await;
                if let Some(entry) = state.entry.as_mut().filter(|e| e.handle == handle) {
                    entry.cancel_worker = false;
                }
                state.wake.notify_waiters();
            });
        }
        if publishing {
            loop {
                let notified = local_wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let state = self.state.lock().await;
                let entry = state
                    .entry
                    .as_ref()
                    .filter(|e| e.handle == handle)
                    .ok_or_else(|| error(BleErrorCode::OwnershipDenied))?;
                if !entry.cancel_worker {
                    return Ok(entry.status.clone());
                }
                drop(state);
                notified.await;
            }
        }
        if marker.is_none() {
            return Ok(cached);
        }
        let answer = self
            .central
            .cancel_ordered_marker_write_by_id(
                &self.peer,
                self.collection,
                self.token,
                marker.unwrap(),
            )
            .await;
        let status = match answer {
            Ok(status) => status,
            Err(_) if publishing => return Ok(cached),
            Err(cause) => return Err(cause),
        };
        let mut state = self.state.lock().await;
        if let Some(entry) = state.entry.as_mut().filter(|e| e.handle == handle) {
            retain_newer(entry, status.clone());
            return Ok(entry.status.clone());
        }
        Ok(status)
    }
    pub async fn forget(&self, handle: u64) -> Result<(), DesktopError> {
        let mut state = self.state.lock().await;
        if state.retired.is_some_and(|retired| handle <= retired) {
            return Ok(());
        }
        let entry = state
            .entry
            .as_ref()
            .filter(|e| e.handle == handle)
            .ok_or_else(|| error(BleErrorCode::OwnershipDenied))?;
        if matches!(entry.status, OrderedMarkerWriteStatus::Pending) {
            return Err(error(BleErrorCode::LifecycleInvalidState));
        }
        if let Some(marker) = entry.marker {
            self.central
                .forget_ordered_marker_write_by_id(&self.peer, self.collection, self.token, marker)
                .await?;
        }
        state.entry = None;
        state.retired = Some(handle);
        state.gate.store(0, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::FakeRadio;

    #[tokio::test]
    async fn idle_cancel_and_checked_retirement_do_not_touch_central() {
        let central = DesktopCentral::open(FakeRadio::new(), "marker-reservation-test")
            .await
            .unwrap();
        let shared = Arc::new(Mutex::new(MarkerReservationState::new()));
        let owner = MarkerReservationOwner::new_with_state(
            central,
            "absent".into(),
            4,
            None,
            Arc::clone(&shared),
        );
        let first = owner.reserve("key").await.unwrap();
        assert_eq!(owner.reserve("key").await.unwrap(), first);
        assert!(owner.reserve("other-key").await.is_err());
        assert_eq!(
            owner.cancel(first).await.unwrap(),
            OrderedMarkerWriteStatus::NotAdmitted(None)
        );
        owner.forget(first).await.unwrap();
        owner.forget(first).await.unwrap();
        let second = owner.reserve("key").await.unwrap();
        assert_eq!(second, first + 1);
        assert!(owner.cancel(first).await.is_err());
        assert_eq!(shared.lock().await.gate.load(Ordering::Acquire), 1);
        owner.cancel(second).await.unwrap();
        owner.forget(second).await.unwrap();
        assert_eq!(shared.lock().await.gate.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn absent_group_preflight_is_retained_without_marker_id() {
        let central = DesktopCentral::open(FakeRadio::new(), "marker-preflight-test")
            .await
            .unwrap();
        let shared = Arc::new(Mutex::new(MarkerReservationState::new()));
        let owner = MarkerReservationOwner::new_with_state(
            central,
            "absent".into(),
            8,
            None,
            Arc::clone(&shared),
        );
        let handle = owner.reserve("key").await.unwrap();
        let selector = PathSelector {
            service_uuid: "service".into(),
            service_occurrence: Some(0),
            characteristic_uuid: Some("characteristic".into()),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        };
        assert!(
            owner
                .start(
                    handle,
                    selector.clone(),
                    vec![0; crate::boundary::ATT_MAX_ATTRIBUTE_VALUE as usize + 1],
                    true
                )
                .await
                .is_err()
        );
        assert!(shared.lock().await.entry.as_ref().unwrap().input.is_none());
        assert_eq!(
            owner
                .start(handle, selector.clone(), vec![1], true)
                .await
                .unwrap(),
            OrderedMarkerWriteStatus::Pending
        );
        let observed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = owner.status(handle).await.unwrap();
                if !matches!(status, OrderedMarkerWriteStatus::Pending) {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            observed,
            OrderedMarkerWriteStatus::NotAdmitted(Some(_))
        ));
        assert_eq!(
            owner
                .start(handle, selector.clone(), vec![1], true)
                .await
                .unwrap(),
            observed
        );
        assert!(owner.start(handle, selector, vec![2], true).await.is_err());
        owner.forget(handle).await.unwrap();
    }
    #[tokio::test]
    async fn exhaustion_and_group_closing_do_not_allocate() {
        let central = DesktopCentral::open(FakeRadio::new(), "marker-limit-test")
            .await
            .unwrap();
        let shared = Arc::new(Mutex::new(MarkerReservationState::new()));
        let owner = MarkerReservationOwner::new_with_state(
            central,
            "absent".into(),
            9,
            None,
            Arc::clone(&shared),
        );
        shared.lock().await.gate.store(2, Ordering::Release);
        assert!(owner.reserve("blocked").await.is_err());
        shared.lock().await.gate.store(0, Ordering::Release);
        shared.lock().await.next = Some(u64::MAX);
        let last = owner.reserve("last").await.unwrap();
        assert_eq!(last, u64::MAX);
        owner.cancel(last).await.unwrap();
        owner.forget(last).await.unwrap();
        assert!(owner.reserve("exhausted").await.is_err());
        assert_eq!(shared.lock().await.gate.load(Ordering::Acquire), 0);
    }
}
