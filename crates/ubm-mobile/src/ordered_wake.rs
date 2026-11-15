// crates/ubm-mobile/src/ordered_wake.rs
//! Bounded level-triggered readiness for native ordered notification groups.

use crate::foreign::lock;
use crate::radio::OrderedSourceGeneration;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::Notify;

const MAX_GROUPS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedWakeError {
    Capacity,
    Duplicate,
    Unknown,
    GenerationMismatch,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedReady {
    pub group_id: u64,
    pub generation: OrderedSourceGeneration,
}

#[derive(Default)]
struct Entry {
    generation: Option<OrderedSourceGeneration>,
    early: Option<OrderedSourceGeneration>,
    ready: bool,
    in_flight: bool,
    failed: bool,
    failure_reported: bool,
    source_failed: bool,
    retiring: bool,
}
#[derive(Default)]
struct State {
    entries: HashMap<u64, Entry>,
    ready: VecDeque<u64>,
    closed: bool,
}

/// One reserved slot per opening group. Wakes never share the lossy control queue.
#[derive(Default)]
pub struct OrderedWakeRegistry {
    state: Mutex<State>,
    notify: Notify,
}
impl OrderedWakeRegistry {
    /// Reserve before submitting native open; capacity refusal precedes source creation.
    pub fn begin(&self, group_id: u64) -> Result<(), OrderedWakeError> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(OrderedWakeError::Closed);
        }
        if state.entries.contains_key(&group_id) {
            return Err(OrderedWakeError::Duplicate);
        }
        if state.entries.len() == MAX_GROUPS {
            return Err(OrderedWakeError::Capacity);
        }
        state.entries.insert(group_id, Entry::default());
        Ok(())
    }
    /// Bind open's exact generation and preserve a wake which raced completion.
    pub fn bind(
        &self,
        group_id: u64,
        generation: OrderedSourceGeneration,
    ) -> Result<(), OrderedWakeError> {
        let mut state = lock(&self.state);
        let entry = state
            .entries
            .get_mut(&group_id)
            .ok_or(OrderedWakeError::Unknown)?;
        if entry.failed
            || entry.generation.is_some()
            || entry
                .early
                .as_ref()
                .is_some_and(|early| early != &generation)
        {
            entry.failed = true;
            self.notify.notify_one();
            return Err(OrderedWakeError::GenerationMismatch);
        }
        entry.generation = Some(generation);
        let enqueue = entry.early.take().is_some() && !entry.ready;
        if enqueue {
            entry.ready = true;
            state.ready.push_back(group_id);
        }
        drop(state);
        if enqueue {
            self.notify.notify_one();
        }
        Ok(())
    }
    /// Native wake is a level hint, not a record. Conflicts fail the group.
    pub fn wake(
        &self,
        group_id: u64,
        generation: OrderedSourceGeneration,
    ) -> Result<(), OrderedWakeError> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(OrderedWakeError::Closed);
        }
        let entry = state
            .entries
            .get_mut(&group_id)
            .ok_or(OrderedWakeError::Unknown)?;
        if entry.retiring {
            return Err(OrderedWakeError::Closed);
        }
        if entry.failed {
            return Err(OrderedWakeError::GenerationMismatch);
        }
        if let Some(bound) = &entry.generation {
            if bound != &generation {
                entry.failed = true;
                self.notify.notify_one();
                return Err(OrderedWakeError::GenerationMismatch);
            }
            if !entry.ready {
                entry.ready = true;
                if !entry.in_flight {
                    state.ready.push_back(group_id);
                }
            }
        } else if entry
            .early
            .as_ref()
            .is_some_and(|early| early != &generation)
        {
            entry.failed = true;
            self.notify.notify_one();
            return Err(OrderedWakeError::GenerationMismatch);
        } else {
            entry.early = Some(generation);
        }
        drop(state);
        self.notify.notify_one();
        Ok(())
    }
    /// Reserve a reliable source-failure control and re-drive its FIFO.
    /// The native source has already retained its terminal before calling this.
    pub fn wake_failed(
        &self,
        group_id: u64,
        generation: OrderedSourceGeneration,
    ) -> Result<(), OrderedWakeError> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(OrderedWakeError::Closed);
        }
        let entry = state
            .entries
            .get_mut(&group_id)
            .ok_or(OrderedWakeError::Unknown)?;
        if entry.retiring {
            return Err(OrderedWakeError::Closed);
        }
        if entry
            .generation
            .as_ref()
            .or(entry.early.as_ref())
            .is_some_and(|known| known != &generation)
        {
            entry.failed = true;
            self.notify.notify_one();
            return Err(OrderedWakeError::GenerationMismatch);
        }
        if entry.failed {
            return Err(OrderedWakeError::GenerationMismatch);
        }
        entry.source_failed = true;
        entry.failure_reported = false;
        let enqueue = if entry.generation.is_some() {
            let enqueue = !entry.ready && !entry.in_flight;
            entry.ready = true;
            enqueue
        } else {
            entry.early = Some(generation);
            false
        };
        if enqueue {
            state.ready.push_back(group_id);
        }
        drop(state);
        self.notify.notify_one();
        Ok(())
    }
    /// Take one ready group; finish only after the source drain and admission complete.
    pub fn take(&self) -> Option<OrderedReady> {
        let mut state = lock(&self.state);
        while let Some(group_id) = state.ready.pop_front() {
            let Some(entry) = state.entries.get_mut(&group_id) else {
                continue;
            };
            if entry.failed || entry.retiring || entry.in_flight || !entry.ready {
                continue;
            }
            let Some(generation) = entry.generation.clone() else {
                continue;
            };
            entry.ready = false;
            entry.in_flight = true;
            return Some(OrderedReady {
                group_id,
                generation,
            });
        }
        None
    }
    /// A concurrent wake is retained while the consumer drains and rearms.
    pub fn finish(&self, ready: &OrderedReady) -> Result<(), OrderedWakeError> {
        let mut state = lock(&self.state);
        let entry = state
            .entries
            .get_mut(&ready.group_id)
            .ok_or(OrderedWakeError::Unknown)?;
        if entry.generation.as_ref() != Some(&ready.generation) || !entry.in_flight {
            return Err(OrderedWakeError::GenerationMismatch);
        }
        entry.in_flight = false;
        if entry.retiring {
            state.entries.remove(&ready.group_id);
            return Ok(());
        }
        let enqueue = entry.ready && !entry.failed;
        if enqueue {
            state.ready.push_back(ready.group_id);
        }
        drop(state);
        if enqueue {
            self.notify.notify_one();
        }
        Ok(())
    }
    /// A mismatched wake is attributed to the already bound source, even
    /// while its worker is in flight. Opening failures remain with setup.
    pub fn take_mismatch_failure(&self) -> Option<OrderedReady> {
        let mut state = lock(&self.state);
        let (&group_id, entry) = state.entries.iter_mut().find(|(_, entry)| {
            entry.failed && !entry.failure_reported && entry.generation.is_some()
        })?;
        entry.failure_reported = true;
        Some(OrderedReady {
            group_id,
            generation: entry.generation.clone()?,
        })
    }
    pub fn is_retiring(&self, group_id: u64) -> bool {
        lock(&self.state)
            .entries
            .get(&group_id)
            .is_some_and(|entry| entry.retiring)
    }
    #[cfg(test)]
    pub fn has_inflight(&self, group_id: u64) -> bool {
        lock(&self.state)
            .entries
            .get(&group_id)
            .is_some_and(|entry| entry.in_flight)
    }
    pub fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }
    /// Report a failed group exactly once; retain its slot for cleanup.
    pub fn take_failure(&self) -> Option<u64> {
        let mut state = lock(&self.state);
        let (&group_id, entry) = state
            .entries
            .iter_mut()
            .find(|(_, entry)| (entry.failed || entry.source_failed) && !entry.failure_reported)?;
        entry.failure_reported = true;
        Some(group_id)
    }
    /// Keep failed or cleanup-pending entries until the owner explicitly removes them.
    pub fn failed(&self, group_id: u64) -> bool {
        lock(&self.state)
            .entries
            .get(&group_id)
            .is_some_and(|entry| entry.failed || entry.source_failed)
    }
    /// Retire after native cleanup; retain an in-flight slot until its pump turn finishes.
    pub fn remove(&self, group_id: u64) {
        let mut state = lock(&self.state);
        if let Some(entry) = state.entries.get_mut(&group_id) {
            if entry.in_flight {
                entry.retiring = true;
                entry.ready = false;
            } else {
                state.entries.remove(&group_id);
            }
        }
    }
    pub fn close(&self) {
        lock(&self.state).closed = true;
        self.notify.notify_one();
    }
    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generation(n: u64) -> OrderedSourceGeneration {
        OrderedSourceGeneration::Counter(n)
    }
    #[test]
    fn early_and_concurrent_wakes_coalesce_without_loss() {
        let registry = OrderedWakeRegistry::default();
        registry.begin(1).unwrap();
        registry.wake(1, generation(7)).unwrap();
        registry.bind(1, generation(7)).unwrap();
        let first = registry.take().unwrap();
        registry.wake(1, generation(7)).unwrap();
        registry.wake(1, generation(7)).unwrap();
        registry.finish(&first).unwrap();
        let second = registry.take().unwrap();
        registry.finish(&second).unwrap();
        assert!(registry.take().is_none());
    }
    #[test]
    fn bounds_and_mismatched_early_generation_are_explicit() {
        let registry = OrderedWakeRegistry::default();
        for id in 0..MAX_GROUPS {
            registry.begin(id as u64).unwrap();
        }
        assert_eq!(registry.begin(999), Err(OrderedWakeError::Capacity));
        registry.wake(0, generation(1)).unwrap();
        assert_eq!(
            registry.bind(0, generation(2)),
            Err(OrderedWakeError::GenerationMismatch)
        );
        assert!(registry.failed(0));
        registry.remove(0);
        registry.begin(999).unwrap();
    }
    #[test]
    fn failure_control_redrives_source_without_another_wake() {
        let registry = OrderedWakeRegistry::default();
        registry.begin(10).unwrap();
        registry.bind(10, generation(9)).unwrap();
        registry.wake_failed(10, generation(9)).unwrap();
        assert_eq!(registry.take_failure(), Some(10));
        assert_eq!(
            registry.take().unwrap(),
            OrderedReady {
                group_id: 10,
                generation: generation(9)
            }
        );
    }
    #[test]
    fn retirement_keeps_inflight_turn_until_finished() {
        let registry = OrderedWakeRegistry::default();
        registry.begin(1).unwrap();
        registry.bind(1, generation(1)).unwrap();
        registry.wake(1, generation(1)).unwrap();
        let ready = registry.take().unwrap();
        registry.remove(1);
        assert_eq!(registry.begin(1), Err(OrderedWakeError::Duplicate));
        registry.finish(&ready).unwrap();
        registry.begin(2).unwrap();
    }
}
