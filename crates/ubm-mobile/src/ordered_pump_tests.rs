// crates/ubm-mobile/src/ordered_pump_tests.rs
use super::*;
use crate::test_common::{self, Reply, Scripted};
use std::sync::atomic::{AtomicUsize, Ordering};
use ubm_core::ordered_notifications::{LossScope, LossUnit, SourceGeneration};
use ubm_desktop::boundary::{OrderedSourceIngress, OrderedSourceLease};
use ubm_desktop::central::empty_ordered_source::EmptyOrderedPoll;

struct Fixture {
    host: Arc<crate::MobileHost>,
    radio: Arc<Scripted>,
    a: Arc<OrderedBatchConsumer>,
    b: Arc<OrderedBatchConsumer>,
    source_a: SourceKey,
    source_b: SourceKey,
}
async fn fixture(stall_ack: bool) -> Fixture {
    fixture_with_source_failure(stall_ack, false).await
}

async fn fixture_with_source_failure(stall_ack: bool, source_failure: bool) -> Fixture {
    let a_drains = Arc::new(AtomicUsize::new(0));
    let b_drains = Arc::new(AtomicUsize::new(0));
    let a = a_drains.clone();
    let b = b_drains.clone();
    let radio = Scripted::new(Box::new(move |request| match request {
        RadioRequest::OrderedDrain { group_id: 7, .. } if a.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(1),
                records: vec![OrderedRecord::Loss {
                    scope: LossScope::Unknown,
                    unit: LossUnit::Values,
                    count: None,
                    byte_count: None,
                }],
            })
        }
        RadioRequest::OrderedDrain { group_id: 8, .. } if b.fetch_add(1, Ordering::SeqCst) == 0 => {
            Reply::Now(RadioCompletion::OrderedRecords {
                batch_id: Some(1),
                records: vec![if source_failure {
                    OrderedRecord::SourceFailed {
                        uncertain_tail: true,
                        failure: crate::PlatformFailure::new(
                            crate::FailureKind::PermissionDenied,
                            "permission denied",
                        ),
                        native_domain: Some("android".into()),
                        native_code: None,
                        failure_member: None,
                        rejected: None,
                    }
                } else {
                    OrderedRecord::Terminal {
                        reason: OrderedEnd::WakeFailed,
                        uncertain_tail: true,
                        native_domain: Some("test".into()),
                        native_code: Some("wake".into()),
                        failure_member: None,
                        rejected: None,
                    }
                }],
            })
        }
        RadioRequest::OrderedDrain { .. } => Reply::Now(RadioCompletion::OrderedRecords {
            batch_id: None,
            records: vec![],
        }),
        RadioRequest::OrderedAcknowledgeBatch { group_id: 7, .. } if stall_ack => Reply::Hold,
        _ => test_common::polar_responder(request),
    }));
    let (host, _) = test_common::open(&radio, crate::MobilePlatform::Android).await;
    let mut sources = Vec::new();
    for id in [7, 8] {
        let peer_id = if source_failure && id == 7 {
            "other-peer"
        } else {
            "peer"
        };
        let source = SourceKey {
            peer_id: peer_id.into(),
            generation: SourceGeneration::Counter(id),
            native_group: id,
        };
        host.inner
            .central
            .install_empty_ordered_ingress(source.clone(), id, 8)
            .unwrap();
        host.ordered_wakes().begin(id).unwrap();
        host.inner
            .attach_ordered_source(&OrderedSourceLease {
                peer_id: peer_id.into(),
                connection_generation: "connection".into(),
                collection_id: id,
                source_generation: source.generation.clone(),
                native_group: id,
                members: vec![],
                ingress: OrderedSourceIngress::CentralRetainedBatch,
            })
            .unwrap();
        sources.push(source);
    }
    let consumer_a = host.ordered_consumer(&sources[0]).unwrap();
    let consumer_b = host.ordered_consumer(&sources[1]).unwrap();
    Fixture {
        host,
        radio,
        a: consumer_a,
        b: consumer_b,
        source_a: sources.remove(0),
        source_b: sources.remove(0),
    }
}

#[tokio::test]
async fn acknowledged_structured_source_failure_marks_the_source_ended() {
    let f = fixture_with_source_failure(false, true).await;
    assert_eq!(wake(&f, &f.source_b), crate::IngressStatus::Accepted);
    wait_until(|| f.b.terminal_acked()).await;
    assert!(f.b.terminal_acked());
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        ubm_desktop::boundary::RadioBoundary::flush_peer_notification_source(
            &f.host.inner.radio,
            "peer",
        ),
    )
    .await
    .expect("flush waited for an ended source")
    .expect("flush rejected an ended source");
    assert!(
        !f.radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| { matches!(request, RadioRequest::OrderedBarrier { group_id: 8, .. }) })
    );
}
fn wake(f: &Fixture, source: &SourceKey) -> crate::IngressStatus {
    f.host.ingest(crate::RadioIngress::OrderedWake {
        generation: source.generation.clone(),
        group_id: source.native_group,
    })
}
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordered group did not progress");
}
fn a_drain_count(f: &Fixture) -> usize {
    f.radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request, RadioRequest::OrderedDrain { group_id: 7, .. }))
        .count()
}

#[tokio::test]
async fn paused_group_does_not_block_another_and_shutdown_joins_workers() {
    let f = fixture(false).await;
    f.a.pause_routing().await;
    assert_eq!(wake(&f, &f.source_a), crate::IngressStatus::Accepted);
    assert_eq!(wake(&f, &f.source_b), crate::IngressStatus::Accepted);
    wait_until(|| f.b.terminal_acked()).await;
    assert_eq!(a_drain_count(&f), 0);
    assert!(f.host.ingress_failure().is_none());
    tokio::time::timeout(std::time::Duration::from_secs(1), f.host.shutdown())
        .await
        .expect("shutdown did not join paused worker");
}

#[tokio::test]
async fn stalled_ack_does_not_block_another_group() {
    let f = fixture(true).await;
    wake(&f, &f.source_a);
    wait_until(|| {
        !f.radio
            .held_of(crate::RequestKind::OrderedAcknowledgeBatch)
            .is_empty()
    })
    .await;
    wake(&f, &f.source_b);
    wait_until(|| f.b.terminal_acked()).await;
    assert!(!f.a.failed());
    assert!(f.host.ingress_failure().is_none());
    let held = f.radio.held_of(crate::RequestKind::OrderedAcknowledgeBatch);
    assert_eq!(held.len(), 1);
    f.radio.answer(held[0], RadioCompletion::Unit);
    wait_until(|| a_drain_count(&f) == 2).await;
}

#[tokio::test]
async fn wrong_generation_during_drain_fails_only_its_bound_group() {
    let f = fixture(true).await;
    wake(&f, &f.source_a);
    wait_until(|| {
        !f.radio
            .held_of(crate::RequestKind::OrderedAcknowledgeBatch)
            .is_empty()
    })
    .await;
    assert_eq!(
        f.host.ingest(crate::RadioIngress::OrderedWake {
            generation: SourceGeneration::Counter(999),
            group_id: 7
        }),
        crate::IngressStatus::Dropped(crate::IngressClass::Control)
    );
    wait_until(|| f.a.failed()).await;
    wake(&f, &f.source_b);
    wait_until(|| f.b.terminal_acked()).await;
    assert!(f.host.ingress_failure().is_none());
    assert!(matches!(
        f.host
            .inner
            .central
            .poll_empty_ordered_ingress(&f.source_a, 7)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::Loss { .. })
    ));
    assert!(matches!(
        f.host
            .inner
            .central
            .poll_empty_ordered_ingress(&f.source_a, 7)
            .unwrap(),
        EmptyOrderedPoll::Record(OrderedIngressRecord::BridgeFailure { .. })
    ));
    let held = f.radio.held_of(crate::RequestKind::OrderedAcknowledgeBatch);
    assert_eq!(held.len(), 1);
    f.radio.answer(held[0], RadioCompletion::Unit);
    wait_until(|| !f.host.ordered_wakes().has_inflight(7)).await;
    assert!(f.host.ingress_failure().is_none());
}
