// vendor/btleplug/src/corebluetooth/peer_waiters.rs
//! Peer-owned callback waiters. The source owner serializes access with callbacks.

use std::collections::{HashMap, VecDeque};

use super::central_delegate::AttrKey;
use super::internal::{CoreBluetoothReply, CoreBluetoothReplyStateShared};
use super::read_notify::{enable_not_notifying_error, ReadNotifyState, ValueRoute};
use crate::PlatformError;

type Route = (AttrKey, AttrKey);

#[derive(Default)]
struct AttributeWaiters {
    reads: VecDeque<CoreBluetoothReplyStateShared>,
    enables: VecDeque<CoreBluetoothReplyStateShared>,
    disables: VecDeque<CoreBluetoothReplyStateShared>,
    cleanup: Option<(u64, CoreBluetoothReplyStateShared)>,
}


#[derive(Default)]
pub(super) struct PeerWaiters {
    attributes: HashMap<Route, AttributeWaiters>,
    next_cleanup: u64,
    pub(super) revision: u64,
    pub(super) revision_exhausted: bool,
    pub(super) ready: bool,
    pub(super) generation: Option<u64>,
    pub(super) callback_tickets: usize,
    pub(super) discovery_actor_pending: bool,
    pub(super) adapter_overflow_paused: bool,
    pub(super) adapter_tail_reported: bool,
    pub(super) overflow_cuts_queued: usize,
    // Lifecycle causes already admitted for the current logically retired graph.
    pub(super) lifecycle_causes: u8,
    pub(super) empty_invalidation_admitted: bool,
    pub(super) empty_source_failure_admitted: bool,
    pub(super) reclaim_operations: usize,
    pub(super) reclaim_uncertain: bool,
    pub(super) reclaim_reservation_failure: Option<crate::common::util::SourceGroupError>,
    pub(super) reclaim_held: bool,
    pub(super) reclaim_drain_active: bool,
    pub(super) reclaim_release_pending: Option<crate::ubm_reclaim_authority::GraphKey>,
    pub(super) reclaim_gate: Option<crate::ubm_reclaim_graph_gate::GraphGate<super::internal::DeferredPeerCallback>>,
    pub(super) reclaim_binding: crate::ubm_reclaim_graph_binding::GraphBindingOwner,
    pending_wakers: Vec<std::task::Waker>,
}

pub(super) type PreparedReply = (CoreBluetoothReplyStateShared, CoreBluetoothReply);

pub(super) struct PreparedNotifyState {
    pub(super) completion: Option<PreparedReply>,
    pub(super) cleanup: Option<u64>,
}

pub(super) fn complete_reply((waiter, reply): PreparedReply) {
    let (accepted, waker) = waiter.lock().unwrap().try_set_reply_deferred(reply);
    if !accepted { log::warn!("CoreBluetooth reply arrived after terminal completion"); }
    if let Some(waker) = waker { waker.wake(); }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WaiterError {
    CleanupPending,
    NoCleanupPending,
    CleanupIdExhausted,
}

fn answer(
    wakers: &mut Vec<std::task::Waker>,
    waiter: CoreBluetoothReplyStateShared,
    reply: CoreBluetoothReply,
) {
    let (accepted, waker) = waiter.lock().unwrap().try_set_reply_deferred(reply);
    if !accepted {
        log::warn!("CoreBluetooth reply arrived after terminal completion");
    }
    if let Some(waker) = waker {
        wakers.push(waker);
    }
}

impl PeerWaiters {
    pub(super) fn take_wakers(&mut self) -> Vec<std::task::Waker> {
        std::mem::take(&mut self.pending_wakers)
    }
    pub(super) fn read(&mut self, route: Route, waiter: CoreBluetoothReplyStateShared) {
        self.attributes
            .entry(route)
            .or_default()
            .reads
            .push_back(waiter);
    }

    pub(super) fn enable(
        &mut self,
        route: Route,
        waiter: CoreBluetoothReplyStateShared,
    ) -> Result<(), WaiterError> {
        let state = self.attributes.entry(route).or_default();
        if state.cleanup.is_some() || !state.disables.is_empty() {
            return Err(WaiterError::CleanupPending);
        }
        state.enables.push_back(waiter);
        Ok(())
    }

    pub(super) fn disable(
        &mut self,
        route: Route,
        waiter: CoreBluetoothReplyStateShared,
    ) -> Result<(), WaiterError> {
        let state = self.attributes.entry(route).or_default();
        if state.cleanup.is_some() || !state.disables.is_empty() {
            return Err(WaiterError::CleanupPending);
        }
        state.disables.push_back(waiter);
        Ok(())
    }

    pub(super) fn classify_value(&self, route: Route, notifying: bool) -> ValueRoute {
        let state = self.attributes.get(&route);
        ReadNotifyState {
            is_notifying: notifying,
            pending_reads: state.is_some_and(|state| !state.reads.is_empty()),
            pending_subscribe: state.is_some_and(|state| !state.enables.is_empty()),
            pending_unsubscribe: state.is_some_and(|state| !state.disables.is_empty() || state.cleanup.is_some()),
        }
        .route_value()
    }

    pub(super) fn reserve_value_read(&mut self, route: Route) -> Option<CoreBluetoothReplyStateShared> {
        self.attributes.get_mut(&route).and_then(|state| state.reads.pop_front())
    }

    pub(super) fn prepare_value(&mut self, route: Route, notifying: bool) -> (ValueRoute, Option<(CoreBluetoothReplyStateShared, crate::api::ReadProvenance)>) {
        let decision = self.classify_value(route, notifying);
        let read = decision.read.map(|provenance| {
            (self.reserve_value_read(route).expect("classified read remains owned under peer lock"), provenance)
        });
        (decision, read)
    }

    pub(super) fn value(&mut self, route: Route, notifying: bool, bytes: &[u8]) -> ValueRoute {
        let (decision, read) = self.prepare_value(route, notifying);
        if let Some((waiter, provenance)) = read {
            answer(&mut self.pending_wakers, waiter, CoreBluetoothReply::CharacteristicRead(bytes.to_vec(), provenance));
        }
        decision
    }

    pub(super) fn routes(&self) -> impl Iterator<Item = Route> + '_ {
        self.attributes.keys().copied()
    }

    pub(super) fn notification_possible(&self, route: Route, notifying: bool) -> bool {
        notifying
            || self.attributes.get(&route).is_some_and(|state| {
                !state.enables.is_empty() || !state.disables.is_empty() || state.cleanup.is_some()
            })
    }

    pub(super) fn prepare_value_error(&mut self, route: Route) -> Option<CoreBluetoothReplyStateShared> {
        self.reserve_value_read(route)
    }

    pub(super) fn value_error(&mut self, route: Route, error: PlatformError) -> bool {
        if let Some(waiter) = self.prepare_value_error(route) {
            answer(&mut self.pending_wakers, waiter, CoreBluetoothReply::Failed(error));
            true
        } else { false }
    }

    /// Select the exact callback-time waiter and install cleanup before replay.
    pub(super) fn prepare_notification_state(
        &mut self, route: Route, notifying: bool,
    ) -> Result<PreparedNotifyState, WaiterError> {
        let state = self.attributes.entry(route).or_default();
        let mut prepared = PreparedNotifyState { completion: None, cleanup: None };
        if notifying {
            if let Some(waiter) = state.enables.pop_front() {
                prepared.completion = Some((waiter, CoreBluetoothReply::Ok));
            } else if let Some(waiter) = state.disables.pop_front() {
                prepared.completion = Some((waiter, CoreBluetoothReply::Failed(PlatformError::new(
                    "corebluetooth", "notification-disable-still-notifying",
                    "CoreBluetooth reported that notifications remain enabled",
                ))));
            }
        } else if state.cleanup.is_none() {
            if !state.disables.is_empty() {
                let Some(next) = self.next_cleanup.checked_add(1) else {
                    return Err(WaiterError::CleanupIdExhausted);
                };
                let waiter = state.disables.pop_front().expect("checked under peer lock");
                let id = self.next_cleanup;
                self.next_cleanup = next;
                state.cleanup = Some((id, waiter));
                prepared.cleanup = Some(id);
            } else if let Some(waiter) = state.enables.pop_front() {
                prepared.completion = Some((waiter, CoreBluetoothReply::Failed(enable_not_notifying_error())));
            }
        }
        Ok(prepared)
    }

    pub(super) fn notification_state(&mut self, route: Route, notifying: bool) -> Result<Option<u64>, WaiterError> {
        let prepared = self.prepare_notification_state(route, notifying)?;
        if let Some((waiter, reply)) = prepared.completion {
            answer(&mut self.pending_wakers, waiter, reply);
        }
        Ok(prepared.cleanup)
    }

    pub(super) fn prepare_notification_error(&mut self, route: Route, error: PlatformError) -> Option<PreparedReply> {
        let state = self.attributes.entry(route).or_default();
        state.enables.pop_front().or_else(|| state.disables.pop_front())
            .map(|waiter| (waiter, CoreBluetoothReply::Failed(error)))
    }

    pub(super) fn notification_error(&mut self, route: Route, error: PlatformError) {
        if let Some((waiter, reply)) = self.prepare_notification_error(route, error) {
            answer(&mut self.pending_wakers, waiter, reply);
        }
    }

    pub(super) fn finish_cleanup(
        &mut self,
        route: Route,
        id: u64,
        result: Result<(), PlatformError>,
    ) -> Result<(), WaiterError> {
        let state = self.attributes.entry(route).or_default();
        if !state
            .cleanup
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            return Err(WaiterError::NoCleanupPending);
        }
        let Some((_, waiter)) = state.cleanup.take() else {
            return Err(WaiterError::NoCleanupPending);
        };
        answer(
            &mut self.pending_wakers,
            waiter,
            match result {
                Ok(()) => CoreBluetoothReply::Ok,
                Err(error) => CoreBluetoothReply::Failed(error),
            },
        );
        Ok(())
    }

    pub(super) fn prepare_invalidation(&mut self, error: PlatformError) -> Vec<PreparedReply> {
        let mut replies = Vec::new();
        for (_, mut state) in self.attributes.drain() {
            for waiter in state
                .reads
                .drain(..)
                .chain(state.enables.drain(..))
                .chain(state.disables.drain(..))
                .chain(state.cleanup.take().map(|(_, waiter)| waiter))
            {
                replies.push((waiter, CoreBluetoothReply::Failed(error.clone())));
            }
        }
        replies
    }

    pub(super) fn invalidation_waiter_count(&self) -> usize {
        self.attributes.values().map(|state| state.reads.len() + state.enables.len()
            + state.disables.len() + usize::from(state.cleanup.is_some())).sum()
    }

    pub(super) fn invalidate(&mut self, error: PlatformError) {
        for (waiter, reply) in self.prepare_invalidation(error) {
            answer(&mut self.pending_wakers, waiter, reply);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::internal::CoreBluetoothReplyFuture;
    use super::*;
    use crate::api::ReadProvenance;
    use uuid::Uuid;

    fn route(instance: u64) -> Route {
        (
            AttrKey {
                uuid: Uuid::nil(),
                instance: 0,
            },
            AttrKey {
                uuid: Uuid::nil(),
                instance,
            },
        )
    }
    fn error() -> PlatformError {
        PlatformError::new("corebluetooth", "closed", "closed")
    }

    #[tokio::test]
    async fn reads_preserve_route_order_and_ambiguous_provenance() {
        let mut owner = PeerWaiters::default();
        let first = CoreBluetoothReplyFuture::default();
        let second = CoreBluetoothReplyFuture::default();
        owner.read(route(1), first.get_state_clone());
        owner.read(route(2), second.get_state_clone());
        assert!(owner.value(route(1), true, &[1]).notification);
        assert!(!owner.value(route(2), false, &[2]).notification);
        assert!(
            matches!(first.await, CoreBluetoothReply::CharacteristicRead(v, ReadProvenance::ReadOrNotification) if v == [1])
        );
        assert!(
            matches!(second.await, CoreBluetoothReply::CharacteristicRead(v, ReadProvenance::ReadResponse) if v == [2])
        );
    }

    #[tokio::test]
    async fn disable_waits_for_cleanup_and_invalidation_is_exactly_once() {
        let mut owner = PeerWaiters::default();
        let disable = CoreBluetoothReplyFuture::default();
        owner.disable(route(1), disable.get_state_clone()).unwrap();
        let cleanup = owner.notification_state(route(1), false).unwrap().unwrap();
        let enable = CoreBluetoothReplyFuture::default();
        assert_eq!(
            owner.enable(route(1), enable.get_state_clone()),
            Err(WaiterError::CleanupPending)
        );
        owner
            .finish_cleanup(route(1), cleanup, Err(error()))
            .unwrap();
        assert!(matches!(disable.await, CoreBluetoothReply::Failed(_)));
        assert_eq!(
            owner.finish_cleanup(route(1), cleanup, Ok(())),
            Err(WaiterError::NoCleanupPending)
        );
        owner.enable(route(1), enable.get_state_clone()).unwrap();
        owner.invalidate(error());
        owner.invalidate(error());
        assert!(matches!(enable.await, CoreBluetoothReply::Failed(_)));
    }

    #[tokio::test]
    async fn disable_that_remains_notifying_fails_and_can_be_retried() {
        let mut owner = PeerWaiters::default();
        let first = CoreBluetoothReplyFuture::default();
        owner.disable(route(1), first.get_state_clone()).unwrap();
        assert_eq!(owner.notification_state(route(1), true).unwrap(), None);
        assert!(
            matches!(first.await, CoreBluetoothReply::Failed(error) if error.code == "notification-disable-still-notifying")
        );
        let retry = CoreBluetoothReplyFuture::default();
        owner.disable(route(1), retry.get_state_clone()).unwrap();
        let id = owner.notification_state(route(1), false).unwrap().unwrap();
        owner.finish_cleanup(route(1), id, Ok(())).unwrap();
        assert!(matches!(retry.await, CoreBluetoothReply::Ok));
    }

    #[tokio::test]
    async fn successful_cleanup_and_notification_failure_answer_waiters() {
        let mut owner = PeerWaiters::default();
        let enable = CoreBluetoothReplyFuture::default();
        owner.enable(route(1), enable.get_state_clone()).unwrap();
        owner.notification_error(route(1), error());
        assert!(matches!(enable.await, CoreBluetoothReply::Failed(_)));
        let disable = CoreBluetoothReplyFuture::default();
        owner.disable(route(1), disable.get_state_clone()).unwrap();
        let id = owner.notification_state(route(1), false).unwrap().unwrap();
        owner.finish_cleanup(route(1), id, Ok(())).unwrap();
        assert!(matches!(disable.await, CoreBluetoothReply::Ok));
    }

    #[tokio::test]
    async fn callbacks_and_closed_handoff_complete_only_owned_waiters() {
        let mut owner = PeerWaiters::default();
        let read = CoreBluetoothReplyFuture::default();
        owner.read(route(1), read.get_state_clone());
        assert!(owner.value_error(route(1), error()));
        assert!(!owner.value_error(route(1), error()));
        assert!(matches!(read.await, CoreBluetoothReply::Failed(_)));
        let enable = CoreBluetoothReplyFuture::default();
        owner.enable(route(1), enable.get_state_clone()).unwrap();
        owner.notification_state(route(1), true).unwrap();
        assert!(matches!(enable.await, CoreBluetoothReply::Ok));
        let disable = CoreBluetoothReplyFuture::default();
        owner.disable(route(1), disable.get_state_clone()).unwrap();
        let cleanup = owner.notification_state(route(1), false).unwrap().unwrap();
        owner.invalidate(error());
        assert!(matches!(disable.await, CoreBluetoothReply::Failed(_)));
        assert_eq!(
            owner.finish_cleanup(route(1), cleanup, Ok(())),
            Err(WaiterError::NoCleanupPending)
        );
    }
}

#[cfg(test)]
mod source_tests {
    use super::super::central_delegate::AttrKey;
    use super::super::internal::{CoreBluetoothReply, CoreBluetoothReplyFuture, PeerSourceOwner};
    use crate::api::ReadProvenance;
    use crate::ubm_peer_notification_ingress::{
        CallbackProvenance, IngressRecord, NotificationRoute, Poll, TerminalReason,
    };
    use uuid::Uuid;

    #[test]
    fn marker_actor_admission_fences_dispatch_and_replays_after_waiter_drop() {
        use crate::common::util::{
            marker_graph_identity, SourceGroupError, SourceWriteRequest, SourceWriteStart,
            SourceWriteStatus,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        let owner = PeerSourceOwner::new();
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 128).unwrap();
        owner.waiters.lock().unwrap().ready = true;
        let expected = (
            "corebluetooth".to_owned(),
            owner.graph_attachment.unwrap(),
            0,
        );
        let request = SourceWriteRequest {
            selector: Default::default(),
            id: 0,
            graph: marker_graph_identity(&expected),
            value: vec![1],
            write_type: crate::api::WriteType::WithResponse,
            route: NotificationRoute {
                service_uuid: Uuid::nil(),
                service_instance: 1,
                characteristic_uuid: Uuid::nil(),
                characteristic_instance: 2,
            },
        };
        let calls = AtomicUsize::new(0);
        let (pending, reserved) = group.reserve_marker_write(request.clone()).unwrap();
        assert!(reserved);
        assert_eq!(pending.status(), SourceWriteStatus::Pending);
        drop(pending);
        let SourceWriteStart::Initiated { receipt, wake, .. } = owner
            .start_marker_transaction(&group, &expected, request.clone(), || {
                assert!(owner.waiters.try_lock().is_err());
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap()
        else {
            panic!("expected dispatch")
        };
        for consumer in wake {
            consumer.wake();
        }
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Marker { id: 0, ordinal: 0 })
        ));
        assert!(matches!(owner.admit_adapter_barrier(1), crate::ubm_reclaim_graph_gate::BarrierAdmission::Drain));
        drop(receipt);
        let SourceWriteStart::Replay(receipt) = owner
            .start_marker_transaction::<()>(&group, &expected, request.clone(), || {
                panic!("redispatched")
            })
            .unwrap()
        else {
            panic!("expected replay")
        };
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |id| assert_eq!(id, Some(1)), |_| {}), Err(crate::ubm_reclaim_graph_gate::GateError::Paused)));
        owner.resume_adapter_barrier(1).unwrap();
        owner.drain_reclaim_callbacks(|_, _| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}, |_| {}).unwrap();
        receipt.settle(Ok(()));
        assert!(matches!(
            group
                .replay_marker_write(&request)
                .unwrap()
                .unwrap()
                .status(),
            SourceWriteStatus::Settled { .. }
        ));
        group.forget_marker_write(0).unwrap();
        let mut next = request;
        next.id = 1;
        owner.waiters.lock().unwrap().revision = 1;
        assert!(matches!(
            owner.start_marker_transaction::<()>(&group, &expected, next, || panic!(
                "stale dispatch"
            )),
            Err(SourceGroupError::StaleGeneration)
        ));
    }

    #[test]
    fn callback_wakes_read_only_after_waiter_owner_unlocks() {
        use std::future::Future;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        use std::task::{Context, Poll as TaskPoll, Wake, Waker};
        struct Check {
            owner: Arc<PeerSourceOwner>,
            unlocked: AtomicBool,
        }
        impl Wake for Check {
            fn wake(self: Arc<Self>) {
                self.unlocked
                    .store(self.owner.waiters.try_lock().is_ok(), Ordering::SeqCst);
            }
        }
        let owner = PeerSourceOwner::new();
        let route = (
            AttrKey {
                uuid: Uuid::nil(),
                instance: 1,
            },
            AttrKey {
                uuid: Uuid::nil(),
                instance: 2,
            },
        );
        let mut read = CoreBluetoothReplyFuture::default();
        owner
            .waiters
            .lock()
            .unwrap()
            .read(route, read.get_state_clone());
        let check = Arc::new(Check {
            owner: owner.clone(),
            unlocked: AtomicBool::new(false),
        });
        let waker = Waker::from(check.clone());
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            std::pin::Pin::new(&mut read).poll(&mut context),
            TaskPoll::Pending
        ));
        owner.value(route, false, &[7]);
        assert!(check.unlocked.load(Ordering::SeqCst));
        assert!(matches!(
            std::pin::Pin::new(&mut read).poll(&mut context),
            TaskPoll::Ready(CoreBluetoothReply::CharacteristicRead(_, _))
        ));
    }

    #[tokio::test]
    async fn callback_admission_precedes_retirement_and_preserves_read_provenance() {
        let owner = PeerSourceOwner::new();
        let service = AttrKey {
            uuid: Uuid::nil(),
            instance: 1,
        };
        let characteristic = AttrKey {
            uuid: Uuid::nil(),
            instance: 2,
        };
        let route = NotificationRoute {
            service_uuid: service.uuid,
            service_instance: service.instance,
            characteristic_uuid: characteristic.uuid,
            characteristic_instance: characteristic.instance,
        };
        owner.faults.start_source_generation(1).unwrap();
        let (group, consumer) = owner.faults.open_source_group(1, 8, 128).unwrap();
        let membership = owner
            .faults
            .register_source_route(&group, route, 9)
            .unwrap();
        let read = CoreBluetoothReplyFuture::default();
        {
            let mut state = owner.waiters.lock().unwrap();
            state.generation = Some(1);
            state.read((service, characteristic), read.get_state_clone());
        }
        assert!(owner.value((service, characteristic), true, &[7]));
        {
            let _state = owner.waiters.lock().unwrap();
            owner
                .faults
                .retire_source_route(&group, route, membership)
                .unwrap();
        }
        assert!(
            matches!(read.await, CoreBluetoothReply::CharacteristicRead(bytes, ReadProvenance::ReadOrNotification) if bytes == [7])
        );
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Value {
                ordinal: 0,
                provenance: CallbackProvenance::ReadOrNotification,
                ..
            })
        ));
        assert!(
            matches!(consumer.poll(), Poll::Record(IngressRecord::Fence { membership: current }) if current == membership)
        );
        owner.invalidate(crate::PlatformError::new(
            "corebluetooth",
            "disconnected",
            "disconnected",
        ));
        assert!(matches!(
            consumer.poll(),
            Poll::Record(IngressRecord::Terminal {
                reason: TerminalReason::Closed { tail_uncertain: true },
                memberships,
            }) if memberships.is_empty()
        ));
    }
}
