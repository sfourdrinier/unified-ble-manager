//! Queue-confined connection-attempt ledger, pending integration with the
//! manager-lifetime executor. This module does not yet dispatch native calls or
//! own actor retirement, reset cuts, native dispatch, or delivery recovery.
//! In particular, returning an action does not itself make delivery durable.
//! The chosen local fences do not establish Apple's ordering of late central
//! callbacks on the same manager and peripheral after a subsequent attempt.

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn same(a: &Rc<u8>, b: &Rc<u8>) -> bool {
        Rc::ptr_eq(a, b)
    }
    type Ledger = NativeAttempts<u8, Rc<u8>, Rc<u8>, &'static str, &'static str, &'static str>;

    #[test]
    fn terminal_delivery_must_finish_before_replacement_dispatch() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "old-reply");
        lanes.connect(1, Rc::new(2), 2, "new", "new-reply");
        let effects = lanes.failed(&manager, same, &1, &object, same, "failure");
        assert!(matches!(effects.as_slice(), [Action::ConnectResult { id: 1, .. }]));
        assert!(matches!(lanes.connected(&manager, same, &1, &object, same).as_slice(), [Action::Unassociated { .. }]));
        assert!(lanes.complete_terminal(&1, 99).is_none());
        assert!(matches!(lanes.complete_terminal(&1, 1), Some(Action::Connect { id: 2, .. })));
        assert!(lanes.complete_terminal(&1, 1).is_none());
    }

    #[test]
    fn held_terminal_does_not_block_other_peers_or_queued_cancellation() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "old-reply");
        lanes.connect(1, Rc::new(2), 2, "queued", "queued-reply");
        assert!(matches!(lanes.disconnected(&manager, same, &1, &object, same, None).as_slice(),
            [Action::DisconnectedWhileConnecting { id: 1, .. }]));
        assert!(matches!(lanes.cancel(&1, 2), Some(Action::CancelledQueued { id: 2, reply: "queued-reply" })));
        assert!(matches!(lanes.connect(2, Rc::new(3), 3, "other", "other-reply"),
            Some(Action::Connect { id: 3, .. })));
        assert!(lanes.complete_terminal(&1, 1).is_none());
    }

    #[test]
    fn replacement_waits_and_result_belongs_to_dispatched_owner() {
        let manager = Rc::new(9);
        let mut lanes = Ledger::new(manager.clone());
        let a = Rc::new(1);
        let b = Rc::new(2);
        assert!(matches!(
            lanes.connect(1, a.clone(), 11, "old", "a"),
            Some(Action::Connect { id: 11, .. })
        ));
        assert!(lanes.connect(1, b.clone(), 12, "new", "b").is_none());
        let actions = lanes.connected(&manager, same, &1, &a, same);
        assert!(matches!(
            actions.as_slice(),
            [Action::ConnectResult {
                id: 11,
                owner: "old",
                reply: "a",
                result: Ok(())
            }]
        ));
        assert!(matches!(
            lanes.cancel(&1, 11),
            Some(Action::Cancel { id: 11, .. })
        ));
        let actions = lanes.disconnected(&manager, same, &1, &a, same, None);
        assert!(matches!(
            actions.as_slice(),
            [
                Action::Disconnected {
                    id: 11,
                    owner: "old",
                    ..
                }
            ]
        ));
        assert!(matches!(lanes.complete_terminal(&1, 11), Some(Action::Connect { id: 12, .. })));
        assert!(matches!(
            lanes.connected(&manager, same, &1, &b, same).as_slice(),
            [Action::ConnectResult {
                id: 12,
                owner: "new",
                reply: "b",
                result: Ok(())
            }]
        ));
    }

    #[test]
    fn cancelled_connect_preserves_failure_until_disconnect_fence() {
        let manager = Rc::new(9);
        let mut lanes = Ledger::new(manager.clone());
        let a = Rc::new(1);
        let b = Rc::new(2);
        lanes.connect(1, a.clone(), 1, "old", "a");
        lanes.connect(1, b, 2, "new", "b");
        assert!(matches!(lanes.cancel(&1, 1), Some(Action::Cancel { .. })));
        assert!(matches!(
            lanes
                .failed(&manager, same, &1, &a, same, "native-error")
                .as_slice(),
            [Action::ConnectResult {
                id: 1,
                owner: "old",
                reply: "a",
                result: Err("native-error")
            }]
        ));
        assert!(matches!(
            lanes
                .disconnected(&manager, same, &1, &a, same, None)
                .as_slice(),
            [
                Action::Disconnected {
                    id: 1,
                    owner: "old",
                    ..
                }
            ]
        ));
        assert!(matches!(lanes.complete_terminal(&1, 1), Some(Action::Connect { id: 2, .. })));
    }

    #[test]
    fn disconnect_waiters_stay_with_the_old_cycle_until_its_callback() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let replacement = Rc::new(2);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "connect-old");
        lanes.connect(1, replacement, 2, "new", "connect-new");
        assert!(matches!(
            lanes.disconnect(&1, 1, "disconnect-old"),
            Ok(Some(Action::Cancel { id: 1, .. }))
        ));
        assert!(matches!(
            lanes.disconnect(&1, 1, "disconnect-again"),
            Ok(None)
        ));
        assert!(matches!(
            lanes
                .failed(&manager, same, &1, &object, same, "connect-failure")
                .as_slice(),
            [Action::ConnectResult {
                id: 1,
                result: Err("connect-failure"),
                ..
            }]
        ));
        assert!(
            matches!(lanes.disconnected(&manager, same, &1, &object, same, Some("disconnect-error")).as_slice(),
            [Action::Disconnected { id: 1, owner: "old", error: Some("disconnect-error"), disconnect_replies }]
            if disconnect_replies == &["disconnect-old", "disconnect-again"])
        );
        assert!(matches!(lanes.complete_terminal(&1, 1), Some(Action::Connect { id: 2, .. })));
    }

    #[test]
    fn disconnect_before_connect_result_preserves_both_replies_and_error() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "connect");
        assert!(matches!(
            lanes.disconnect(&1, 1, "disconnect"),
            Ok(Some(Action::Cancel { .. }))
        ));
        assert!(
            matches!(lanes.disconnected(&manager, same, &1, &object, same, Some("native" )).as_slice(),
            [Action::DisconnectedWhileConnecting { id: 1, owner: "old", reply: "connect", error: Some("native"), disconnect_replies }]
            if disconnect_replies == &["disconnect"])
        );
    }

    #[test]
    fn disconnect_refusal_returns_the_exact_reply_without_cancelling_another_cycle() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "connect");
        assert!(matches!(
            lanes.disconnect(&1, 2, "foreign"),
            Err(DisconnectError::UnknownCycle("foreign"))
        ));
        assert!(matches!(
            lanes
                .connected(&manager, same, &1, &object, same)
                .as_slice(),
            [Action::ConnectResult {
                id: 1,
                result: Ok(()),
                ..
            }]
        ));
    }

    #[test]
    fn unknown_object_does_not_claim_owner_or_release_lane() {
        let manager = Rc::new(9);
        let mut lanes = Ledger::new(manager.clone());
        let a = Rc::new(1);
        let unknown = Rc::new(1);
        lanes.connect(1, a.clone(), 1, "old", "a");
        lanes.connect(1, Rc::new(2), 2, "new", "b");
        assert!(matches!(
            lanes
                .connected(&manager, same, &1, &unknown, same)
                .as_slice(),
            [Action::Unassociated { .. }]
        ));
        assert!(matches!(
            lanes
                .failed(&manager, same, &1, &unknown, same, "bad")
                .as_slice(),
            [Action::Unassociated { .. }]
        ));
        assert!(matches!(
            lanes.connected(&manager, same, &1, &a, same).as_slice(),
            [Action::ConnectResult {
                id: 1,
                owner: "old",
                ..
            }]
        ));
    }

    #[test]
    fn callback_from_another_manager_cannot_claim_the_attempt() {
        let manager = Rc::new(9);
        let other_manager = Rc::new(9);
        let object = Rc::new(1);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "a");
        assert!(matches!(
            lanes
                .connected(&other_manager, same, &1, &object, same)
                .as_slice(),
            [Action::Unassociated { .. }]
        ));
        assert!(matches!(
            lanes
                .connected(&manager, same, &1, &object, same)
                .as_slice(),
            [Action::ConnectResult { id: 1, .. }]
        ));
    }

    #[test]
    fn unassociated_failure_preserves_the_platform_error() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let mut lanes = Ledger::new(manager.clone());
        lanes.connect(1, object.clone(), 1, "old", "a");
        let other = Rc::new(1);
        assert!(matches!(
            lanes
                .failed(&manager, same, &1, &other, same, "native-error")
                .as_slice(),
            [Action::Unassociated {
                error: Some("native-error")
            }]
        ));
        assert!(matches!(
            lanes
                .connected(&manager, same, &1, &object, same)
                .as_slice(),
            [Action::ConnectResult { id: 1, .. }]
        ));
    }

    #[test]
    fn queued_cancellation_does_not_cancel_active_peer_and_other_peer_progresses() {
        let manager = Rc::new(9);
        let mut lanes = Ledger::new(manager.clone());
        let a = Rc::new(1);
        let other = Rc::new(3);
        lanes.connect(1, a, 1, "old", "a");
        lanes.connect(1, Rc::new(2), 2, "new", "b");
        assert!(matches!(
            lanes.cancel(&1, 2),
            Some(Action::CancelledQueued { id: 2, reply: "b" })
        ));
        assert!(matches!(
            lanes.connect(2, other.clone(), 3, "other", "c"),
            Some(Action::Connect { id: 3, .. })
        ));
        assert!(matches!(
            lanes.connected(&manager, same, &2, &other, same).as_slice(),
            [Action::ConnectResult { id: 3, .. }]
        ));
    }

    #[test]
    fn manager_and_dispatched_object_survive_until_the_lane_finishes() {
        let manager = Rc::new(9);
        let object = Rc::new(1);
        let weak_manager = Rc::downgrade(&manager);
        let weak_object = Rc::downgrade(&object);
        let mut lanes = Ledger::new(manager.clone());
        drop(lanes.connect(1, object.clone(), 1, "old", "a"));
        drop(manager);
        drop(object);
        let manager = weak_manager.upgrade().expect("manager retained by ledger");
        let object = weak_object
            .upgrade()
            .expect("dispatched object retained by cycle");
        assert!(matches!(
            lanes
                .failed(&manager, same, &1, &object, same, "failure")
                .as_slice(),
            [Action::ConnectResult {
                id: 1,
                result: Err("failure"),
                ..
            }]
        ));
        drop(object);
        assert!(weak_object.upgrade().is_some());
        assert!(lanes.complete_terminal(&1, 1).is_none());
        assert!(weak_object.upgrade().is_none());
        drop(manager);
        drop(lanes);
        assert!(weak_manager.upgrade().is_none());
    }
}

use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    marker::PhantomData,
    rc::Rc,
};

// This ledger is deliberately !Send and !Sync. The manager, objects and all
// operations must stay on the delegate's serial queue. Returned actions are
// executed by the caller only after its borrow of the ledger has ended.
#[allow(dead_code)]
pub(super) enum Action<N, O, R, E> {
    Connect {
        id: u64,
        object: N,
    },
    Cancel {
        id: u64,
        object: N,
    },
    CancelledQueued {
        id: u64,
        reply: R,
    },
    ConnectResult {
        id: u64,
        owner: O,
        reply: R,
        result: Result<(), E>,
    },
    Disconnected {
        id: u64,
        owner: O,
        error: Option<E>,
        disconnect_replies: Vec<R>,
    },
    DisconnectedWhileConnecting {
        id: u64,
        owner: O,
        reply: R,
        error: Option<E>,
        disconnect_replies: Vec<R>,
    },
    Unassociated {
        error: Option<E>,
    },
}

pub(super) enum DisconnectError<R> {
    UnknownCycle(R),
    Capacity(R),
}

struct Request<N, O, R> {
    object: N,
    id: u64,
    owner: O,
    reply: R,
}
struct Cycle<N, O, R> {
    object: N,
    id: u64,
    owner: O,
    reply: Option<R>,
    cancel_sent: bool,
    connected: bool,
    terminal: bool,
    disconnect_replies: Vec<R>,
}
struct Lane<N, O, R> {
    active: Cycle<N, O, R>,
    queued: VecDeque<Request<N, O, R>>,
}

#[allow(dead_code)]
pub(super) struct NativeAttempts<K, M, N, O, R, E> {
    manager: M,
    lanes: HashMap<K, Lane<N, O, R>>,
    _local: PhantomData<(Rc<()>, E)>,
}

#[allow(dead_code)]
impl<K: Eq + Hash, M, N: Clone, O: Clone, R, E> NativeAttempts<K, M, N, O, R, E> {
    pub(super) fn new(manager: M) -> Self {
        Self {
            manager,
            lanes: HashMap::new(),
            _local: PhantomData,
        }
    }
    // IDs are caller-provided identities for cancellation and terminal
    // receipts, never evidence for attributing a native callback. The executor
    // must not reuse them during its lifetime: an old delivery receipt must
    // never acknowledge a later cycle.
    pub(super) fn connect(
        &mut self,
        peer: K,
        object: N,
        id: u64,
        owner: O,
        reply: R,
    ) -> Option<Action<N, O, R, E>> {
        let request = Request {
            object,
            id,
            owner,
            reply,
        };
        if let Some(lane) = self.lanes.get_mut(&peer) {
            assert!(
                lane.active.id != id && !lane.queued.iter().any(|item| item.id == id),
                "duplicate native attempt identity"
            );
            lane.queued.push_back(request);
            return None;
        }
        let (active, action) = Self::start(request);
        self.lanes.insert(
            peer,
            Lane {
                active,
                queued: VecDeque::new(),
            },
        );
        Some(action)
    }

    fn start(request: Request<N, O, R>) -> (Cycle<N, O, R>, Action<N, O, R, E>) {
        let action = Action::Connect {
            id: request.id,
            object: request.object.clone(),
        };
        (
            Cycle {
                object: request.object,
                id: request.id,
                owner: request.owner,
                reply: Some(request.reply),
                cancel_sent: false,
                connected: false,
                terminal: false,
                disconnect_replies: Vec::new(),
            },
            action,
        )
    }

    pub(super) fn cancel(&mut self, peer: &K, id: u64) -> Option<Action<N, O, R, E>> {
        let lane = self.lanes.get_mut(peer)?;
        if lane.active.id == id {
            if lane.active.terminal { return None; }
            if lane.active.cancel_sent {
                return None;
            }
            lane.active.cancel_sent = true;
            return Some(Action::Cancel {
                id,
                object: lane.active.object.clone(),
            });
        }
        let position = lane.queued.iter().position(|request| request.id == id)?;
        let request = lane
            .queued
            .remove(position)
            .expect("position came from queue");
        Some(Action::CancelledQueued {
            id,
            reply: request.reply,
        })
    }

    /// The owner comparison is supplied by the executor because ownership is
    /// an identity, not value equality. Never select a queued or terminal cycle.
    pub(super) fn active_cycle_for_owner(
        &self,
        peer: &K,
        owner: &O,
        same_owner: impl Fn(&O, &O) -> bool,
    ) -> Option<u64> {
        let cycle = &self.lanes.get(peer)?.active;
        (!cycle.terminal && same_owner(&cycle.owner, owner)).then_some(cycle.id)
    }

    /// A disconnect belongs to the active native cycle, even if its logical
    /// owner has retired. Cancellation must not wait behind that cycle.
    pub(super) fn disconnect(
        &mut self,
        peer: &K,
        id: u64,
        reply: R,
    ) -> Result<Option<Action<N, O, R, E>>, DisconnectError<R>> {
        let Some(lane) = self.lanes.get_mut(peer) else {
            return Err(DisconnectError::UnknownCycle(reply));
        };
        if lane.active.id != id || lane.active.terminal {
            return Err(DisconnectError::UnknownCycle(reply));
        }
        if lane.active.disconnect_replies.len() >= 256 {
            return Err(DisconnectError::Capacity(reply));
        }
        lane.active.disconnect_replies.push(reply);
        if lane.active.cancel_sent {
            return Ok(None);
        }
        lane.active.cancel_sent = true;
        Ok(Some(Action::Cancel {
            id,
            object: lane.active.object.clone(),
        }))
    }

    pub(super) fn connected(
        &mut self,
        manager: &M,
        same_manager: impl Fn(&M, &M) -> bool,
        peer: &K,
        object: &N,
        same: impl Fn(&N, &N) -> bool,
    ) -> Vec<Action<N, O, R, E>> {
        if !same_manager(&self.manager, manager) {
            return vec![Action::Unassociated { error: None }];
        }
        let Some(lane) = self.lanes.get_mut(peer) else {
            return vec![Action::Unassociated { error: None }];
        };
        let cycle = &mut lane.active;
        if !same(&cycle.object, object) || cycle.connected || cycle.terminal {
            return vec![Action::Unassociated { error: None }];
        }
        let Some(reply) = cycle.reply.take() else {
            return vec![Action::Unassociated { error: None }];
        };
        cycle.connected = true;
        vec![Action::ConnectResult {
            id: cycle.id,
            owner: cycle.owner.clone(),
            reply,
            result: Ok(()),
        }]
    }

    pub(super) fn failed(
        &mut self,
        manager: &M,
        same_manager: impl Fn(&M, &M) -> bool,
        peer: &K,
        object: &N,
        same: impl Fn(&N, &N) -> bool,
        error: E,
    ) -> Vec<Action<N, O, R, E>> {
        if !same_manager(&self.manager, manager) {
            return vec![Action::Unassociated { error: Some(error) }];
        }
        let Some(lane) = self.lanes.get_mut(peer) else {
            return vec![Action::Unassociated { error: Some(error) }];
        };
        let cycle = &mut lane.active;
        if !same(&cycle.object, object) || cycle.connected || cycle.terminal {
            return vec![Action::Unassociated { error: Some(error) }];
        }
        let Some(reply) = cycle.reply.take() else {
            return vec![Action::Unassociated { error: Some(error) }];
        };
        let result = Action::ConnectResult {
            id: cycle.id,
            owner: cycle.owner.clone(),
            reply,
            result: Err(error),
        };
        let cancelled = cycle.cancel_sent;
        let actions = vec![result];
        // A cancellation retains debt until disconnect, even if the connect
        // failure arrives first. An uncancelled failure is the local fence.
        if !cancelled {
            lane.active.terminal = true;
        }
        actions
    }

    pub(super) fn disconnected(
        &mut self,
        manager: &M,
        same_manager: impl Fn(&M, &M) -> bool,
        peer: &K,
        object: &N,
        same: impl Fn(&N, &N) -> bool,
        error: Option<E>,
    ) -> Vec<Action<N, O, R, E>> {
        if !same_manager(&self.manager, manager) {
            return vec![Action::Unassociated { error }];
        }
        let Some(lane) = self.lanes.get_mut(peer) else {
            return vec![Action::Unassociated { error }];
        };
        let cycle = &mut lane.active;
        if !same(&cycle.object, object) || cycle.terminal {
            return vec![Action::Unassociated { error }];
        }
        let event = if let Some(reply) = cycle.reply.take() {
            Action::DisconnectedWhileConnecting {
                id: cycle.id,
                owner: cycle.owner.clone(),
                reply,
                error,
                disconnect_replies: std::mem::take(&mut cycle.disconnect_replies),
            }
        } else {
            Action::Disconnected {
                id: cycle.id,
                owner: cycle.owner.clone(),
                error,
                disconnect_replies: std::mem::take(&mut cycle.disconnect_replies),
            }
        };
        cycle.terminal = true;
        vec![event]
    }

    /// Release a terminal cycle only after its captured effects have been
    /// delivered or durably handed to their exact destinations. A stale
    /// receipt cannot release a newer cycle.
    pub(super) fn complete_terminal(&mut self, peer: &K, id: u64) -> Option<Action<N, O, R, E>> {
        let lane = self.lanes.get_mut(peer)?;
        if lane.active.id != id || !lane.active.terminal { return None; }
        if let Some(next) = lane.queued.pop_front() {
            let (active, action) = Self::start(next);
            lane.active = active;
            Some(action)
        } else {
            self.lanes.remove(peer);
            None
        }
    }

    pub(super) fn is_terminal(&self, peer: &K, id: u64) -> bool {
        self.lanes.get(peer).is_some_and(|lane| lane.active.id == id && lane.active.terminal)
    }
}

impl<K: Eq + Hash, M, N: Clone, O: Clone, R, E: Clone> NativeAttempts<K, M, N, O, R, E> {
    /// Settle every queue-confined request when the delegate queue is closing.
    /// Native callbacks cannot be awaited after that point, so retaining an
    /// attempt would strand its connect or disconnect caller indefinitely.
    pub(super) fn abort_all(&mut self, error: E) -> Vec<Action<N, O, R, E>> {
        let lanes = std::mem::take(&mut self.lanes);
        let mut actions = Vec::new();
        for (_, mut lane) in lanes {
            let active = lane.active;
            if let Some(reply) = active.reply {
                actions.push(Action::DisconnectedWhileConnecting {
                    id: active.id,
                    owner: active.owner,
                    reply,
                    error: Some(error.clone()),
                    disconnect_replies: active.disconnect_replies,
                });
            } else {
                actions.push(Action::Disconnected {
                    id: active.id,
                    owner: active.owner,
                    error: Some(error.clone()),
                    disconnect_replies: active.disconnect_replies,
                });
            }
            actions.extend(lane.queued.drain(..).map(|request| Action::CancelledQueued {
                id: request.id,
                reply: request.reply,
            }));
        }
        actions
    }
}
