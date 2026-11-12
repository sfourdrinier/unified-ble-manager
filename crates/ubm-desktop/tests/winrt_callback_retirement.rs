//! Portable regression of the WinRT callback admission and reclaim queue.
#![cfg(feature = "btleplug")]
#[path = "../../../vendor/btleplug/src/winrtble/callback_retirement.rs"]
mod callback_retirement;
use btleplug::ubm_reclaim_authority::GraphKey;
use btleplug::ubm_reclaim_graph_gate::{Admission, DrainItem, GraphGate};
use callback_retirement::{CallbackRetirement, admit_managed};
use std::sync::{Arc, Mutex};

fn key() -> GraphKey {
    GraphKey {
        authority_id: 1,
        peer_id: "p".into(),
        connection_generation: "c".into(),
        graph: ("g".into(), 1, 1),
        permit_id: 1,
    }
}

#[tokio::test]
async fn unrelated_retirement_waits_for_blocked_peer_prefix_and_overflow() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use btleplug::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, PeerNotificationIngress, Poll, TerminalReason,
    };
    use std::sync::mpsc;
    let ingress = PeerNotificationIngress::new(1, 8, 64, 0);
    let route = |instance| NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: instance,
    };
    ingress.register(1, route(1), 1).unwrap();
    let b = ingress.register(1, route(2), 2).unwrap();
    let queue: Arc<Mutex<GraphGate<Box<dyn FnOnce() + Send>>>> =
        Arc::new(Mutex::new(GraphGate::new(1, 128)));
    let k = key();
    queue.lock().unwrap().acquire(k.clone()).unwrap();
    queue.lock().unwrap().commit(&k).unwrap();
    let global = Arc::new(CallbackRetirement::new());
    let a = Arc::new(CallbackRetirement::new());
    let b_owner = Arc::new(CallbackRetirement::new());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    admit_managed(
        &global,
        &b_owner,
        |callback| {
            assert!(matches!(
                queue.lock().unwrap().admit(callback, 1, false),
                Admission::Deferred
            ));
        },
        move |tickets| {
            entered_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            drop(tickets);
        },
    );
    queue.lock().unwrap().release(&k).unwrap();
    let changed = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_queue = queue.clone();
    let worker_ingress = ingress.clone();
    let worker_changed = changed.clone();
    let worker_finished = finished.clone();
    let worker = std::thread::spawn(move || {
        loop {
            let item = worker_queue.lock().unwrap().next().unwrap();
            match item {
                DrainItem::Event(callback) => callback(),
                DrainItem::Overflow => {
                    worker_ingress.close(1, true).unwrap();
                }
                DrainItem::Finished => {
                    worker_finished.store(true, std::sync::atomic::Ordering::SeqCst);
                    worker_changed.notify_waiters();
                    break;
                }
                _ => panic!("unexpected barrier"),
            }
        }
    });
    entered_rx.recv().unwrap();
    let source = ingress.clone();
    admit_managed(
        &global,
        &b_owner,
        |callback| {
            assert!(matches!(
                queue.lock().unwrap().admit(callback, 1, false),
                Admission::Deferred
            ));
        },
        move |tickets| {
            source
                .admit(
                    1,
                    Some(b),
                    NotificationEvent::Value(ValueNotification {
                        uuid: uuid::Uuid::nil(),
                        instance: 2,
                        service_uuid: uuid::Uuid::nil(),
                        service_instance: 1,
                        value: vec![7],
                        source_failure: None,
                        lost_before: 0,
                    }),
                )
                .unwrap();
            drop(tickets);
        },
    );
    assert!(matches!(
        queue.lock().unwrap().admit(Box::new(|| {}), 1, false),
        Admission::Overflow(_)
    ));
    a.close();
    let waiter = {
        let changed = changed.clone();
        let finished = finished.clone();
        tokio::spawn(async move {
            a.wait().await;
            callback_retirement::wait_for_peer_fence(&changed, || {
                Ok::<_, ()>(finished.load(std::sync::atomic::Ordering::SeqCst))
            })
            .await
            .unwrap();
        })
    };
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    assert_eq!(ingress.poll(), Poll::Empty);
    resume_tx.send(()).unwrap();
    worker.join().unwrap();
    waiter.await.unwrap();
    assert!(
        matches!(ingress.poll(), Poll::Record(IngressRecord::Value { membership, value, .. }) if membership == b && value.value == [7])
    );
    assert!(matches!(
        ingress.poll(),
        Poll::Record(IngressRecord::Terminal {
            reason: TerminalReason::Closed {
                tail_uncertain: true
            },
            ..
        })
    ));
    assert_eq!(ingress.poll(), Poll::Finished);
}

#[tokio::test]
async fn accepted_queue_prefix_precedes_unsubscribe_and_disconnect() {
    for _operation in ["unsubscribe", "disconnect"] {
        let owner = Arc::new(CallbackRetirement::new());
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut queue: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(4, 1000);
        let key = key();
        queue.acquire(key.clone()).unwrap();
        queue.commit(&key).unwrap();
        let global = Arc::new(CallbackRetirement::new());
        let seen = events.clone();
        admit_managed(
            &global,
            &owner,
            |callback| {
                assert!(matches!(
                    queue.admit(callback, 1, false),
                    Admission::Deferred
                ));
            },
            move |tickets| {
                seen.lock().unwrap().push("value");
                drop(tickets);
            },
        );
        if _operation == "disconnect" {
            global.close();
        } else {
            owner.close();
        }
        let closed = if _operation == "disconnect" {
            global.clone()
        } else {
            owner.clone()
        };
        assert!(closed.admit().is_none());
        let waiter = {
            let events = events.clone();
            tokio::spawn(async move {
                closed.wait().await;
                events.lock().unwrap().push("terminal");
            })
        };
        tokio::task::yield_now().await;
        assert!(
            events.lock().unwrap().is_empty(),
            "terminal overtook accepted value"
        );
        queue.release(&key).unwrap();
        match queue.next().unwrap() {
            DrainItem::Event(callback) => callback(),
            _ => panic!("missing accepted callback"),
        }
        assert!(matches!(queue.next(), Ok(DrainItem::Finished)));
        waiter.await.unwrap();
        assert_eq!(*events.lock().unwrap(), ["value", "terminal"]);
    }
}

#[path = "../../../vendor/btleplug/src/winrtble/callback_gate.rs"]
mod callback_gate;

#[test]
fn handoff_terminates_source_before_fencing_and_never_installs_candidate() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use btleplug::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, PeerNotificationIngress, Poll, TerminalReason,
    };
    use callback_gate::{CallbackGate, terminate_handoff};
    use uuid::Uuid;
    for removal_succeeds in [false, true] {
        let ingress = PeerNotificationIngress::new(1, 8, 64, 0);
        let route = NotificationRoute {
            service_uuid: Uuid::nil(),
            service_instance: 1,
            characteristic_uuid: Uuid::nil(),
            characteristic_instance: 2,
        };
        let member = ingress.register(1, route, 1).unwrap();
        let old = CallbackGate::new(true);
        let emit = |byte| {
            let note = ValueNotification {
                uuid: Uuid::nil(),
                instance: 2,
                service_uuid: Uuid::nil(),
                service_instance: 1,
                value: vec![byte],
                source_failure: None,
                lost_before: 0,
            };
            ingress.admit(1, Some(member), NotificationEvent::Value(note))
        };
        old.admit(|| emit(1).unwrap());
        terminate_handoff(
            || ingress.close(1, true).unwrap(),
            || {
                assert!(
                    matches!(ingress.poll(), Poll::Record(IngressRecord::Value { value, .. }) if value.value == [1])
                );
                assert!(matches!(
                    ingress.poll(),
                    Poll::Record(IngressRecord::Terminal {
                        reason: TerminalReason::Closed {
                            tail_uncertain: true
                        },
                        ..
                    })
                ));
                old.set(false);
            },
        );
        old.admit(|| panic!("old callback admitted during cleanup"));
        // Exercise the actual displaced-owner retry boundary. A failed native
        // removal keeps this owner; no candidate handler was installed.
        let mut displaced = vec![1];
        let result = callback_retirement::recover_matching(
            &mut displaced,
            |_| true,
            |_| {
                if removal_succeeds {
                    Ok(())
                } else {
                    Err("remove failed")
                }
            },
        );
        if removal_succeeds {
            assert_eq!(result, Ok(()));
            assert!(displaced.is_empty());
            old.admit(|| panic!("old callback admitted after publication"));
            let fresh = CallbackGate::new(true);
            let mut observed = false;
            fresh.admit(|| observed = true);
            assert!(observed);
        } else {
            assert_eq!(result, Err("remove failed"));
            assert_eq!(displaced, [1]);
            old.admit(|| panic!("failed removal reopened old callback"));
            callback_retirement::recover_matching(&mut displaced, |_| true, |_| Ok::<_, &str>(()))
                .unwrap();
            assert!(displaced.is_empty());
        }
        assert_eq!(ingress.poll(), Poll::Finished);
    }
}

#[test]
fn running_queue_callback_finishes_before_terminal_and_wake_can_reenter() {
    use std::sync::mpsc;
    let global = Arc::new(CallbackRetirement::new());
    let owner = Arc::new(CallbackRetirement::new());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let mut queue: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(4, 1000);
    let key = key();
    queue.acquire(key.clone()).unwrap();
    queue.commit(&key).unwrap();
    admit_managed(
        &global,
        &owner,
        |callback| {
            assert!(matches!(
                queue.admit(callback, 1, false),
                Admission::Deferred
            ));
        },
        move |tickets| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            seen.lock().unwrap().push("value");
            drop(tickets);
        },
    );
    queue.release(&key).unwrap();
    let drain = std::thread::spawn(move || {
        match queue.next().unwrap() {
            DrainItem::Event(callback) => callback(),
            _ => panic!("missing callback"),
        }
        assert!(matches!(queue.next(), Ok(DrainItem::Finished)));
    });
    entered_rx.recv().unwrap();
    owner.close();
    let waiting_owner = owner.clone();
    let waiting_events = events.clone();
    let wait = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(waiting_owner.wait());
        waiting_events.lock().unwrap().push("terminal");
        done_tx.send(()).unwrap();
    });
    assert!(done_rx.try_recv().is_err());
    release_tx.send(()).unwrap();
    drain.join().unwrap();
    wait.join().unwrap();
    assert_eq!(*events.lock().unwrap(), ["value", "terminal"]);
    // A synchronous wake after ticket release can reacquire and close the gate.
    owner.open();
    assert!(owner.admit().is_some());
    owner.close();
}

#[test]
fn shared_claim_retirement_follows_accepted_callback_in_source_queue() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use btleplug::ubm_peer_notification_ingress::{
        IngressRecord, NotificationRoute, PeerNotificationIngress, Poll,
    };
    use uuid::Uuid;
    let ingress = PeerNotificationIngress::new(1, 8, 64, 0);
    let route = NotificationRoute {
        service_uuid: Uuid::nil(),
        service_instance: 1,
        characteristic_uuid: Uuid::nil(),
        characteristic_instance: 2,
    };
    let member = ingress.register(1, route, 1).unwrap();
    let mut queue: GraphGate<Box<dyn FnOnce() + Send>> = GraphGate::new(4, 1000);
    let key = key();
    queue.acquire(key.clone()).unwrap();
    queue.commit(&key).unwrap();
    let source = ingress.clone();
    assert!(matches!(
        queue.admit(
            Box::new(move || {
                let value = ValueNotification {
                    uuid: Uuid::nil(),
                    instance: 2,
                    service_uuid: Uuid::nil(),
                    service_instance: 1,
                    value: vec![7],
                    source_failure: None,
                    lost_before: 0,
                };
                source
                    .admit(1, Some(member), NotificationEvent::Value(value))
                    .unwrap();
            }),
            1,
            false
        ),
        Admission::Deferred
    ));
    let source = ingress.clone();
    assert!(callback_retirement::enqueue_retirement(
        |callback| matches!(queue.admit(callback, 1, false), Admission::Deferred),
        move || source.retire(1, route, member).unwrap(),
    ));
    queue.release(&key).unwrap();
    if let DrainItem::Event(callback) = queue.next().unwrap() {
        callback();
    }
    if let DrainItem::Event(callback) = queue.next().unwrap() {
        callback();
    }
    assert!(
        matches!(ingress.poll(), Poll::Record(IngressRecord::Value { .. })),
        "membership fence overtook accepted callback"
    );
    assert!(matches!(
        ingress.poll(),
        Poll::Record(IngressRecord::Fence { .. })
    ));
}
