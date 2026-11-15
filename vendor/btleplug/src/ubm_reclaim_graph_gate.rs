// vendor/btleplug/src/ubm_reclaim_graph_gate.rs
//! Bounded source callback gate for an exact reclaim graph permit.

use std::collections::LinkedList;
use crate::ubm_reclaim_authority::GraphKey;

#[derive(Debug, PartialEq, Eq)]
pub enum Admission<T> {
    Apply(T),
    Deferred,
    /// The value was refused; the owner must surface an uncertain source tail.
    Overflow(T),
}

#[derive(Debug, PartialEq, Eq)]
pub enum DrainItem<T> { Event(T), Barrier { id: u64, event: T }, BarrierOverflow, Overflow, Finished }

#[derive(Debug, PartialEq, Eq)]
pub enum BarrierAdmission<T> {
    /// The caller owns draining when the gate was previously idle.
    Drain,
    Queued,
    /// The coordinator must authenticate the supplied event against its
    /// retained identity; the gate does not enqueue a second copy.
    Duplicate(T),
    /// The coordinator must retain this mandatory transition and fail-stop.
    Overflow { event: T, start_drain: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateError { Busy, NotCurrent, Invalidated, AlreadyCommitted, Paused }

enum Entry<T> { Event { value: T, bytes: usize }, Mandatory(T), Barrier { id: u64, event: T, bytes: usize }, BarrierOverflow, Overflow }

/// A pre-owned FIFO node for one mandatory effect and its captured resources.
///
/// The caller must back each node and payload with resources already admitted
/// by its owner (for example a detached waiter or generation). The gate does
/// not impose a second arbitrary cut limit or account payload bytes. Creating
/// unlimited independent effects without upstream admission is not bounded.
/// Dropping a node drops its payload; owners requiring recovery must implement
/// that recovery in the payload. Admission itself allocates no queue node.
pub struct MandatoryCut<T> { node: LinkedList<Entry<T>> }

impl<T> MandatoryCut<T> {
    pub fn new(value: T) -> Self {
        let mut node = LinkedList::new();
        node.push_back(Entry::Mandatory(value));
        Self { node }
    }
}

/// The platform owns this structure under its native callback/graph lock.
/// Callback execution and wake delivery happen only after dropping that lock.
pub struct GraphGate<T> {
    permit: Option<GraphKey>,
    committed: bool,
    invalidated: bool,
    draining: bool,
    overflowed: bool,
    overflow_reported: bool,
    max_items: usize,
    max_bytes: usize,
    max_barriers: usize,
    max_barrier_bytes: usize,
    bytes: usize,
    items: usize,
    in_flight_bytes: Option<usize>,
    barriers: usize,
    barrier_bytes: usize,
    barrier_overflow_queued: bool,
    paused_barrier: Option<(u64, usize)>,
    last_resumed: Option<u64>,
    queued: LinkedList<Entry<T>>,
}

impl<T> GraphGate<T> {
    pub fn is_busy(&self) -> bool { self.permit.is_some() || self.draining }
    pub fn is_draining(&self) -> bool { self.draining }
    pub(crate) fn remaining_bytes(&self) -> usize { self.max_bytes.saturating_sub(self.bytes) }
    /// Capacity preflight under the same owner lock as the following admission.
    pub(crate) fn can_reserve_before_admit(&self, bytes: usize) -> bool {
        self.is_busy() && !self.overflowed && self.items < self.max_items
            && bytes <= self.max_bytes.saturating_sub(self.bytes)
    }
    /// Non-mutating identity preflight; the coordinator must keep its
    /// admission fence until it either installs all peer cuts or rejects.
    pub fn barrier_id_seen(&self, id: u64) -> bool {
        self.last_resumed.is_some_and(|last| id <= last)
            || self.paused_barrier.is_some_and(|(current, _)| current == id)
            || self.queued.iter().any(|entry| matches!(entry, Entry::Barrier { id: current, .. } if *current == id))
    }
    pub fn new(max_items: usize, max_bytes: usize) -> Self {
        Self::new_with_barriers(max_items, max_bytes, 0, 0)
    }
    pub fn new_with_barriers(max_items: usize, max_bytes: usize, max_barriers: usize, max_barrier_bytes: usize) -> Self {
        Self { permit: None, committed: false, invalidated: false, draining: false, overflowed: false, overflow_reported: false, max_items, max_bytes, max_barriers, max_barrier_bytes, bytes: 0, items: 0, in_flight_bytes: None, barriers: 0, barrier_bytes: 0, barrier_overflow_queued: false, paused_barrier: None, last_resumed: None, queued: LinkedList::new() }
    }

    pub fn acquire(&mut self, key: GraphKey) -> Result<(), GateError> {
        if self.permit.as_ref() == Some(&key) { return Ok(()); }
        if self.permit.is_some() || self.draining { return Err(GateError::Busy); }
        self.permit = Some(key);
        self.committed = false;
        self.invalidated = false;
        self.overflowed = false;
        self.overflow_reported = false;
        Ok(())
    }

    pub fn admit(&mut self, value: T, bytes: usize, invalidates: bool) -> Admission<T> {
        if self.permit.is_none() && !self.draining { return Admission::Apply(value); }
        if invalidates && !self.committed { self.invalidated = true; }
        if self.overflowed || self.items >= self.max_items || bytes > self.max_bytes.saturating_sub(self.bytes) {
            if !self.overflowed { self.queued.push_back(Entry::Overflow); }
            self.overflowed = true;
            if !self.committed { self.invalidated = true; }
            return Admission::Overflow(value);
        }
        self.bytes += bytes;
        self.items += 1;
        self.queued.push_back(Entry::Event { value, bytes });
        Admission::Deferred
    }

    /// Transfer a mandatory effect into the same FIFO, even if ordinary
    /// capacity is exhausted or the tail has failed. Returns true exactly
    /// when the caller must start a newly established drain. The payload is
    /// returned as `DrainItem::Event` and must be executed or recovered outside
    /// the owner lock before requesting the next item.
    pub fn admit_mandatory_cut(&mut self, mut cut: MandatoryCut<T>) -> bool {
        let start_drain = self.permit.is_none() && !self.draining;
        if start_drain { self.draining = true; }
        if !self.committed { self.invalidated = true; }
        self.queued.append(&mut cut.node);
        start_drain
    }

    /// A bounded reserved control lane, ordered with ordinary callbacks.
    /// The coordinator retains any refused transition and handles the ordered
    /// overflow marker before applying that mandatory state.
    pub fn admit_barrier(&mut self, id: u64, event: T, bytes: usize) -> BarrierAdmission<T> {
        if self.barrier_id_seen(id) {
            return BarrierAdmission::Duplicate(event);
        }
        if self.barriers >= self.max_barriers || bytes > self.max_barrier_bytes.saturating_sub(self.barrier_bytes) || self.overflowed {
            let start_drain = self.permit.is_none() && !self.draining;
            if start_drain { self.draining = true; }
            if !self.barrier_overflow_queued {
                self.queued.push_back(Entry::BarrierOverflow);
                self.barrier_overflow_queued = true;
            }
            self.overflowed = true;
            if !self.committed { self.invalidated = true; }
            return BarrierAdmission::Overflow { event, start_drain };
        }
        let owns_drain = self.permit.is_none() && !self.draining;
        if owns_drain { self.draining = true; }
        self.barriers += 1;
        self.barrier_bytes += bytes;
        self.queued.push_back(Entry::Barrier { id, event, bytes });
        if !self.committed { self.invalidated = true; }
        if owns_drain { BarrierAdmission::Drain } else { BarrierAdmission::Queued }
    }

    pub fn resume_barrier(&mut self, id: u64) -> Result<(), GateError> {
        if let Some((current, bytes)) = self.paused_barrier.filter(|(current, _)| *current == id) {
            self.paused_barrier = None;
            self.barriers -= 1;
            self.barrier_bytes -= bytes;
            self.last_resumed = Some(current);
            return Ok(());
        }
        // Barrier IDs are checked, increasing identities issued by the
        // coordinator. An older resume is a no-effect retry even while a
        // newer barrier is paused.
        if self.last_resumed.is_some_and(|last| id <= last) { return Ok(()); }
        Err(GateError::NotCurrent)
    }

    /// Fail the tail without discarding the accepted prefix or an installed
    /// reset cut. Its ordered overflow marker follows that retained queue.
    /// Returns true when the caller must start the newly established drain.
    pub fn fail_tail(&mut self) -> bool {
        let start_drain = self.permit.is_none() && !self.draining;
        if start_drain { self.draining = true; }
        if !self.overflowed || self.overflow_reported {
            self.queued.push_back(Entry::Overflow);
            self.overflow_reported = false;
        }
        self.overflowed = true;
        if !self.committed { self.invalidated = true; }
        start_drain
    }

    pub fn commit(&mut self, key: &GraphKey) -> Result<(), GateError> {
        if self.permit.as_ref() != Some(key) { return Err(GateError::NotCurrent); }
        if self.invalidated { return Err(GateError::Invalidated); }
        if self.committed { return Ok(()); }
        self.committed = true;
        Ok(())
    }

    /// Transfer the queued prefix to a drain owner. Later source callbacks
    /// continue queueing until `next` observes the queue empty.
    pub fn release(&mut self, key: &GraphKey) -> Result<(), GateError> {
        if self.permit.as_ref() != Some(key) { return Err(GateError::NotCurrent); }
        self.permit = None;
        self.draining = true;
        Ok(())
    }

    /// The owner applies each item outside the lock before requesting another.
    /// Overflow is delivered before the gate permits later callbacks through.
    pub fn next(&mut self) -> Result<DrainItem<T>, GateError> {
        if !self.draining { return Err(GateError::NotCurrent); }
        // Requesting the next item acknowledges completion of the preceding
        // handoff; until then its payload remains part of the gate budget.
        if let Some(bytes) = self.in_flight_bytes.take() {
            self.bytes -= bytes;
            self.items -= 1;
        }
        if self.paused_barrier.is_some() { return Err(GateError::Paused); }
        if let Some(entry) = self.queued.pop_front() {
            return Ok(match entry {
                Entry::Event { value, bytes } => { self.in_flight_bytes = Some(bytes); DrainItem::Event(value) }
                Entry::Mandatory(value) => DrainItem::Event(value),
                Entry::Barrier { id, event, bytes } => { self.paused_barrier = Some((id, bytes)); DrainItem::Barrier { id, event } }
                Entry::BarrierOverflow => { self.barrier_overflow_queued = false; self.overflow_reported = true; DrainItem::BarrierOverflow }
                Entry::Overflow => { self.overflow_reported = true; DrainItem::Overflow }
            });
        }
        self.draining = false;
        self.overflowed = false;
        Ok(DrainItem::Finished)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> GraphKey { GraphKey { authority_id: 1, peer_id: "peer".into(), connection_generation: "connection".into(), graph: ("owner".into(), 1, 1), permit_id: 1 } }

    #[test]
    fn mandatory_cuts_keep_prefix_barrier_and_reentrant_wake_ownership() {
        use std::sync::{Arc, Mutex};

        #[derive(Debug)]
        struct Effect { name: &'static str, wakes: Arc<Mutex<Vec<&'static str>>> }
        impl Effect {
            fn apply(self) { self.wakes.lock().unwrap().push(self.name); }
        }
        let wakes = Arc::new(Mutex::new(Vec::new()));
        let effect = |name| Effect { name, wakes: wakes.clone() };
        let mut gate = GraphGate::new_with_barriers(1, 1, 1, 1);
        let key = key();
        gate.acquire(key.clone()).unwrap();
        assert!(matches!(gate.admit(effect("prefix"), 1, false), Admission::Deferred));
        assert!(!gate.admit_mandatory_cut(MandatoryCut::new(effect("first"))));
        assert_eq!(gate.commit(&key), Err(GateError::Invalidated));
        assert!(matches!(gate.admit_barrier(7, effect("barrier"), 1), BarrierAdmission::Queued));
        assert!(!gate.admit_mandatory_cut(MandatoryCut::new(effect("second"))));
        assert!(wakes.lock().unwrap().is_empty());
        gate.release(&key).unwrap();
        match gate.next().unwrap() { DrainItem::Event(e) => e.apply(), _ => panic!("expected prefix") }
        match gate.next().unwrap() { DrainItem::Event(e) => e.apply(), _ => panic!("expected first cut") }
        // Admission during the first cut's handoff goes behind everything already accepted.
        assert!(!gate.admit_mandatory_cut(MandatoryCut::new(effect("reentrant"))));
        match gate.next().unwrap() { DrainItem::Barrier { id: 7, event } => event.apply(), _ => panic!("expected barrier") }
        assert!(matches!(gate.next(), Err(GateError::Paused)));
        assert_eq!(*wakes.lock().unwrap(), ["prefix", "first", "barrier"]);
        gate.resume_barrier(7).unwrap();
        match gate.next().unwrap() { DrainItem::Event(e) => e.apply(), _ => panic!("expected second cut") }
        match gate.next().unwrap() { DrainItem::Event(e) => e.apply(), _ => panic!("expected reentrant cut") }
        assert!(matches!(gate.next(), Ok(DrainItem::Finished)));
        assert_eq!(*wakes.lock().unwrap(), ["prefix", "first", "barrier", "second", "reentrant"]);
    }

    #[test]
    fn refused_callback_is_reported_before_a_later_mandatory_cut() {
        let mut gate = GraphGate::new(1, 1);
        let key = key();
        gate.acquire(key.clone()).unwrap();
        gate.commit(&key).unwrap();
        assert_eq!(gate.admit(1, 1, false), Admission::Deferred);
        assert_eq!(gate.admit(2, 1, false), Admission::Overflow(2));
        assert!(!gate.admit_mandatory_cut(MandatoryCut::new(3)));
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert_eq!(gate.next(), Ok(DrainItem::Overflow));
        assert_eq!(gate.next(), Ok(DrainItem::Event(3)));
        assert_eq!(gate.next(), Ok(DrainItem::Finished));
    }

    #[test]
    fn cut_transfers_exact_wakers_and_wake_can_reenter_the_unlocked_gate() {
        use std::sync::{Arc, Mutex, Weak};
        use std::task::{Wake, Waker};

        struct Effect { name: &'static str, waiter: Option<Waker> }
        struct Probe {
            name: &'static str,
            woke: Arc<Mutex<Vec<&'static str>>>,
            gate: Weak<Mutex<GraphGate<Effect>>>,
            reenter: bool,
        }
        impl Wake for Probe {
            fn wake(self: Arc<Self>) {
                self.woke.lock().unwrap().push(self.name);
                if self.reenter {
                    let gate = self.gate.upgrade().unwrap();
                    let mut gate = gate.try_lock().expect("wake ran under gate lock");
                    assert!(!gate.admit_mandatory_cut(MandatoryCut::new(Effect {
                        name: "reentrant", waiter: None,
                    })));
                }
            }
        }
        let gate = Arc::new(Mutex::new(GraphGate::new_with_barriers(1, 1, 1, 1)));
        let woke = Arc::new(Mutex::new(Vec::new()));
        let waker = |name, reenter| Waker::from(Arc::new(Probe {
            name, woke: woke.clone(), gate: Arc::downgrade(&gate), reenter,
        }));
        let mut first = Some(waker("first waiter", true));
        let mut second = Some(waker("second waiter", false));
        let later = waker("later waiter", false);
        let key = key();
        {
            let mut gate = gate.lock().unwrap();
            gate.acquire(key.clone()).unwrap();
            gate.commit(&key).unwrap();
            assert!(matches!(gate.admit(Effect { name: "prefix", waiter: None }, 1, false), Admission::Deferred));
            assert!(!gate.admit_mandatory_cut(MandatoryCut::new(Effect { name: "cut one", waiter: first.take() })));
            assert!(matches!(gate.admit_barrier(7, Effect { name: "barrier", waiter: None }, 1), BarrierAdmission::Queued));
            assert!(!gate.admit_mandatory_cut(MandatoryCut::new(Effect { name: "cut two", waiter: second.take() })));
            gate.release(&key).unwrap();
        }
        assert!(woke.lock().unwrap().is_empty());
        let mut replayed = Vec::new();
        loop {
            let next = gate.lock().unwrap().next().unwrap();
            match next {
                DrainItem::Event(effect) => {
                    replayed.push(effect.name);
                    if let Some(waiter) = effect.waiter { waiter.wake(); }
                }
                DrainItem::Barrier { id, event } => {
                    replayed.push(event.name);
                    assert!(matches!(gate.lock().unwrap().next(), Err(GateError::Paused)));
                    gate.lock().unwrap().resume_barrier(id).unwrap();
                }
                DrainItem::Finished => break,
                _ => panic!("unexpected overflow"),
            }
        }
        assert_eq!(replayed, ["prefix", "cut one", "barrier", "cut two", "reentrant"]);
        assert_eq!(*woke.lock().unwrap(), ["first waiter", "second waiter"]);
        drop(later);
        assert_eq!(woke.lock().unwrap().len(), 2);
    }

    #[test]
    fn idle_cut_starts_one_drain_and_owned_payload_can_recover_on_drop() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        struct Owned(Arc<AtomicUsize>);
        impl Drop for Owned {
            fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
        }
        let recovered = Arc::new(AtomicUsize::new(0));
        let mut gate = GraphGate::new(0, 0);
        assert!(gate.admit_mandatory_cut(MandatoryCut::new(Owned(recovered.clone()))));
        assert!(!gate.admit_mandatory_cut(MandatoryCut::new(Owned(recovered.clone()))));
        assert_eq!(recovered.load(Ordering::SeqCst), 0);
        let first = match gate.next().unwrap() { DrainItem::Event(effect) => effect, _ => panic!("expected cut") };
        drop(first);
        assert_eq!(recovered.load(Ordering::SeqCst), 1);
        drop(gate);
        assert_eq!(recovered.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn reservation_preflight_accounts_for_capacity_across_control_transitions() {
        let mut gate = GraphGate::new_with_barriers(2, 8, 1, 8);
        let key = key();
        gate.acquire(key.clone()).unwrap();
        assert!(gate.can_reserve_before_admit(1));
        assert_eq!(gate.admit(1, 1, false), Admission::Deferred);
        assert!(gate.can_reserve_before_admit(1));
        assert_eq!(gate.admit_barrier(7, 9, 1), BarrierAdmission::Queued);
        assert!(gate.can_reserve_before_admit(1));
        assert_eq!(gate.admit(1, 1, false), Admission::Deferred);
        assert!(!gate.can_reserve_before_admit(1));
        assert_eq!(gate.admit(2, 1, false), Admission::Overflow(2));
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert_eq!(gate.next(), Ok(DrainItem::Barrier { id: 7, event: 9 }));
        gate.resume_barrier(7).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert_eq!(gate.next(), Ok(DrainItem::Overflow));
    }

    #[test]
    fn in_flight_effect_remains_charged_during_reentrant_admission() {
        let mut gate = GraphGate::new(1, 4);
        let key = key();
        gate.acquire(key.clone()).unwrap();
        gate.commit(&key).unwrap();
        assert_eq!(gate.admit(1, 4, false), Admission::Deferred);
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert!(!gate.can_reserve_before_admit(1));
    }

    #[test]
    fn committed_gate_replays_prefix_and_callbacks_arriving_during_drain() {
        let mut gate = GraphGate::new(3, 8);
        let key = key(); gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
        assert_eq!(gate.admit(1, 1, false), Admission::Deferred);
        assert_eq!(gate.admit(2, 1, true), Admission::Deferred);
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert_eq!(gate.admit(3, 1, false), Admission::Deferred);
        assert_eq!(gate.next(), Ok(DrainItem::Event(2)));
        assert_eq!(gate.next(), Ok(DrainItem::Event(3)));
        assert_eq!(gate.next(), Ok(DrainItem::Finished));
        assert_eq!(gate.admit(4, 1, false), Admission::Apply(4));
    }

    #[test]
    fn precommit_invalidation_and_postcommit_overflow_remain_explicit() {
        let mut gate = GraphGate::new(1, 1); let key = key();
        gate.acquire(key.clone()).unwrap();
        assert_eq!(gate.admit(1, 1, true), Admission::Deferred);
        assert_eq!(gate.commit(&key), Err(GateError::Invalidated));
        gate.release(&key).unwrap(); gate.next().unwrap(); gate.next().unwrap();
        gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
        assert_eq!(gate.admit(2, 1, false), Admission::Deferred);
        assert_eq!(gate.admit(3, 1, true), Admission::Overflow(3));
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(2)));
        assert_eq!(gate.next(), Ok(DrainItem::Overflow));
        assert_eq!(gate.admit(4, 1, false), Admission::Overflow(4));
        assert_eq!(gate.next(), Ok(DrainItem::Finished));
    }

    #[test]
    fn reserved_barrier_follows_full_value_prefix_and_pauses_until_resume() {
        let mut gate = GraphGate::new_with_barriers(1, 1, 2, 2); let key = key();
        gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
        assert_eq!(gate.admit(10, 1, false), Admission::Deferred);
        assert_eq!(gate.admit_barrier(1, 20, 1), BarrierAdmission::Queued);
        assert!(gate.barrier_id_seen(1));
        assert!(!gate.barrier_id_seen(2));
        assert_eq!(gate.admit_barrier(1, 20, 1), BarrierAdmission::Duplicate(20));
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(10)));
        assert_eq!(gate.next(), Ok(DrainItem::Barrier { id: 1, event: 20 }));
        assert_eq!(gate.next(), Err(GateError::Paused));
        assert_eq!(gate.resume_barrier(9), Err(GateError::NotCurrent));
        assert_eq!(gate.resume_barrier(1), Ok(()));
        assert_eq!(gate.resume_barrier(1), Ok(()));
        assert_eq!(gate.admit_barrier(1, 20, 1), BarrierAdmission::Duplicate(20));
        assert_eq!(gate.next(), Ok(DrainItem::Finished));
    }

    #[test]
    fn two_peers_wait_for_coordinator_and_preserve_interleaved_resets() {
        let mut first = GraphGate::new_with_barriers(4, 4, 2, 2);
        let mut second = GraphGate::new_with_barriers(4, 4, 2, 2);
        let first_key = key(); let mut second_key = key(); second_key.peer_id = "other".into();
        first.acquire(first_key.clone()).unwrap(); second.acquire(second_key.clone()).unwrap();
        first.commit(&first_key).unwrap(); second.commit(&second_key).unwrap();
        assert_eq!(first.admit(1, 1, false), Admission::Deferred);
        assert_eq!(first.admit_barrier(11, 100, 1), BarrierAdmission::Queued);
        assert_eq!(first.admit(2, 1, false), Admission::Deferred);
        assert_eq!(first.admit_barrier(12, 200, 1), BarrierAdmission::Queued);
        assert_eq!(second.admit_barrier(11, 100, 1), BarrierAdmission::Queued);
        second.release(&second_key).unwrap();
        assert_eq!(second.next(), Ok(DrainItem::Barrier { id: 11, event: 100 }));
        assert_eq!(second.next(), Err(GateError::Paused));
        first.release(&first_key).unwrap();
        assert_eq!(first.next(), Ok(DrainItem::Event(1)));
        assert_eq!(first.next(), Ok(DrainItem::Barrier { id: 11, event: 100 }));
        // The adapter coordinator applies reset 11 only after both arrivals.
        first.resume_barrier(11).unwrap(); second.resume_barrier(11).unwrap();
        assert_eq!(second.next(), Ok(DrainItem::Finished));
        assert_eq!(first.next(), Ok(DrainItem::Event(2)));
        assert_eq!(first.next(), Ok(DrainItem::Barrier { id: 12, event: 200 }));
        assert_eq!(first.resume_barrier(11), Ok(()));
        assert_eq!(first.next(), Err(GateError::Paused));
        first.resume_barrier(12).unwrap();
        assert_eq!(first.next(), Ok(DrainItem::Finished));
    }

    #[test]
    fn exhausted_barrier_lane_retains_prefix_and_explicit_fail_stop() {
        let mut gate = GraphGate::new_with_barriers(1, 1, 1, 1); let key = key();
        gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
        assert_eq!(gate.admit(1, 1, false), Admission::Deferred);
        assert_eq!(gate.admit_barrier(1, 10, 1), BarrierAdmission::Queued);
        assert_eq!(gate.admit_barrier(2, 20, 1), BarrierAdmission::Overflow { event: 20, start_drain: false });
        assert_eq!(gate.admit(2, 1, false), Admission::Overflow(2));
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert_eq!(gate.next(), Ok(DrainItem::Barrier { id: 1, event: 10 }));
        gate.resume_barrier(1).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::BarrierOverflow));
        assert_eq!(gate.next(), Ok(DrainItem::Finished));
        let mut idle = GraphGate::new_with_barriers(1, 1, 0, 0);
        assert_eq!(idle.admit_barrier(7, 70, 1), BarrierAdmission::Overflow { event: 70, start_drain: true });
        assert_eq!(idle.next(), Ok(DrainItem::BarrierOverflow));
        assert_eq!(idle.next(), Ok(DrainItem::Finished));
        let mut bytes = GraphGate::new_with_barriers(1, 1, 2, 1);
        assert_eq!(bytes.admit_barrier(1, 10, 1), BarrierAdmission::Drain);
        assert_eq!(bytes.admit_barrier(2, 20, 1), BarrierAdmission::Overflow { event: 20, start_drain: false });
        assert_eq!(bytes.next(), Ok(DrainItem::Barrier { id: 1, event: 10 }));
        bytes.resume_barrier(1).unwrap();
        assert_eq!(bytes.next(), Ok(DrainItem::BarrierOverflow));
        assert_eq!(bytes.next(), Ok(DrainItem::Finished));
    }

    #[test]
    fn fail_tail_preserves_the_reserved_cut_before_reporting_loss() {
        let mut gate = GraphGate::new_with_barriers(2, 2, 1, 1); let key = key();
        gate.acquire(key.clone()).unwrap(); gate.commit(&key).unwrap();
        assert_eq!(gate.admit(1, 1, false), Admission::Deferred);
        assert_eq!(gate.admit_barrier(1, 10, 1), BarrierAdmission::Queued);
        gate.fail_tail(); gate.fail_tail();
        assert_eq!(gate.admit(2, 1, false), Admission::Overflow(2));
        gate.release(&key).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Event(1)));
        assert_eq!(gate.next(), Ok(DrainItem::Barrier { id: 1, event: 10 }));
        gate.resume_barrier(1).unwrap();
        assert_eq!(gate.next(), Ok(DrainItem::Overflow));
        assert_eq!(gate.next(), Ok(DrainItem::Finished));
        let mut idle = GraphGate::new_with_barriers(1, 1, 1, 1);
        assert!(idle.fail_tail());
        assert!(!idle.fail_tail());
        assert_eq!(idle.admit(3, 1, false), Admission::Overflow(3));
        assert_eq!(idle.next(), Ok(DrainItem::Overflow));
        assert_eq!(idle.next(), Ok(DrainItem::Finished));
    }
}
