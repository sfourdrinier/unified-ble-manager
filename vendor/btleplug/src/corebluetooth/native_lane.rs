//! Queue-local ownership of native identities and connection cycles.

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::native_attempt::{Action, DisconnectError, NativeAttempts};
use super::native_registry::{NativeObjectLease, NativeObjectRegistry, NativeObjectToken};

/// This object and the native objects it holds must remain on the delegate
/// queue. An ID identifies a cycle for cancellation and delivery receipts;
/// callback attribution uses only the manager and native object identities.
pub(super) struct NativeLane<M, N, O, R, E> {
    objects: NativeObjectRegistry<M, N>,
    attempts: NativeAttempts<Uuid, (), NativeObjectToken, O, R, E>,
    leases: HashMap<u64, (Uuid, NativeObjectLease)>,
    // Discovery owns one current receipt per (peer, logical owner). A scan can
    // report the same peripheral indefinitely, so a fresh advertisement must
    // replace and retire the old receipt instead of accumulating native
    // objects. The actor never receives this object; it carries only the
    // logical owner across the queue boundary.
    discovered: HashMap<NativeObjectLease, (Uuid, O)>,
    observations: HashMap<NativeObjectLease, Uuid>,
    pending_connect_results: HashSet<u64>,
    next: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TerminalReceiptError {
    NotTerminal,
    ConnectResultPending,
}

#[allow(dead_code)]
impl<M, N, O: Clone, R, E> NativeLane<M, N, O, R, E> {
    pub(super) fn new(manager: M) -> Self {
        Self {
            objects: NativeObjectRegistry::new(manager),
            attempts: NativeAttempts::new(()),
            leases: HashMap::new(),
            discovered: HashMap::new(),
            observations: HashMap::new(),
            pending_connect_results: HashSet::new(),
            next: Some(1),
        }
    }

    pub(super) fn connect(
        &mut self,
        peer: Uuid,
        object: N,
        owner: O,
        reply: R,
        same: impl Fn(&N, &N) -> bool,
    ) -> Result<(u64, Option<Action<NativeObjectToken, O, R, E>>), R> {
        // Bound both active and queued native retains, including across peers.
        if self.leases.len() >= 4096 {
            return Err(reply);
        }
        let Some(id) = self.next else {
            return Err(reply);
        };
        self.next = id.checked_add(1);
        let Some(token) = self.objects.register(peer, object, same) else {
            return Err(reply);
        };
        let Some(lease) = self.objects.lease(peer, token) else {
            self.objects.discard_unleased(peer, token);
            return Err(reply);
        };
        self.leases.insert(id, (peer, lease));
        Ok((id, self.attempts.connect(peer, token, id, owner, reply)))
    }

    pub(super) fn manager(&self) -> &M {
        self.objects.manager()
    }

    /// Register a discovery on the delegate queue.  The returned receipt is
    /// deliberately distinct from an attempt lease: retiring an attempt must
    /// not make a later queued request lose the exact peripheral it selected.
    pub(super) fn remember(
        &mut self,
        peer: Uuid,
        object: N,
        owner: O,
        same: impl Fn(&N, &N) -> bool,
        same_owner: impl Fn(&O, &O) -> bool,
    ) -> Option<NativeObjectLease> {
        // Keep discovery retention bounded even when the scan sees an
        // unbounded stream of distinct peers. Replacement below is not a new
        // entry and is permitted at capacity.
        let replacing = self.discovered.iter().find_map(|(lease, (candidate_peer, candidate_owner))|
            (*candidate_peer == peer && same_owner(candidate_owner, &owner)).then_some(*lease)
        );
        if replacing.is_none() && self.discovered.len() >= 4096 {
            return None;
        }
        let token = self.objects.register(peer, object, same)?;
        let lease = self.objects.lease(peer, token)?;
        if let Some(old) = replacing {
            let (old_peer, _) = self.discovered.remove(&old).expect("remembered discovery disappeared");
            assert!(self.objects.retire(old_peer, old), "native discovery lost its lease");
        }
        self.discovered.insert(lease, (peer, owner));
        Some(lease)
    }

    /// Resolve the exact object retained for this logical owner.  UUID alone
    /// is intentionally insufficient: a replacement owner must never inherit
    /// an old callback or native object.
    pub(super) fn connect_remembered(
        &mut self,
        peer: Uuid,
        owner: O,
        reply: R,
        same_owner: impl Fn(&O, &O) -> bool,
    ) -> Result<(u64, Option<Action<NativeObjectToken, O, R, E>>), R> {
        if self.leases.len() >= 4096 { return Err(reply); }
        let Some(id) = self.next else { return Err(reply); };
        let Some((lease, _)) = self.discovered.iter().find(|(_, (candidate_peer, candidate_owner))|
            *candidate_peer == peer && same_owner(candidate_owner, &owner)
        ) else { return Err(reply); };
        let object = lease.object();
        let Some(cycle_lease) = self.objects.lease(peer, object) else { return Err(reply); };
        self.next = id.checked_add(1);
        self.leases.insert(id, (peer, cycle_lease));
        Ok((id, self.attempts.connect(peer, object, id, owner, reply)))
    }

    /// Retain an observed native identity independently of attempt ownership.
    /// A callback alone does not prove that a connection attempt exists.
    pub(super) fn observe(
        &mut self,
        manager: &M,
        peer: Uuid,
        object: N,
        same_manager: impl Fn(&M, &M) -> bool,
        same_object: impl Fn(&N, &N) -> bool,
    ) -> Option<NativeObjectLease> {
        if !same_manager(self.objects.manager(), manager) || self.observations.len() >= 4096 {
            return None;
        }
        let token = self.objects.register(peer, object, same_object)?;
        let Some(lease) = self.objects.lease(peer, token) else {
            self.objects.discard_unleased(peer, token);
            return None;
        };
        self.observations.insert(lease, peer);
        Some(lease)
    }

    pub(super) fn retire_observation(&mut self, peer: Uuid, lease: NativeObjectLease) -> bool {
        if self.observations.get(&lease) != Some(&peer) { return false; }
        self.observations.remove(&lease);
        assert!(self.objects.retire(peer, lease), "native observation lost its lease");
        true
    }
    pub(super) fn resolve(&self, peer: Uuid, token: NativeObjectToken) -> Option<&N> {
        self.objects.resolve(peer, token)
    }

    pub(super) fn cancel(
        &mut self,
        peer: Uuid,
        id: u64,
    ) -> Option<Action<NativeObjectToken, O, R, E>> {
        let action = self.attempts.cancel(&peer, id)?;
        if matches!(&action, Action::CancelledQueued { .. }) {
            self.retire(id);
        }
        Some(action)
    }

    /// Resolve a disconnect against the dispatched cycle's exact logical owner.
    /// Queued owners and terminal cycles cannot acquire the active cycle. An
    /// admitted cycle retains its owner identity even after registry retirement.
    pub(super) fn active_cycle_for_owner(
        &self,
        peer: Uuid,
        owner: &O,
        same_owner: impl Fn(&O, &O) -> bool,
    ) -> Option<u64> {
        self.attempts
            .active_cycle_for_owner(&peer, owner, same_owner)
    }

    pub(super) fn disconnect(
        &mut self,
        peer: Uuid,
        id: u64,
        reply: R,
    ) -> Result<Option<Action<NativeObjectToken, O, R, E>>, DisconnectError<R>> {
        self.attempts.disconnect(&peer, id, reply)
    }

    /// A disconnect is tied to the admitted native cycle, not to whichever
    /// registry owner happens to be current after replacement.
    pub(super) fn disconnect_for_owner(
        &mut self,
        peer: Uuid,
        owner: &O,
        reply: R,
        same_owner: impl Fn(&O, &O) -> bool,
    ) -> Result<Option<Action<NativeObjectToken, O, R, E>>, DisconnectError<R>> {
        let Some(id) = self.active_cycle_for_owner(peer, owner, same_owner) else {
            return Err(DisconnectError::UnknownCycle(reply));
        };
        self.disconnect(peer, id, reply)
    }

    fn associated(
        &self,
        manager: &M,
        object: &N,
        peer: Uuid,
        same_manager: impl Fn(&M, &M) -> bool,
        same: impl Fn(&N, &N) -> bool,
    ) -> Option<NativeObjectToken> {
        if !same_manager(self.objects.manager(), manager) {
            return None;
        }
        self.objects.lookup(peer, object, same)
    }

    pub(super) fn connected(
        &mut self,
        manager: &M,
        object: &N,
        peer: Uuid,
        same_manager: impl Fn(&M, &M) -> bool,
        same: impl Fn(&N, &N) -> bool,
    ) -> Vec<Action<NativeObjectToken, O, R, E>> {
        let Some(token) = self.associated(manager, object, peer, same_manager, same) else {
            return vec![Action::Unassociated { error: None }];
        };
        let actions = self
            .attempts
            .connected(&(), |_, _| true, &peer, &token, |a, b| a == b);
        self.record_connect_results(&actions);
        actions
    }

    pub(super) fn failed(
        &mut self,
        manager: &M,
        object: &N,
        peer: Uuid,
        same_manager: impl Fn(&M, &M) -> bool,
        same: impl Fn(&N, &N) -> bool,
        error: E,
    ) -> Vec<Action<NativeObjectToken, O, R, E>> {
        let Some(token) = self.associated(manager, object, peer, same_manager, same) else {
            return vec![Action::Unassociated { error: Some(error) }];
        };
        let actions = self
            .attempts
            .failed(&(), |_, _| true, &peer, &token, |a, b| a == b, error);
        self.record_connect_results(&actions);
        actions
    }

    pub(super) fn disconnected(
        &mut self,
        manager: &M,
        object: &N,
        peer: Uuid,
        same_manager: impl Fn(&M, &M) -> bool,
        same: impl Fn(&N, &N) -> bool,
        error: Option<E>,
    ) -> Vec<Action<NativeObjectToken, O, R, E>> {
        let Some(token) = self.associated(manager, object, peer, same_manager, same) else {
            return vec![Action::Unassociated { error }];
        };
        self.attempts
            .disconnected(&(), |_, _| true, &peer, &token, |a, b| a == b, error)
    }

    /// Only the exact delivered terminal receipt may retire the native lease
    /// and start the next cycle. A stale receipt changes nothing.
    pub(super) fn complete_terminal(
        &mut self,
        peer: Uuid,
        id: u64,
    ) -> Result<Option<Action<NativeObjectToken, O, R, E>>, TerminalReceiptError> {
        if !self.attempts.is_terminal(&peer, id) {
            return Err(TerminalReceiptError::NotTerminal);
        }
        if self.pending_connect_results.contains(&id) {
            return Err(TerminalReceiptError::ConnectResultPending);
        }
        let next = self.attempts.complete_terminal(&peer, id);
        self.retire(id);
        Ok(next)
    }

    fn record_connect_results(&mut self, actions: &[Action<NativeObjectToken, O, R, E>]) {
        for action in actions {
            if let Action::ConnectResult { id, .. } = action {
                assert!(
                    self.pending_connect_results.insert(*id),
                    "duplicate connect result receipt"
                );
            }
        }
    }

    /// Acknowledge only after the exact result has been delivered or retained
    /// by its destination. The executor owns the returned action until then.
    pub(super) fn complete_connect_result(&mut self, peer: Uuid, id: u64) -> bool {
        if !self
            .leases
            .get(&id)
            .is_some_and(|(owner_peer, _)| *owner_peer == peer)
        {
            return false;
        }
        self.pending_connect_results.remove(&id)
    }

    fn retire(&mut self, id: u64) {
        if let Some((peer, lease)) = self.leases.remove(&id) {
            assert!(
                self.objects.retire(peer, lease),
                "native cycle lost its lease"
            );
        }
    }
}

impl<M, N, O: Clone, R, E: Clone> NativeLane<M, N, O, R, E> {
    /// Retire all native retains and return every caller reply for explicit
    /// settlement before the queue context is destroyed. Dropping the lane is
    /// not a settlement mechanism: it would otherwise strand futures waiting
    /// for a CoreBluetooth callback that can no longer arrive.
    pub(super) fn abort_all(&mut self, error: E) -> Vec<Action<NativeObjectToken, O, R, E>> {
        let actions = self.attempts.abort_all(error);
        let leases = std::mem::take(&mut self.leases);
        for (_, (peer, lease)) in leases {
            assert!(self.objects.retire(peer, lease), "native cycle lost its lease during queue shutdown");
        }
        let discovered = std::mem::take(&mut self.discovered);
        for (lease, (peer, _)) in discovered {
            assert!(self.objects.retire(peer, lease), "native discovery lost its lease during queue shutdown");
        }
        let observations = std::mem::take(&mut self.observations);
        for (lease, peer) in observations {
            assert!(self.objects.retire(peer, lease), "native observation lost its lease during queue shutdown");
        }
        self.pending_connect_results.clear();
        actions
    }
}

/// Execute the queue-local terminal callback path independently of the
/// CoreBluetooth object model.  The production delegate supplies its retained
/// `CBCentralManager` and `CBPeripheral`; tests supply identity-safe Rust
/// objects.  Keeping this path generic is deliberate: CoreBluetooth framework
/// objects cannot be constructed safely for tests, yet the callback-to-lane
/// ordering and terminal receipt rules must be exercised through the same
/// executor the delegate calls.
///
/// A terminal cycle is acknowledged only after its lifecycle observation has
/// been accepted by the original owner (or retained by that owner's gate).
/// Returning `false` from `deliver_lifecycle` keeps the terminal receipt and
/// prevents a queued successor from dispatching.  This makes queue pressure,
/// actor rejection and a closed delivery destination explicit rather than
/// allowing them to reorder or lose a disconnect.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_disconnect_callback<M, N, O, R, E, SameManager, SameObject,
    SettleConnect, SettleDisconnect, DeliverLifecycle, StartConnect, Unassociated>(
    lane: &mut NativeLane<M, N, O, R, E>,
    manager: &M,
    object: &N,
    peer: Uuid,
    same_manager: SameManager,
    same_object: SameObject,
    error: Option<E>,
    mut settle_connect: SettleConnect,
    mut settle_disconnect: SettleDisconnect,
    mut deliver_lifecycle: DeliverLifecycle,
    mut start_connect: StartConnect,
    mut unassociated: Unassociated,
) where
    O: Clone,
    E: Clone,
    SameManager: Fn(&M, &M) -> bool,
    SameObject: Fn(&N, &N) -> bool,
    SettleConnect: FnMut(R, Option<E>),
    SettleDisconnect: FnMut(Vec<R>, Option<E>),
    DeliverLifecycle: FnMut(&O, Option<E>) -> bool,
    StartConnect: FnMut(&M, &N),
    Unassociated: FnMut(Option<E>),
{
    let actions = lane.disconnected(
        manager,
        object,
        peer,
        same_manager,
        same_object,
        error,
    );
    for action in actions {
        let (id, owner, connect_reply, terminal_error, disconnect_replies) = match action {
            Action::Disconnected { id, owner, error, disconnect_replies } => {
                (id, owner, None, error, disconnect_replies)
            }
            Action::DisconnectedWhileConnecting { id, owner, reply, error, disconnect_replies } => {
                (id, owner, Some(reply), error, disconnect_replies)
            }
            Action::Unassociated { error } => {
                unassociated(error);
                continue;
            }
            _ => unreachable!("disconnect callback can only resolve a terminal disconnect action"),
        };

        if let Some(reply) = connect_reply {
            settle_connect(reply, terminal_error.clone());
        }
        settle_disconnect(disconnect_replies, terminal_error.clone());

        // Do not call complete_terminal until the exact old owner has either
        // accepted or durably captured its lifecycle.  The lane still owns
        // the object and queued successor while delivery is pending.
        if !deliver_lifecycle(&owner, terminal_error) {
            continue;
        }

        match lane.complete_terminal(peer, id) {
            Ok(Some(Action::Connect { object, .. })) => {
                let next = lane.resolve(peer, object)
                    .expect("queued lane cycle lost its native peripheral");
                start_connect(lane.manager(), next);
            }
            Ok(None) => {}
            Err(TerminalReceiptError::NotTerminal) => {
                unreachable!("disconnected action must be terminal")
            }
            Err(TerminalReceiptError::ConnectResultPending) => {
                unreachable!("a disconnected cycle cannot retain a connect result receipt")
            }
            Ok(Some(_)) => unreachable!("a completed disconnect cycle can only start another connect"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn same(a: &Rc<u8>, b: &Rc<u8>) -> bool {
        Rc::ptr_eq(a, b)
    }

    #[test]
    fn lifecycle_observations_retain_exact_native_identity_until_retired() {
        let manager = Rc::new(1);
        let other_manager = Rc::new(1);
        let object = Rc::new(2);
        let replacement = Rc::new(2);
        let weak = Rc::downgrade(&object);
        let peer = Uuid::from_u128(91);
        let mut lane = NativeLane::<_, _, (), (), ()>::new(manager.clone());
        assert!(lane.observe(&other_manager, peer, object.clone(), same, same).is_none());
        let first = lane.observe(&manager, peer, object.clone(), same, same).unwrap();
        let second = lane.observe(&manager, peer, object.clone(), same, same).unwrap();
        let other = lane.observe(&manager, peer, replacement, same, same).unwrap();
        assert_eq!(first.object(), second.object());
        assert_ne!(first.object(), other.object());
        drop(object);
        assert!(!lane.retire_observation(Uuid::from_u128(92), first));
        assert!(lane.retire_observation(peer, first));
        assert!(!lane.retire_observation(peer, first));
        assert!(weak.upgrade().is_some());
        assert!(lane.retire_observation(peer, second));
        assert!(weak.upgrade().is_none());
        assert!(lane.retire_observation(peer, other));
    }

    #[test]
    fn remembered_object_is_selected_only_by_its_exact_owner() {
        let manager = Rc::new(1);
        let peer = Uuid::from_u128(90);
        let old_owner = Rc::new(3);
        let replacement = Rc::new(3);
        let object = Rc::new(2);
        let mut lane = NativeLane::<_, _, _, _, ()>::new(manager);
        let remembered = lane.remember(peer, object, old_owner.clone(), same, same).unwrap();
        assert!(lane.connect_remembered(peer, replacement, "wrong", same).is_err());
        assert!(matches!(
            lane.connect_remembered(peer, old_owner, "right", same),
            Ok((_, Some(Action::Connect { .. })))
        ));
        assert!(lane.retire_observation(peer, remembered) == false,
            "discovery receipts are distinct from callback observations");
    }

    #[test]
    fn repeated_discovery_replaces_the_same_owner_receipt_and_selects_the_latest_object() {
        let manager = Rc::new(1);
        let peer = Uuid::from_u128(901);
        let owner = Rc::new(3);
        let first = Rc::new(2);
        let first_weak = Rc::downgrade(&first);
        let latest = Rc::new(4);
        let mut lane = NativeLane::<_, _, _, _, ()>::new(manager);

        lane.remember(peer, first.clone(), owner.clone(), same, same).unwrap();
        lane.remember(peer, latest.clone(), owner.clone(), same, same).unwrap();
        drop(first);
        assert!(first_weak.upgrade().is_none(), "superseded discovery must retire its native retain");

        let (_, action) = lane.connect_remembered(peer, owner, "reply", same).unwrap();
        let Action::Connect { object, .. } = action.expect("first remembered connect dispatches") else {
            panic!("remembered connect must dispatch")
        };
        assert_eq!(lane.resolve(peer, object).unwrap().as_ref(), latest.as_ref());
    }

    #[test]
    fn retiring_an_observation_does_not_retire_an_attempt() {
        let manager = Rc::new(1);
        let object = Rc::new(2);
        let weak = Rc::downgrade(&object);
        let peer = Uuid::from_u128(93);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let (id, _) = lane.connect(peer, object.clone(), "owner", "reply", same).unwrap();
        let observation = lane.observe(&manager, peer, object.clone(), same, same).unwrap();
        drop(object);
        assert!(lane.retire_observation(peer, observation));
        let object = weak.upgrade().expect("the active attempt owns its own lease");
        assert!(matches!(lane.failed(&manager, &object, peer, same, same, "failure").as_slice(),
            [Action::ConnectResult { id: observed, .. }] if *observed == id));
        drop(object);
        assert!(lane.complete_connect_result(peer, id));
        assert!(matches!(lane.complete_terminal(peer, id), Ok(None)));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn terminal_receipt_retains_the_exact_object_until_delivery() {
        let manager = Rc::new(1);
        let old = Rc::new(2);
        let new = Rc::new(3);
        let weak_old = Rc::downgrade(&old);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let peer = Uuid::from_u128(1);
        let (old_id, first) = lane
            .connect(peer, old.clone(), "old", "old reply", same)
            .unwrap();
        assert!(matches!(first, Some(Action::Connect { id, .. }) if id == old_id));
        let (new_id, queued) = lane
            .connect(peer, new.clone(), "new", "new reply", same)
            .unwrap();
        assert!(queued.is_none());
        let effects = lane.failed(&manager, &old, peer, same, same, "error");
        assert!(
            matches!(effects.as_slice(), [Action::ConnectResult { id, owner: "old", reply: "old reply", .. }] if *id == old_id)
        );
        drop(old);
        assert!(weak_old.upgrade().is_some());
        assert!(matches!(
            lane.complete_terminal(peer, new_id),
            Err(TerminalReceiptError::NotTerminal)
        ));
        assert!(matches!(
            lane.complete_terminal(peer, old_id),
            Err(TerminalReceiptError::ConnectResultPending)
        ));
        assert!(weak_old.upgrade().is_some());
        assert!(lane.complete_connect_result(peer, old_id));
        assert!(
            matches!(lane.complete_terminal(peer, old_id), Ok(Some(Action::Connect { id, .. })) if id == new_id)
        );
        assert!(weak_old.upgrade().is_none());
        assert!(
            matches!(lane.connected(&manager, &new, peer, same, same).as_slice(), [Action::ConnectResult { id, owner: "new", .. }] if *id == new_id)
        );
    }

    #[test]
    fn callback_identity_and_queued_cancellation_do_not_release_active_lease() {
        let manager = Rc::new(1);
        let other_manager = Rc::new(1);
        let old = Rc::new(2);
        let new = Rc::new(2);
        let weak_new = Rc::downgrade(&new);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let peer = Uuid::from_u128(2);
        let (old_id, _) = lane.connect(peer, old.clone(), "old", "a", same).unwrap();
        let (new_id, _) = lane.connect(peer, new.clone(), "new", "b", same).unwrap();
        assert!(matches!(
            lane.connected(&other_manager, &old, peer, same, same)
                .as_slice(),
            [Action::Unassociated { .. }]
        ));
        assert!(matches!(
            lane.connected(&manager, &new, peer, same, same).as_slice(),
            [Action::Unassociated { .. }]
        ));
        assert!(
            matches!(lane.cancel(peer, new_id), Some(Action::CancelledQueued { id, reply: "b" }) if id == new_id)
        );
        drop(new);
        assert!(weak_new.upgrade().is_none());
        assert!(
            matches!(lane.connected(&manager, &old, peer, same, same).as_slice(), [Action::ConnectResult { id, owner: "old", .. }] if *id == old_id)
        );
    }

    #[test]
    fn shared_object_remains_retained_for_the_queued_cycle() {
        let manager = Rc::new(1);
        let object = Rc::new(2);
        let weak = Rc::downgrade(&object);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let peer = Uuid::from_u128(3);
        let (first, _) = lane
            .connect(peer, object.clone(), "first", "a", same)
            .unwrap();
        let (second, _) = lane
            .connect(peer, object.clone(), "second", "b", same)
            .unwrap();
        let effects = lane.failed(&manager, &object, peer, same, same, "failure");
        assert!(matches!(effects.as_slice(), [Action::ConnectResult { id, .. }] if *id == first));
        drop(object);
        assert!(lane.complete_connect_result(peer, first));
        let next = lane.complete_terminal(peer, first);
        assert!(matches!(next, Ok(Some(Action::Connect { id, .. })) if id == second));
        let object = weak.upgrade().expect("queued cycle owns its own lease");
        assert!(
            matches!(lane.failed(&manager, &object, peer, same, same, "failure").as_slice(), [Action::ConnectResult { id, .. }] if *id == second)
        );
        drop(object);
        assert!(lane.complete_connect_result(peer, second));
        assert!(matches!(lane.complete_terminal(peer, second), Ok(None)));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn disconnected_error_and_replies_belong_to_the_dispatched_cycle() {
        let manager = Rc::new(1);
        let object = Rc::new(2);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let peer = Uuid::from_u128(4);
        let (id, _) = lane
            .connect(peer, object.clone(), "owner", "connect", same)
            .unwrap();
        assert!(
            matches!(lane.disconnect(peer, id, "disconnect"), Ok(Some(Action::Cancel { id: observed, .. })) if observed == id)
        );
        assert!(
            matches!(lane.disconnected(&manager, &object, peer, same, same, Some("native error")).as_slice(),
            [Action::DisconnectedWhileConnecting { id: observed, owner: "owner", reply: "connect", error: Some("native error"), disconnect_replies }]
                if *observed == id && disconnect_replies.as_slice() == ["disconnect"])
        );
        assert!(matches!(lane.complete_terminal(peer, id), Ok(None)));
    }

    #[test]
    fn disconnect_lookup_selects_only_the_exact_active_owner() {
        let manager = Rc::new(1);
        let old_owner = Rc::new(10);
        let replacement_owner = Rc::new(10);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let peer = Uuid::from_u128(41);
        let (old, _) = lane
            .connect(peer, Rc::new(2), old_owner.clone(), "old", same)
            .unwrap();
        let (replacement, _) = lane
            .connect(peer, Rc::new(3), replacement_owner.clone(), "new", same)
            .unwrap();

        assert_eq!(
            lane.active_cycle_for_owner(peer, &old_owner, same),
            Some(old)
        );
        assert_eq!(
            lane.active_cycle_for_owner(peer, &replacement_owner, same),
            None
        );
        assert_eq!(
            lane.active_cycle_for_owner(Uuid::from_u128(42), &old_owner, same),
            None
        );

        let token = match lane.cancel(peer, old).unwrap() {
            Action::Cancel { object, .. } => object,
            _ => panic!("active cancellation must dispatch"),
        };
        let object = lane.resolve(peer, token).unwrap().clone();
        assert!(
            matches!(lane.disconnected(&manager, &object, peer, same, same, None).as_slice(),
            [Action::DisconnectedWhileConnecting { id, .. }] if *id == old)
        );
        assert_eq!(
            lane.active_cycle_for_owner(peer, &old_owner, same),
            None,
            "a terminal cycle cannot accept a disconnect"
        );
        assert!(
            matches!(lane.complete_terminal(peer, old), Ok(Some(Action::Connect { id, .. })) if id == replacement)
        );
        assert_eq!(
            lane.active_cycle_for_owner(peer, &replacement_owner, same),
            Some(replacement)
        );
        assert_eq!(lane.active_cycle_for_owner(peer, &old_owner, same), None);
    }

    #[test]
    fn terminal_receipt_waits_for_the_exact_connect_result_receipt() {
        let manager = Rc::new(1);
        let object = Rc::new(2);
        let replacement = Rc::new(3);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let peer = Uuid::from_u128(5);
        let other_peer = Uuid::from_u128(6);
        let (old, _) = lane
            .connect(peer, object.clone(), "old", "connect", same)
            .unwrap();
        let (new, _) = lane
            .connect(peer, replacement, "new", "next", same)
            .unwrap();
        assert!(matches!(
            lane.disconnect(peer, old, "disconnect"),
            Ok(Some(Action::Cancel { .. }))
        ));
        assert!(
            matches!(lane.failed(&manager, &object, peer, same, same, "native failure").as_slice(),
            [Action::ConnectResult { id, result: Err("native failure"), .. }] if *id == old)
        );
        assert!(
            matches!(lane.disconnected(&manager, &object, peer, same, same, Some("link lost")).as_slice(),
            [Action::Disconnected { id, error: Some("link lost"), .. }] if *id == old)
        );

        assert!(
            matches!(
                lane.complete_terminal(peer, old),
                Err(TerminalReceiptError::ConnectResultPending)
            ),
            "terminal delivery cannot overtake the connect result"
        );
        assert!(!lane.complete_connect_result(other_peer, old));
        assert!(!lane.complete_connect_result(peer, new));
        assert!(lane.complete_connect_result(peer, old));
        assert!(
            !lane.complete_connect_result(peer, old),
            "duplicate receipt must have no effect"
        );
        assert!(
            matches!(lane.complete_terminal(peer, old), Ok(Some(Action::Connect { id, .. })) if id == new)
        );
    }

    #[test]
    fn held_connect_result_does_not_block_another_peer() {
        let manager = Rc::new(1);
        let first_object = Rc::new(2);
        let second_object = Rc::new(3);
        let first_weak = Rc::downgrade(&first_object);
        let second_weak = Rc::downgrade(&second_object);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let first_peer = Uuid::from_u128(7);
        let second_peer = Uuid::from_u128(8);
        let (first, _) = lane
            .connect(first_peer, first_object.clone(), "first", "one", same)
            .unwrap();
        let (second, _) = lane
            .connect(second_peer, second_object.clone(), "second", "two", same)
            .unwrap();
        assert!(
            matches!(lane.failed(&manager, &first_object, first_peer, same, same, "first failure").as_slice(),
            [Action::ConnectResult { id, .. }] if *id == first)
        );
        assert!(
            matches!(lane.failed(&manager, &second_object, second_peer, same, same, "second failure").as_slice(),
            [Action::ConnectResult { id, .. }] if *id == second)
        );
        drop(first_object);
        drop(second_object);
        assert!(lane.complete_connect_result(second_peer, second));
        assert!(matches!(
            lane.complete_terminal(second_peer, second),
            Ok(None)
        ));
        assert!(second_weak.upgrade().is_none());
        assert!(matches!(
            lane.complete_terminal(first_peer, first),
            Err(TerminalReceiptError::ConnectResultPending)
        ));
        assert!(first_weak.upgrade().is_some());
        assert!(lane.complete_connect_result(first_peer, first));
        assert!(matches!(
            lane.complete_terminal(first_peer, first),
            Ok(None)
        ));
        assert!(first_weak.upgrade().is_none());
    }

    #[test]
    fn queue_shutdown_settles_active_and_queued_requests_then_retires_their_native_objects() {
        let manager = Rc::new(1);
        let active = Rc::new(2);
        let queued = Rc::new(3);
        let weak_active = Rc::downgrade(&active);
        let weak_queued = Rc::downgrade(&queued);
        let peer = Uuid::from_u128(902);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager);
        lane.connect(peer, active.clone(), "active", "connect-active", same).unwrap();
        lane.connect(peer, queued.clone(), "queued", "connect-queued", same).unwrap();
        assert!(matches!(lane.disconnect(peer, 1, "disconnect-active"), Ok(Some(Action::Cancel { .. }))));
        drop(active);
        drop(queued);

        let actions = lane.abort_all("queue closed");
        assert!(matches!(actions.as_slice(), [
            Action::DisconnectedWhileConnecting { owner: "active", reply: "connect-active", error: Some("queue closed"), disconnect_replies, .. },
            Action::CancelledQueued { reply: "connect-queued", .. },
        ] if *disconnect_replies == ["disconnect-active"]));
        assert!(weak_active.upgrade().is_none());
        assert!(weak_queued.upgrade().is_none());
    }

    #[test]
    fn callback_executor_delivers_the_old_owner_before_starting_the_successor() {
        use std::cell::RefCell;

        let manager = Rc::new(1);
        let old = Rc::new(2);
        let replacement = Rc::new(3);
        let old_weak = Rc::downgrade(&old);
        let peer = Uuid::from_u128(903);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let (old_id, _) = lane.connect(peer, old.clone(), "old", "old-connect", same).unwrap();
        let (replacement_id, _) = lane.connect(peer, replacement.clone(), "replacement", "replacement-connect", same).unwrap();
        let cancellation = match lane.disconnect(peer, old_id, "old-disconnect") {
            Ok(action) => action,
            Err(_) => panic!("active disconnect must be admitted"),
        };

        // The callback object is the only object reference outside the lane;
        // the caller's old object is gone before terminal delivery.
        let callback_object = lane.resolve(peer, match cancellation {
            Some(Action::Cancel { object, .. }) => object,
            _ => panic!("active disconnect must retain the exact native object"),
        }).unwrap().clone();
        drop(old);

        let order = Rc::new(RefCell::new(Vec::new()));
        let delivered = Rc::new(RefCell::new(Vec::new()));
        let starts = Rc::new(RefCell::new(Vec::new()));
        let order_for_connect = order.clone();
        let order_for_disconnect = order.clone();
        let order_for_lifecycle = order.clone();
        let order_for_start = order.clone();
        let delivered_for_lifecycle = delivered.clone();
        let starts_for_start = starts.clone();

        execute_disconnect_callback(
            &mut lane,
            &manager,
            &callback_object,
            peer,
            same,
            same,
            Some("link-lost"),
            move |reply, error| {
                order_for_connect.borrow_mut().push("connect-reply");
                assert_eq!(reply, "old-connect");
                assert_eq!(error, Some("link-lost"));
            },
            move |replies, error| {
                order_for_disconnect.borrow_mut().push("disconnect-reply");
                assert_eq!(replies, vec!["old-disconnect"]);
                assert_eq!(error, Some("link-lost"));
            },
            move |owner, error| {
                order_for_lifecycle.borrow_mut().push("old-lifecycle");
                delivered_for_lifecycle.borrow_mut().push((*owner, error));
                true
            },
            move |_, object| {
                order_for_start.borrow_mut().push("replacement-start");
                starts_for_start.borrow_mut().push(**object);
            },
            |_| panic!("associated callback must not become unassociated"),
        );
        drop(callback_object);

        assert_eq!(
            &*order.borrow(),
            &["connect-reply", "disconnect-reply", "old-lifecycle", "replacement-start"],
            "the executor must retain and deliver the old destination before dispatching its successor"
        );
        assert_eq!(&*delivered.borrow(), &[("old", Some("link-lost"))]);
        assert_eq!(&*starts.borrow(), &[3]);
        assert!(old_weak.upgrade().is_none(), "old native identity retires after terminal delivery");
        assert_eq!(lane.active_cycle_for_owner(peer, &"replacement", |a, b| a == b), Some(replacement_id));
    }

    #[test]
    fn callback_executor_does_not_start_successor_when_old_lifecycle_is_refused() {
        use std::cell::Cell;

        let manager = Rc::new(1);
        let old = Rc::new(2);
        let replacement = Rc::new(3);
        let peer = Uuid::from_u128(904);
        let mut lane = NativeLane::<_, _, _, _, &'static str>::new(manager.clone());
        let (_old_id, _) = lane.connect(peer, old.clone(), "old", "old-connect", same).unwrap();
        let (_replacement_id, _) = lane.connect(peer, replacement, "replacement", "replacement-connect", same).unwrap();
        let callback_object = old.clone();
        let started = Cell::new(false);

        execute_disconnect_callback(
            &mut lane, &manager, &callback_object, peer, same, same, None,
            |_reply, _error| {},
            |_replies, _error| {},
            |_owner, _error| false,
            |_, _| started.set(true),
            |_| panic!("associated callback must not become unassociated"),
        );

        assert!(!started.get());
        assert_eq!(lane.active_cycle_for_owner(peer, &"replacement", |a, b| a == b), None);
        assert!(lane.active_cycle_for_owner(peer, &"old", |a, b| a == b).is_none(),
            "the terminal old cycle cannot accept another operation while delivery is refused");
    }

    #[test]
    fn callback_executor_surfaces_unassociated_callback_to_its_destination() {
        let manager = Rc::new(1);
        let unknown = Rc::new(2);
        let peer = Uuid::from_u128(905);
        let mut lane = NativeLane::<_, _, (), (), &'static str>::new(manager.clone());
        let observed = std::cell::RefCell::new(None);

        execute_disconnect_callback(
            &mut lane, &manager, &unknown, peer, same, same, Some("late"),
            |_, _| panic!("unassociated callback cannot settle a connect reply"),
            |_, _| panic!("unassociated callback cannot settle a disconnect reply"),
            |_, _| panic!("unassociated callback cannot deliver a lifecycle"),
            |_, _| panic!("unassociated callback cannot start a successor"),
            |error| *observed.borrow_mut() = error,
        );
        assert_eq!(*observed.borrow(), Some("late"));
    }
}
