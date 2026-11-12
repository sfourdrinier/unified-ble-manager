//! Synchronous native admission, shared by every caller of one central.
//! A slot is owned until its operation finishes or is abandoned. Async workers
//! cannot reorder slots by starting in a different executor turn.

use crate::DesktopError;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

#[derive(Debug)]
struct State {
    next: u64,
    slots: usize,
    sealed: bool,
    peers: HashMap<String, VecDeque<u64>>,
    invalidated: HashMap<u64, BleErrorCode>,
    blocked: HashMap<String, (usize, BleErrorCode)>,
    global_block: Option<(usize, BleErrorCode)>,
    provisional: HashMap<String, usize>,
    starting: HashSet<u64>,
    dispatched: HashSet<u64>,
}

#[derive(Debug)]
pub(crate) struct GattAdmissionQueue {
    state: Mutex<State>,
    wake: Notify,
    maximum: usize,
}

/// A native-owned queue position. It is deliberately not cloneable.
#[derive(Debug)]
pub struct GattAdmission {
    queue: Arc<GattAdmissionQueue>,
    peer: String,
    id: u64,
}

/// Keeps a peer closed to new GATT admissions until its retirement completes.
pub(crate) struct GattRetirementGate {
    queue: Arc<GattAdmissionQueue>,
    peer: String,
}

/// Keeps all GATT admissions closed during an adapter-wide retirement.
pub(crate) struct GattGlobalRetirementGate {
    queue: Arc<GattAdmissionQueue>,
}

pub(crate) struct GattProvisionalGate {
    queue: Arc<GattAdmissionQueue>,
    peer: String,
}

pub(crate) struct StartingPoll {
    queue: Arc<GattAdmissionQueue>,
    id: u64,
    owns: bool,
}
impl Drop for StartingPoll {
    fn drop(&mut self) {
        if !self.owns {
            return;
        }
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.starting.remove(&self.id);
        drop(state);
        self.queue.wake.notify_waiters();
    }
}

fn failure(code: BleErrorCode) -> DesktopError {
    let domain = if code == BleErrorCode::CapabilityLimited {
        BleErrorDomain::Capability
    } else {
        BleErrorDomain::Core
    };
    DesktopError::new(code, domain, "gatt.admission")
}

impl GattAdmissionQueue {
    pub(crate) fn new(maximum: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                next: 0,
                slots: 0,
                sealed: false,
                peers: HashMap::new(),
                invalidated: HashMap::new(),
                blocked: HashMap::new(),
                global_block: None,
                provisional: HashMap::new(),
                starting: HashSet::new(),
                dispatched: HashSet::new(),
            }),
            wake: Notify::new(),
            maximum,
        })
    }

    #[cfg(test)]
    pub(crate) fn queued_for_peer(&self, peer: &str) -> usize {
        self.state
            .lock()
            .expect("GATT admission state")
            .peers
            .get(peer)
            .map_or(0, VecDeque::len)
    }

    pub(crate) fn reserve(self: &Arc<Self>, peer: &str) -> Result<GattAdmission, DesktopError> {
        if peer.is_empty() {
            return Err(failure(BleErrorCode::ArgumentInvalid));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure(BleErrorCode::LifecycleInvalidState))?;
        if state.sealed {
            return Err(failure(BleErrorCode::LifecycleDestroyed));
        }
        if let Some((_, code)) = state.global_block {
            return Err(failure(code));
        }
        if let Some((_, code)) = state.blocked.get(peer) {
            return Err(failure(*code));
        }
        if state.slots >= self.maximum {
            return Err(failure(BleErrorCode::CapabilityLimited)
                .with_detail("the native GATT admission queue is full"));
        }
        let next = state
            .next
            .checked_add(1)
            .ok_or_else(|| failure(BleErrorCode::CapabilityLimited))?;
        let id = state.next;
        state.next = next;
        state.slots += 1;
        state
            .peers
            .entry(peer.to_owned())
            .or_default()
            .push_back(id);
        Ok(GattAdmission {
            queue: Arc::clone(self),
            peer: peer.to_owned(),
            id,
        })
    }

    pub(crate) fn invalidate_peer(&self, peer: &str, code: BleErrorCode) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            eprintln!("native GATT invalidation state poisoned: {error}");
            error.into_inner()
        });
        let ids: Vec<u64> = state
            .peers
            .get(peer)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for id in ids {
            state.invalidated.entry(id).or_insert(code);
        }
        drop(state);
        self.wake.notify_waiters();
    }

    /// Atomically invalidate outstanding admissions and reject new ones.
    /// Overlapping retirements retain independent gates until each finishes.
    pub(crate) fn block_peer(
        self: &Arc<Self>,
        peer: &str,
        code: BleErrorCode,
    ) -> GattRetirementGate {
        self.block_peer_except_id(peer, code, None)
    }

    /// A rediscovery retains its own admission while fencing other work.
    pub(crate) fn block_peer_except(
        self: &Arc<Self>,
        current: &GattAdmission,
        code: BleErrorCode,
    ) -> Result<GattRetirementGate, DesktopError> {
        if !Arc::ptr_eq(self, &current.queue) {
            return Err(failure(BleErrorCode::OwnershipDenied));
        }
        Ok(self.block_peer_except_id(&current.peer, code, Some(current.id)))
    }

    fn block_peer_except_id(
        self: &Arc<Self>,
        peer: &str,
        code: BleErrorCode,
        except: Option<u64>,
    ) -> GattRetirementGate {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let ids: Vec<u64> = state
            .peers
            .get(peer)
            .into_iter()
            .flatten()
            .copied()
            .filter(|id| Some(*id) != except)
            .collect();
        for id in ids {
            state.invalidated.entry(id).or_insert(code);
        }
        let entry = state.blocked.entry(peer.to_owned()).or_insert((0, code));
        entry.0 += 1;
        drop(state);
        self.wake.notify_waiters();
        GattRetirementGate {
            queue: Arc::clone(self),
            peer: peer.to_owned(),
        }
    }

    /// Block all peers and invalidate every existing slot in one critical section.
    pub(crate) fn block_all(self: &Arc<Self>, code: BleErrorCode) -> GattGlobalRetirementGate {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let ids: Vec<u64> = state.peers.values().flatten().copied().collect();
        for id in ids {
            state.invalidated.entry(id).or_insert(code);
        }
        let entry = state.global_block.get_or_insert((0, code));
        entry.0 += 1;
        drop(state);
        self.wake.notify_waiters();
        GattGlobalRetirementGate {
            queue: Arc::clone(self),
        }
    }

    /// Suspend admission while a scoped lifecycle event is validated.
    pub(crate) fn provisional_peer(self: &Arc<Self>, peer: &str) -> GattProvisionalGate {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        *state.provisional.entry(peer.to_owned()).or_default() += 1;
        drop(state);
        self.wake.notify_waiters();
        GattProvisionalGate {
            queue: Arc::clone(self),
            peer: peer.to_owned(),
        }
    }

    /// Rediscovery may invalidate earlier waiters, but must not cross a
    /// provisional scoped-event decision that could prove stale.
    pub(crate) async fn invalidate_except_when_open(
        &self,
        current: &GattAdmission,
        code: BleErrorCode,
    ) -> Result<(), DesktopError> {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| failure(BleErrorCode::LifecycleInvalidState))?;
                if state.sealed {
                    return Err(failure(BleErrorCode::LifecycleDestroyed));
                }
                if let Some(code) = state.invalidated.get(&current.id) {
                    return Err(failure(*code));
                }
                if state.provisional.contains_key(&current.peer) {
                    false
                } else {
                    let ids: Vec<_> = state
                        .peers
                        .get(&current.peer)
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|id| *id != current.id)
                        .collect();
                    for id in ids {
                        state.invalidated.entry(id).or_insert(code);
                    }
                    true
                }
            };
            if done {
                self.wake.notify_waiters();
                return Ok(());
            }
            notified.await;
        }
    }

    pub(crate) fn seal(&self) {
        match self.state.lock() {
            Ok(mut state) => state.sealed = true,
            Err(error) => {
                eprintln!("native GATT admission state poisoned during shutdown: {error}");
                error.into_inner().sealed = true;
            }
        }
        self.wake.notify_waiters();
    }

    pub(crate) fn active_slots(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| {
                eprintln!("native GATT admission counter state poisoned: {error}");
                error.into_inner()
            })
            .slots
    }

    #[cfg(test)]
    fn retained_peers(&self) -> usize {
        self.state.lock().unwrap().peers.len()
    }
}

impl Drop for GattRetirementGate {
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((count, _)) = state.blocked.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                state.blocked.remove(&self.peer);
            }
        }
        drop(state);
        self.queue.wake.notify_waiters();
    }
}

impl Drop for GattGlobalRetirementGate {
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((count, _)) = &mut state.global_block {
            *count -= 1;
            if *count == 0 {
                state.global_block = None;
            }
        }
        drop(state);
        self.queue.wake.notify_waiters();
    }
}

impl GattGlobalRetirementGate {
    pub(crate) async fn wait_starting(&self) {
        loop {
            let notified = self.queue.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let starting = !self
                .queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .starting
                .is_empty();
            if !starting {
                return;
            }
            notified.await;
        }
    }
}

impl GattProvisionalGate {
    /// A ticket classified as starting owns its first radio poll. Wait for
    /// that poll before admitting the source fence; already dispatched work
    /// keeps its existing cancellation and settlement ownership.
    pub(crate) async fn wait_starting(&self) {
        loop {
            let notified = self.queue.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let starting = {
                let state = self
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state
                    .peers
                    .get(&self.peer)
                    .is_some_and(|ids| ids.iter().any(|id| state.starting.contains(id)))
            };
            if !starting {
                return;
            }
            notified.await;
        }
    }

    /// Called while the lifecycle owner holds its identity/generation lock.
    pub(crate) fn commit(&self, code: BleErrorCode) {
        self.queue.invalidate_peer(&self.peer, code);
    }
}

impl Drop for GattProvisionalGate {
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(count) = state.provisional.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                state.provisional.remove(&self.peer);
            }
        }
        drop(state);
        self.queue.wake.notify_waiters();
    }
}

impl GattAdmission {
    pub(crate) fn belongs_to(&self, queue: &Arc<GattAdmissionQueue>, peer: &str) -> bool {
        Arc::ptr_eq(&self.queue, queue) && self.peer == peer
    }

    pub(crate) fn assert_current(&self) -> Result<(), DesktopError> {
        let state = self
            .queue
            .state
            .lock()
            .map_err(|_| failure(BleErrorCode::LifecycleInvalidState))?;
        if state.sealed {
            return Err(failure(BleErrorCode::LifecycleDestroyed));
        }
        if let Some(code) = state.invalidated.get(&self.id) {
            return Err(failure(*code));
        }
        Ok(())
    }

    /// Release the FIFO barrier when the radio future has actually been polled.
    /// Native completion remains owned; it may overlap later admitted work.
    pub(crate) async fn dispatch<F, T>(&self, future: F) -> Result<T, DesktopError>
    where
        F: std::future::Future<Output = Result<T, DesktopError>>,
    {
        let mut starting = Some(self.begin_starting().await?);
        let mut future = std::pin::pin!(future);
        let mut started = false;
        std::future::poll_fn(|context| {
            let answer = future.as_mut().poll(context);
            if !started {
                started = true;
                self.mark_dispatched();
                starting.take();
            }
            answer
        })
        .await
    }

    /// Attempt starting without waiting while a caller holds another gate.
    /// `None` means the caller must release that gate before waiting.
    pub(crate) fn try_begin_starting(&self) -> Result<Option<StartingPoll>, DesktopError> {
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| failure(BleErrorCode::LifecycleInvalidState))?;
        if state.sealed {
            return Err(failure(BleErrorCode::LifecycleDestroyed));
        }
        if let Some(code) = state.invalidated.get(&self.id) {
            return Err(failure(*code));
        }
        let owns = if state.starting.contains(&self.id) {
            false
        } else if state.provisional.contains_key(&self.peer)
            || (!state.dispatched.contains(&self.id)
                && state
                    .peers
                    .get(&self.peer)
                    .and_then(|slots| slots.iter().find(|id| !state.dispatched.contains(*id)))
                    .copied()
                    != Some(self.id))
        {
            return Ok(None);
        } else {
            state.starting.insert(self.id);
            true
        };
        Ok(Some(StartingPoll {
            queue: Arc::clone(&self.queue),
            id: self.id,
            owns,
        }))
    }

    pub(crate) async fn begin_starting(&self) -> Result<StartingPoll, DesktopError> {
        loop {
            let notified = self.queue.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(starting) = self.try_begin_starting()? {
                return Ok(starting);
            }
            notified.await;
        }
    }

    pub(crate) fn mark_dispatched(&self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|error| {
            eprintln!("native GATT dispatch state poisoned: {error}");
            error.into_inner()
        });
        state.dispatched.insert(self.id);
        state.starting.remove(&self.id);
        drop(state);
        self.queue.wake.notify_waiters();
    }

    pub async fn wait(&self) -> Result<(), DesktopError> {
        loop {
            let notified = self.queue.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self
                    .queue
                    .state
                    .lock()
                    .map_err(|_| failure(BleErrorCode::LifecycleInvalidState))?;
                if state.sealed {
                    return Err(failure(BleErrorCode::LifecycleDestroyed));
                }
                if let Some(code) = state.invalidated.get(&self.id) {
                    return Err(failure(*code));
                }
                if !state.provisional.contains_key(&self.peer)
                    && state
                        .peers
                        .get(&self.peer)
                        .and_then(|slots| slots.iter().find(|id| !state.dispatched.contains(*id)))
                        .copied()
                        == Some(self.id)
                {
                    return Ok(());
                }
            }
            notified.await;
        }
    }
}

impl Drop for GattAdmission {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|error| {
            eprintln!("native GATT admission state poisoned during retirement: {error}");
            error.into_inner()
        });
        let Some(slots) = state.peers.get_mut(&self.peer) else {
            eprintln!("native GATT admission owner disappeared before retirement");
            return;
        };
        let before = slots.len();
        slots.retain(|id| *id != self.id);
        if before == slots.len() {
            eprintln!("native GATT admission slot disappeared before retirement");
            return;
        }
        let empty = slots.is_empty();
        state.slots -= 1;
        state.invalidated.remove(&self.id);
        state.dispatched.remove(&self.id);
        state.starting.remove(&self.id);
        if empty {
            state.peers.remove(&self.peer);
        }
        drop(state);
        self.queue.wake.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    #[tokio::test]
    async fn dispatched_work_keeps_ownership_without_blocking_later_native_admission() {
        let queue = GattAdmissionQueue::new(4);
        let first = queue.reserve("peer").unwrap();
        let second = queue.reserve("peer").unwrap();
        first.wait().await.unwrap();
        {
            let pending = first.dispatch(std::future::pending::<Result<(), DesktopError>>());
            tokio::pin!(pending);
            assert!(pending.as_mut().now_or_never().is_none());
            second.wait().await.unwrap();
            assert_eq!(queue.active_slots(), 2);
            queue.invalidate_peer("peer", BleErrorCode::GattStaleHandle);
            assert_eq!(
                first.assert_current().unwrap_err().code(),
                BleErrorCode::GattStaleHandle
            );
        }
        drop(first);
        drop(second);
        assert_eq!(queue.active_slots(), 0);
        assert_eq!(queue.retained_peers(), 0);
    }

    #[tokio::test]
    async fn executor_order_cannot_overtake_admission_and_peers_remain_independent() {
        let queue = GattAdmissionQueue::new(4);
        let first = queue.reserve("peer").unwrap();
        let second = queue.reserve("peer").unwrap();
        let other = queue.reserve("other").unwrap();
        assert!(second.wait().now_or_never().is_none());
        other.wait().await.unwrap();
        first.wait().await.unwrap();
        drop(first);
        second.wait().await.unwrap();
        drop(second);
        drop(other);
        assert_eq!(queue.active_slots(), 0);
        assert_eq!(queue.retained_peers(), 0);
    }

    #[tokio::test]
    async fn abandoning_middle_and_front_slots_retires_only_their_admission() {
        let queue = GattAdmissionQueue::new(3);
        let first = queue.reserve("peer").unwrap();
        let cancelled = queue.reserve("peer").unwrap();
        let third = queue.reserve("peer").unwrap();
        drop(cancelled);
        assert!(third.wait().now_or_never().is_none());
        drop(first);
        third.wait().await.unwrap();
        drop(third);
        assert_eq!(queue.active_slots(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_refuses_without_losing_existing_slots_and_sealing_wakes_waiters() {
        let queue = GattAdmissionQueue::new(2);
        let first = queue.reserve("peer").unwrap();
        let second = queue.reserve("peer").unwrap();
        assert_eq!(
            queue.reserve("third").err().unwrap().code(),
            BleErrorCode::CapabilityLimited
        );
        {
            let waiting = second.wait();
            tokio::pin!(waiting);
            assert!(waiting.as_mut().now_or_never().is_none());
            queue.seal();
            assert_eq!(
                waiting.await.unwrap_err().code(),
                BleErrorCode::LifecycleDestroyed
            );
        }
        assert_eq!(
            queue.reserve("peer").err().unwrap().code(),
            BleErrorCode::LifecycleDestroyed
        );
        drop(first);
        drop(second);
        assert_eq!(queue.active_slots(), 0);
    }

    #[tokio::test]
    async fn invalidation_retires_only_admissions_from_the_ended_database() {
        let queue = GattAdmissionQueue::new(4);
        let first = queue.reserve("peer").unwrap();
        let stale = queue.reserve("peer").unwrap();
        let other = queue.reserve("other").unwrap();
        queue.invalidate_peer("peer", BleErrorCode::GattStaleHandle);
        let fresh = queue.reserve("peer").unwrap();
        assert_eq!(
            stale.wait().await.unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        other.wait().await.unwrap();
        drop(first);
        drop(stale);
        fresh.wait().await.unwrap();
        drop(fresh);
        drop(other);
        assert_eq!(queue.active_slots(), 0);
        assert!(queue.state.lock().unwrap().invalidated.is_empty());
    }

    #[tokio::test]
    async fn overlapping_retirement_gates_block_new_and_invalidate_reserved_admissions() {
        let queue = GattAdmissionQueue::new(4);
        let old = queue.reserve("peer").unwrap();
        let first = queue.block_peer("peer", BleErrorCode::GattStaleHandle);
        let second = queue.block_peer("peer", BleErrorCode::ConnectionLost);
        assert_eq!(
            old.wait().await.unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        assert_eq!(
            queue.reserve("peer").err().unwrap().code(),
            BleErrorCode::GattStaleHandle
        );
        queue.reserve("other").unwrap().wait().await.unwrap();
        drop(first);
        assert_eq!(
            queue.reserve("peer").err().unwrap().code(),
            BleErrorCode::GattStaleHandle
        );
        drop(second);
        let fresh = queue.reserve("peer").unwrap();
        drop(old);
        fresh.wait().await.unwrap();
    }

    #[tokio::test]
    async fn global_retirement_gates_cover_existing_and_new_peers_until_all_drop() {
        let queue = GattAdmissionQueue::new(4);
        let old = queue.reserve("peer").unwrap();
        let peer_gate = queue.block_peer("peer", BleErrorCode::GattStaleHandle);
        let first = queue.block_all(BleErrorCode::AdapterResetting);
        let second = queue.block_all(BleErrorCode::AdapterResetting);
        assert_eq!(
            old.wait().await.unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        assert_eq!(
            queue.reserve("new-peer").err().unwrap().code(),
            BleErrorCode::AdapterResetting
        );
        drop(first);
        assert!(queue.reserve("new-peer").is_err());
        drop(second);
        queue.reserve("new-peer").unwrap().wait().await.unwrap();
        assert!(queue.reserve("peer").is_err());
        drop(peer_gate);
        assert!(queue.reserve("peer").is_ok());
    }

    #[tokio::test]
    async fn rediscovery_gate_preserves_only_its_own_admission() {
        let queue = GattAdmissionQueue::new(4);
        let discovery = queue.reserve("peer").unwrap();
        let stale = queue.reserve("peer").unwrap();
        let gate = queue
            .block_peer_except(&discovery, BleErrorCode::GattStaleHandle)
            .unwrap();
        discovery.wait().await.unwrap();
        assert_eq!(
            stale.wait().await.unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        assert!(queue.reserve("peer").is_err());
        drop(gate);
        assert!(queue.reserve("peer").is_ok());
    }

    #[tokio::test]
    async fn nonwaiting_start_releases_group_for_provisional_lifecycle() {
        let queue = GattAdmissionQueue::new(4);
        let admission = queue.reserve("peer").unwrap();
        admission.wait().await.unwrap();
        let group = tokio::sync::Mutex::new(());
        let held = group.lock().await;
        let provisional = queue.provisional_peer("peer");
        assert!(admission.try_begin_starting().unwrap().is_none());
        drop(held);
        let lifecycle = group.lock().await;
        drop(provisional);
        drop(lifecycle);
        admission.wait().await.unwrap();
        let _group = group.lock().await;
        assert!(admission.try_begin_starting().unwrap().is_some());
    }

    #[tokio::test]
    async fn provisional_stale_event_suspends_and_preserves_existing_and_new_tickets() {
        let queue = GattAdmissionQueue::new(4);
        let old = queue.reserve("peer").unwrap();
        old.wait().await.unwrap();
        let gate = queue.provisional_peer("peer");
        let new = queue.reserve("peer").unwrap();
        let first = old.dispatch(async { Ok::<_, DesktopError>(()) });
        tokio::pin!(first);
        assert!(first.as_mut().now_or_never().is_none());
        assert!(new.wait().now_or_never().is_none());
        gate.wait_starting().await;
        drop(gate);
        first.await.unwrap();
        new.wait().await.unwrap();
    }

    #[tokio::test]
    async fn rediscovery_invalidation_waits_for_provisional_scope_decision() {
        let queue = GattAdmissionQueue::new(3);
        let old = queue.reserve("peer").unwrap();
        let discovery = queue.reserve("peer").unwrap();
        let gate = queue.provisional_peer("peer");
        let invalidate =
            queue.invalidate_except_when_open(&discovery, BleErrorCode::GattStaleHandle);
        tokio::pin!(invalidate);
        assert!(invalidate.as_mut().now_or_never().is_none());
        old.assert_current().unwrap();
        drop(gate);
        invalidate.await.unwrap();
        assert_eq!(
            old.assert_current().unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
        discovery.assert_current().unwrap();
    }

    #[tokio::test]
    async fn confirmed_provisional_event_invalidates_held_tickets() {
        let queue = GattAdmissionQueue::new(4);
        let held = queue.reserve("peer").unwrap();
        let gate = queue.provisional_peer("peer");
        gate.wait_starting().await;
        gate.commit(BleErrorCode::GattStaleHandle);
        drop(gate);
        assert_eq!(
            held.wait().await.unwrap_err().code(),
            BleErrorCode::GattStaleHandle
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provisional_fence_waits_for_a_first_poll_already_starting() {
        let queue = GattAdmissionQueue::new(2);
        let admission = queue.reserve("peer").unwrap();
        admission.wait().await.unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let task = tokio::spawn(async move {
            let future = std::future::poll_fn(move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                std::task::Poll::<Result<(), DesktopError>>::Pending
            });
            admission.dispatch(future).await
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let gate = queue.provisional_peer("peer");
        let waiting = gate.wait_starting();
        tokio::pin!(waiting);
        assert!(waiting.as_mut().now_or_never().is_none());
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn provisional_gate_does_not_wait_for_dispatched_pending_future() {
        let queue = GattAdmissionQueue::new(2);
        let admission = queue.reserve("peer").unwrap();
        admission.wait().await.unwrap();
        {
            let operation = admission.dispatch(std::future::pending::<Result<(), DesktopError>>());
            tokio::pin!(operation);
            assert!(operation.as_mut().now_or_never().is_none());
            let gate = queue.provisional_peer("peer");
            tokio::time::timeout(std::time::Duration::from_millis(100), gate.wait_starting())
                .await
                .unwrap();
        }
        drop(admission);
        assert_eq!(queue.active_slots(), 0);
    }

    #[tokio::test]
    async fn nested_start_guard_does_not_release_outer_source_install_fence() {
        let queue = GattAdmissionQueue::new(2);
        let admission = queue.reserve("peer").unwrap();
        admission.wait().await.unwrap();
        let outer = admission.begin_starting().await.unwrap();
        let inner = admission.begin_starting().await.unwrap();
        drop(inner);
        let gate = queue.provisional_peer("peer");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), gate.wait_starting())
                .await
                .is_err()
        );
        drop(outer);
        tokio::time::timeout(std::time::Duration::from_secs(1), gate.wait_starting())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repeated_successful_sessions_do_not_retain_peer_history() {
        let queue = GattAdmissionQueue::new(2);
        for index in 0..1024 {
            let slot = queue.reserve(&format!("peer-{index}")).unwrap();
            slot.wait().await.unwrap();
            drop(slot);
        }
        assert_eq!(queue.active_slots(), 0);
        assert_eq!(queue.retained_peers(), 0);
    }
}
