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
                dispatched: HashSet::new(),
            }),
            wake: Notify::new(),
            maximum,
        })
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

    pub(crate) fn invalidate_except(&self, current: &GattAdmission, code: BleErrorCode) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            eprintln!("native GATT rediscovery state poisoned: {error}");
            error.into_inner()
        });
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
        drop(state);
        self.wake.notify_waiters();
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
        self.assert_current()?;
        let mut future = std::pin::pin!(future);
        let mut started = false;
        std::future::poll_fn(|context| {
            let answer = future.as_mut().poll(context);
            if !started {
                started = true;
                self.mark_dispatched();
            }
            answer
        })
        .await
    }

    pub(crate) fn mark_dispatched(&self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|error| {
            eprintln!("native GATT dispatch state poisoned: {error}");
            error.into_inner()
        });
        state.dispatched.insert(self.id);
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
                if state
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
