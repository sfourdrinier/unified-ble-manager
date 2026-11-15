#[path = "../src/notification_settlement.rs"]
mod notification_settlement;

use notification_settlement::{Outcome, Registry, RegistryError, State, WaitResult};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};

#[tokio::test]
async fn cancellation_while_waiting_for_backend_lock_prevents_dispatch() {
    use notification_settlement::{FirstPoll, Stop};
    use std::sync::atomic::{AtomicBool, AtomicU8};

    let registry = registry(1);
    let slot = registry.reserve("activation-1", 1, false, 0, 0).unwrap();
    let phase = Arc::new(AtomicU8::new(0));
    let cancelled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (unlock, locked) = oneshot::channel();
    let guard = slot
        .start_controlled(
            Instant::now() + Duration::from_secs(10),
            {
                let phase = Arc::clone(&phase);
                let calls = Arc::clone(&calls);
                move || async move {
                    phase.store(1, Ordering::SeqCst);
                    let _ = locked.await;
                    phase.store(2, Ordering::SeqCst);
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
            {
                let phase = Arc::clone(&phase);
                let cancelled = Arc::clone(&cancelled);
                move |future, cx| {
                    if cancelled.load(Ordering::SeqCst) {
                        return FirstPoll::Stopped(Stop::Cancelled);
                    }
                    let result = future.poll(cx);
                    if phase.load(Ordering::SeqCst) == 1 {
                        FirstPoll::Waiting
                    } else {
                        FirstPoll::Dispatched(result)
                    }
                }
            },
            |_| async { Outcome::Success("disabled") },
        )
        .unwrap();
    while phase.load(Ordering::SeqCst) != 1 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(slot.snapshot(), State::Prepared { .. }));
    cancelled.store(true, Ordering::SeqCst);
    unlock.send(()).unwrap();
    assert_eq!(
        slot.wait().await,
        State::NotStarted {
            attempt: 1,
            reason: Stop::Cancelled
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(guard);
}

#[tokio::test]
async fn dropping_caller_wakes_and_drops_pre_dispatch_backend_wait() {
    use notification_settlement::FirstPoll;
    use std::sync::atomic::AtomicBool;

    let registry = registry(1);
    let slot = registry.reserve("activation-1", 1, false, 0, 0).unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let (mut unlock, locked) = oneshot::channel::<()>();
    let guard = slot
        .start_controlled(
            Instant::now() + Duration::from_secs(10),
            {
                let entered = Arc::clone(&entered);
                move || async move {
                    entered.store(true, Ordering::SeqCst);
                    let _ = locked.await;
                    Ok(())
                }
            },
            |future, cx| match future.poll(cx) {
                std::task::Poll::Pending => FirstPoll::Waiting,
                answer => FirstPoll::Dispatched(answer),
            },
            |_| async { Outcome::Success("disabled") },
        )
        .unwrap();
    while !entered.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    drop(guard);
    tokio::time::timeout(Duration::from_secs(1), unlock.closed())
        .await
        .expect("the abandoned backend wait must be dropped without unlocking it");
    assert!(matches!(slot.snapshot(), State::NotStarted { .. }));
}

#[tokio::test]
async fn deadline_stops_backend_wait_without_waiter_or_unlock() {
    use notification_settlement::{FirstPoll, Stop};

    let registry = registry(1);
    let slot = registry.reserve("activation-1", 1, false, 0, 0).unwrap();
    let (mut unlock, locked) = oneshot::channel::<()>();
    let guard = slot
        .start_controlled(
            Instant::now() + Duration::from_millis(30),
            move || async move {
                let _ = locked.await;
                Ok(())
            },
            |future, cx| match future.poll(cx) {
                std::task::Poll::Pending => FirstPoll::Waiting,
                answer => FirstPoll::Dispatched(answer),
            },
            |_| async { Outcome::Success("disabled") },
        )
        .unwrap();
    let state = tokio::time::timeout(Duration::from_secs(1), slot.wait())
        .await
        .expect("the prepared task must observe its own deadline");
    assert_eq!(
        state,
        State::NotStarted {
            attempt: 1,
            reason: Stop::Deadline
        }
    );
    tokio::time::timeout(Duration::from_secs(1), unlock.closed())
        .await
        .expect("the expired backend wait must be dropped");
    drop(guard);
}

fn registry(
    capacity: usize,
) -> Registry<&'static str, u64, Vec<u8>, (), &'static str, &'static str> {
    Registry::new(capacity, 8, 64)
}

#[tokio::test]
async fn abandoned_waiter_retains_late_success_and_fifo_for_exact_admission() {
    let registry = registry(2);
    let slot = registry.reserve("activation-1", 7, true, 3, 8).unwrap();
    let (tx, rx) = oneshot::channel();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(10),
            move || async move {
                rx.await.unwrap();
                Ok(())
            },
            |native| async move {
                Outcome::Success(if native.is_ok() {
                    "disabled"
                } else {
                    "unexpected"
                })
            },
        )
        .unwrap();
    slot.wait_dispatched().await;
    drop(guard);
    slot.retain(&7, vec![1], 1).unwrap();
    slot.retain(&7, vec![2], 1).unwrap();
    tx.send(()).unwrap();
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 1,
            native: Ok(()),
            outcome: Outcome::Success("disabled")
        }
    );
    assert_eq!(
        slot.deliver(&7, None::<&(dyn Fn(Vec<u8>) + Send + Sync)>)
            .await,
        Err(RegistryError::PendingDrain)
    );
    let seen = std::sync::Mutex::new(Vec::new());
    slot.deliver(&7, Some(&|v| seen.lock().unwrap().push(v)))
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![vec![1], vec![2]]);
    assert_eq!(
        slot.deliver(&8, None::<&(dyn Fn(Vec<u8>) + Send + Sync)>)
            .await,
        Err(RegistryError::UnknownConsumer)
    );
    slot.acknowledge(&7).unwrap();
    registry.retire(&"activation-1").unwrap();
}

#[tokio::test]
async fn late_native_failure_is_observed_before_retry_and_new_attempt_is_exclusive() {
    let registry = registry(2);
    let slot = registry.reserve("activation-1", 7, false, 0, 0).unwrap();
    let (tx, rx) = oneshot::channel();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(10),
            move || async move {
                rx.await.unwrap();
                Err("native detail")
            },
            |r| async move { Outcome::Failed(r.unwrap_err()) },
        )
        .unwrap();
    slot.wait_dispatched().await;
    assert_eq!(slot.wait_until(Instant::now()).await, WaitResult::TimedOut);
    drop(guard);
    assert_eq!(slot.prepare_retry(), Err(RegistryError::Pending));
    tx.send(()).unwrap();
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 1,
            native: Err("native detail"),
            outcome: Outcome::Failed("native detail")
        }
    );
    assert_eq!(slot.prepare_retry(), Err(RegistryError::Unacknowledged));
    slot.acknowledge(&7).unwrap();
    slot.prepare_retry().unwrap();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(10),
            || async { Ok(()) },
            |_| async { Outcome::Success("disabled") },
        )
        .unwrap();
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 2,
            native: Ok(()),
            outcome: Outcome::Success("disabled")
        }
    );
    drop(guard);
}

#[tokio::test]
async fn replacement_identity_is_isolated_and_capacity_never_evicts_debt() {
    let registry = registry(2);
    let old = registry.reserve("generation-1", 5, true, 2, 4).unwrap();
    let new = registry.reserve("generation-2", 5, true, 2, 4).unwrap();
    assert!(matches!(
        registry.reserve("generation-3", 5, false, 0, 0),
        Err(RegistryError::Capacity)
    ));
    old.retain(&5, vec![1, 2], 2).unwrap();
    assert_eq!(
        old.retain(&5, vec![3, 4, 5], 3).unwrap_err().error,
        RegistryError::Capacity
    );
    assert_eq!(
        new.deliver(&5, None::<&(dyn Fn(Vec<u8>) + Send + Sync)>)
            .await,
        Ok(())
    );
    assert!(Arc::ptr_eq(&registry.get(&"generation-1").unwrap(), &old));
    assert_eq!(
        registry.retire(&"generation-1"),
        Err(RegistryError::Pending)
    );
}

#[tokio::test]
async fn pre_dispatch_abandonment_cancel_and_deadline_never_poll_native() {
    let calls = Arc::new(AtomicUsize::new(0));
    for mode in 0..3 {
        let registry = registry(1);
        let slot = registry.reserve("activation-1", 1, false, 0, 0).unwrap();
        let count = calls.clone();
        let deadline = if mode == 2 {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs(10)
        };
        let guard = slot
            .start(
                deadline,
                move || async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_| async { Outcome::Success("done") },
            )
            .unwrap();
        if mode == 0 {
            drop(guard);
        } else {
            if mode == 1 {
                slot.cancel_prepared(1);
            }
            drop(guard);
        }
        assert!(matches!(slot.wait().await, State::NotStarted { .. }));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_retention_returns_unconsumed_item_and_reservations_are_released_only_on_retire() {
    let registry = registry(2);
    let slot = registry.reserve("one", 1, true, 1, 1).unwrap();
    registry.add_consumer(&"one", 2, true, 1, 1).unwrap();
    slot.retain(&1, vec![1], 1).unwrap();
    let rejected = slot.retain(&1, vec![2, 3], 2).unwrap_err();
    assert_eq!(rejected.error, RegistryError::Capacity);
    assert_eq!(rejected.item, vec![2, 3]);
    assert!(matches!(
        registry.reserve("too-large", 3, true, 7, 0),
        Err(RegistryError::Capacity)
    ));
}

#[tokio::test]
async fn panic_during_reconciliation_preserves_native_error_and_forbids_retry() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Err("native detail") },
            |_| async {
                panic!("reconcile panic");
            },
        )
        .unwrap();
    assert_eq!(
        slot.wait().await,
        State::Indeterminate {
            attempt: 1,
            native: Some(Err("native detail")),
            diagnostic: "settlement task ended without a reconciled answer"
        }
    );
    assert_eq!(slot.prepare_retry(), Err(RegistryError::Pending));
    drop(guard);
}

#[tokio::test]
async fn callback_cannot_acknowledge_or_retire_while_delivery_is_running() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, true, 1, 1).unwrap();
    slot.retain(&1, vec![1], 1).unwrap();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Ok(()) },
            |_| async { Outcome::Success("done") },
        )
        .unwrap();
    slot.wait().await;
    slot.deliver(
        &1,
        Some(&|_| {
            assert_eq!(slot.acknowledge(&1), Err(RegistryError::PendingDrain));
            assert_eq!(registry.retire(&"one"), Err(RegistryError::Unacknowledged));
        }),
    )
    .await
    .unwrap();
    slot.acknowledge(&1).unwrap();
    registry.retire(&"one").unwrap();
    drop(guard);
}

#[tokio::test]
async fn cancellation_and_timeout_before_dispatch_stop_the_first_poll() {
    for cancelled in [true, false] {
        let registry = registry(1);
        let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let copy = calls.clone();
        let guard = slot
            .start(
                Instant::now() + Duration::from_secs(5),
                move || async move {
                    copy.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_| async { Outcome::Success("done") },
            )
            .unwrap();
        let outcome = if cancelled {
            slot.wait_or_cancel(Instant::now() + Duration::from_secs(1), async {})
                .await
        } else {
            slot.wait_until(Instant::now()).await
        };
        assert_eq!(
            outcome,
            if cancelled {
                WaitResult::Cancelled
            } else {
                WaitResult::TimedOut
            }
        );
        assert!(matches!(slot.wait().await, State::NotStarted { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(guard);
    }
}

#[tokio::test]
async fn late_retired_native_failure_is_not_confused_with_replacement() {
    let registry = registry(2);
    let old = registry.reserve("generation-1", 5, false, 0, 0).unwrap();
    let (tx, rx) = oneshot::channel();
    let guard = old
        .start(
            Instant::now() + Duration::from_secs(5),
            move || async move {
                rx.await.unwrap();
                Err("old platform error")
            },
            |result| async move { Outcome::Retired(result.map(|_| "done")) },
        )
        .unwrap();
    old.wait_dispatched().await;
    let replacement = registry.reserve("generation-2", 5, false, 0, 0).unwrap();
    tx.send(()).unwrap();
    assert_eq!(
        old.wait().await,
        State::Complete {
            attempt: 1,
            native: Err("old platform error"),
            outcome: Outcome::Retired(Err("old platform error"))
        }
    );
    assert_eq!(replacement.snapshot(), State::Prepared { attempt: 1 });
    drop(guard);
}

#[tokio::test]
async fn consumer_count_is_bounded_even_with_zero_sized_tails() {
    let registry = registry(2);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    registry.add_consumer(&"one", 2, false, 0, 0).unwrap();
    assert_eq!(
        registry.add_consumer(&"one", 3, false, 0, 0),
        Err(RegistryError::Capacity)
    );
    assert!(matches!(
        registry.reserve("two", 4, false, 0, 0),
        Err(RegistryError::Capacity)
    ));
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Ok(()) },
            |_| async { Outcome::Success("done") },
        )
        .unwrap();
    slot.wait().await;
    slot.acknowledge(&1).unwrap();
    slot.acknowledge(&2).unwrap();
    registry.retire(&"one").unwrap();
    assert!(registry.reserve("two", 4, false, 0, 0).is_ok());
    drop(guard);
}

#[tokio::test]
async fn callback_panic_leaves_delivery_indeterminate() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, true, 1, 1).unwrap();
    slot.retain(&1, vec![1], 1).unwrap();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Ok(()) },
            |_| async { Outcome::Success("done") },
        )
        .unwrap();
    slot.wait().await;
    let clone = slot.clone();
    let task = tokio::spawn(async move { clone.deliver(&1, Some(&|_| panic!("callback"))).await });
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(
        slot.acknowledge(&1),
        Err(RegistryError::DeliveryIndeterminate)
    );
    assert_eq!(
        slot.deliver(&1, None::<&(dyn Fn(Vec<u8>) + Send + Sync)>)
            .await,
        Err(RegistryError::DeliveryIndeterminate)
    );
    assert_eq!(registry.retire(&"one"), Err(RegistryError::Unacknowledged));
    drop(guard);
}

#[tokio::test]
async fn panic_during_first_poll_is_indeterminate_and_cannot_poison_registry() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async {
                panic!("native poll");
                #[allow(unreachable_code)]
                Ok(())
            },
            |_| async { Outcome::Success("done") },
        )
        .unwrap();
    assert_eq!(
        slot.wait().await,
        State::Indeterminate {
            attempt: 1,
            native: None,
            diagnostic: "settlement task ended without a reconciled answer"
        }
    );
    assert_eq!(slot.prepare_retry(), Err(RegistryError::Pending));
    drop(guard);
}

#[tokio::test]
async fn old_caller_guard_cannot_abandon_a_new_attempt() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    let old_guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Err("failure") },
            |result| async move { Outcome::Failed(result.unwrap_err()) },
        )
        .unwrap();
    slot.wait().await;
    slot.acknowledge(&1).unwrap();
    slot.prepare_retry().unwrap();
    let new_guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Ok(()) },
            |_| async { Outcome::Success("done") },
        )
        .unwrap();
    drop(old_guard);
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 2,
            native: Ok(()),
            outcome: Outcome::Success("done")
        }
    );
    drop(new_guard);
}

#[tokio::test]
async fn undispatched_old_worker_cannot_claim_a_retry() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let copy = calls.clone();
    let old = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            move || async move {
                copy.fetch_add(1, Ordering::SeqCst);
                Err("old")
            },
            |r| async move { Outcome::Failed(r.unwrap_err()) },
        )
        .unwrap();
    slot.cancel_prepared(1);
    slot.acknowledge(&1).unwrap();
    slot.prepare_retry().unwrap();
    let new = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Ok(()) },
            |_| async { Outcome::Success("new") },
        )
        .unwrap();
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 2,
            native: Ok(()),
            outcome: Outcome::Success("new")
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop((old, new));
}

#[tokio::test]
async fn completion_is_not_published_until_reconciliation_finishes() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    let (tx, rx) = oneshot::channel();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            || async { Err("native detail") },
            move |native| async move {
                rx.await.unwrap();
                Outcome::Failed(native.unwrap_err())
            },
        )
        .unwrap();
    loop {
        if matches!(slot.snapshot(), State::Reconciling { .. }) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(slot.wait_until(Instant::now()).await, WaitResult::TimedOut);
    assert_eq!(
        slot.snapshot(),
        State::Reconciling {
            attempt: 1,
            native: Err("native detail")
        }
    );
    tx.send(()).unwrap();
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 1,
            native: Err("native detail"),
            outcome: Outcome::Failed("native detail")
        }
    );
    drop(guard);
}

#[tokio::test]
async fn dispatched_caller_cancellation_does_not_cancel_native_task() {
    let registry = registry(1);
    let slot = registry.reserve("one", 1, false, 0, 0).unwrap();
    let (tx, rx) = oneshot::channel();
    let guard = slot
        .start(
            Instant::now() + Duration::from_secs(5),
            move || async move {
                rx.await.unwrap();
                Ok(())
            },
            |_| async { Outcome::Success("done") },
        )
        .unwrap();
    slot.wait_dispatched().await;
    assert_eq!(
        slot.wait_or_cancel(Instant::now() + Duration::from_secs(5), async {})
            .await,
        WaitResult::Cancelled
    );
    drop(guard);
    tx.send(()).unwrap();
    assert_eq!(
        slot.wait().await,
        State::Complete {
            attempt: 1,
            native: Ok(()),
            outcome: Outcome::Success("done")
        }
    );
}
