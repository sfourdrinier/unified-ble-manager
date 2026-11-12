// crates/ubm-desktop/src/peer_notification_ingress_tests.rs
use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn target(epoch: u64) -> ForwardTarget {
        ForwardTarget {
            peer_id: "peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 0,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 0,
            epoch,
        }
    }

    #[tokio::test]
    async fn mappings_survive_until_ordered_fence_and_retries_share_wait() {
        let ingress = PeerNotificationIngress::new(1, 16, 100, 0);
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 0,
        };
        let members = Members::default();
        let old = members
            .register(target(4), || ingress.register(1, route, 7))
            .unwrap();
        let first = members
            .retire(old, || ingress.retire(1, route, old))
            .unwrap()
            .unwrap();
        let second = members
            .retire(old, || -> Result<(), IngressError> {
                panic!("must not retire twice")
            })
            .unwrap()
            .unwrap();
        let newer = members
            .register(target(5), || ingress.register(1, route, 7))
            .unwrap();
        assert_eq!(members.target(old).unwrap().epoch, 4);
        assert_eq!(members.target(newer).unwrap().epoch, 5);
        let snapshot = members.snapshot(&[(route, old)]).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].1, 4);
        assert_eq!(
            ingress.poll(),
            Poll::Record(IngressRecord::Fence { membership: old })
        );
        assert!(members.fence(old));
        first.await.unwrap();
        second.await.unwrap();
        assert!(members.target(old).is_none());
        assert_eq!(members.target(newer).unwrap().epoch, 5);
        assert!(!members.fence(old));
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    use btleplug::api::{NotificationEvent, ValueNotification};

    fn target(epoch: u64) -> ForwardTarget {
        ForwardTarget {
            peer_id: "peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 0,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 0,
            epoch,
        }
    }

    #[test]
    fn failure_occurrence_keeps_native_error_and_admission_membership() {
        let ingress = PeerNotificationIngress::new(1, 16, 100, 0);
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 0,
        };
        let members = Members::default();
        let member = members
            .register(target(4), || ingress.register(1, route, 7))
            .unwrap();
        let error = std::sync::Arc::new(
            btleplug::PlatformError::new("winrt", "failure", "original").with("native", "metadata"),
        );
        let record = IngressRecord::FailureOccurrence {
            error: error.clone().into(),
            memberships: vec![(route, member)],
        };
        assert!(
            matches!(map_record(&members, record), Some(MappedRecord::FailureOccurrence { error: found, memberships, .. })
            if std::sync::Arc::ptr_eq(found.as_arc(), &error) && memberships == vec![member])
        );
    }

    #[test]
    fn queued_loss_uses_admission_time_membership_not_later_joiners() {
        let ingress = PeerNotificationIngress::new(1, 16, 100, 0);
        let route = NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: 0,
        };
        let members = Members::default();
        let old = members
            .register(target(4), || ingress.register(1, route, 7))
            .unwrap();
        let value = ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance: 0,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            value: vec![1],
            source_failure: None,
            lost_before: 0,
        };
        ingress
            .admit_with_provenance(
                1,
                Some(old),
                NotificationEvent::Value(value),
                CallbackProvenance::ReadOrNotification,
            )
            .unwrap();
        ingress
            .admit(1, None, NotificationEvent::Lagged { skipped_records: 2 })
            .unwrap();
        let _wait = members
            .retire(old, || ingress.retire(1, route, old))
            .unwrap()
            .unwrap();
        let new = members
            .register(target(5), || ingress.register(1, route, 7))
            .unwrap();
        let Poll::Record(record) = ingress.poll() else {
            panic!("value expected")
        };
        assert!(
            matches!(map_record(&members, record), Some(MappedRecord::Value { target, provenance: CallbackProvenance::ReadOrNotification, .. }) if target.epoch == 4)
        );
        let Poll::Record(record) = ingress.poll() else {
            panic!("loss expected")
        };
        assert!(
            matches!(map_record(&members, record), Some(MappedRecord::Loss { members, skipped_records: 2, .. }) if members.len() == 1 && members[0].1 == 4)
        );
        let Poll::Record(record) = ingress.poll() else {
            panic!("fence expected")
        };
        assert!(
            matches!(map_record(&members, record), Some(MappedRecord::Fence { membership, target }) if membership == old && target.epoch == 4)
        );
        assert!(members.fence(old));
        assert_eq!(members.target(new).unwrap().epoch, 5);
    }
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    use btleplug::api::{NotificationEvent, ValueNotification};

    fn target(characteristic: &str, epoch: u64) -> ForwardTarget {
        ForwardTarget {
            peer_id: "peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 0,
            characteristic_uuid: characteristic.into(),
            characteristic_occurrence: 0,
            epoch,
        }
    }
    fn route(instance: u64) -> NotificationRoute {
        NotificationRoute {
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            characteristic_uuid: uuid::Uuid::nil(),
            characteristic_instance: instance,
        }
    }
    fn value(instance: u64, byte: u8) -> NotificationEvent {
        NotificationEvent::Value(ValueNotification {
            uuid: uuid::Uuid::nil(),
            instance,
            service_uuid: uuid::Uuid::nil(),
            service_instance: 0,
            value: vec![byte],
            source_failure: None,
            lost_before: 0,
        })
    }

    #[test]
    fn oversized_failure_remains_retryable_with_original_evidence_owned() {
        use ubm_core::ordered_notifications::{
            DeliveryMode, Member, OrderedCollection, SourceGeneration,
        };
        let key = crate::ordered_notification_groups::SourceKey {
            peer_id: "peer".into(),
            generation: SourceGeneration::Counter(8),
            native_group: 1,
        };
        let membership = Membership {
            member: 1,
            epoch: 0,
        };
        let member = Member {
            id: 10,
            source: registry_membership(membership),
            delivery: DeliveryMode::Notification,
        };
        let registry =
            Mutex::new(crate::ordered_notification_groups::OrderedGroupRegistry::default());
        {
            let mut registry = registry.lock().unwrap();
            registry
                .install(
                    9,
                    key.clone(),
                    OrderedCollection::new(
                        "connection".into(),
                        "database".into(),
                        SourceGeneration::Counter(8),
                        9,
                        vec![member],
                        8,
                        128,
                        4,
                    )
                    .unwrap(),
                )
                .unwrap();
            registry
                .with_collection(9, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(9, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
        let original = Arc::new(btleplug::PlatformError::new(
            "winrt",
            "oversized",
            "x".repeat(4096),
        ));
        let record = MappedRecord::FailureOccurrence {
            memberships: vec![membership],
            members: Vec::new(),
            error: original.clone().into(),
        };
        let result =
            admit_to_collections(&registry, &key, &record, &Members::default()).unwrap_err();
        assert!(
            result.retryable,
            "oversized original evidence must stay owned for recovery"
        );
        assert!(registry.lock().unwrap().pending_source_wake(&key).is_some());
        assert!(
            matches!(record, MappedRecord::FailureOccurrence { error, .. } if Arc::ptr_eq(error.as_arc(), &original))
        );
    }

    #[cfg(feature = "reclaim-test-support")]
    #[tokio::test]
    async fn admitted_failures_cross_source_pump_once_before_terminal() {
        use ubm_core::ordered_notifications::{
            DeliveryMode, Member, OrderedCollection, Poll as CollectionPoll, Record,
            SourceGeneration,
        };
        let (source, consumer) = btleplug::platform::PeerGroupTestSource::new(8, 16);
        let source_observer = consumer.clone();
        let members = Arc::new(Members::default());
        let membership = members
            .register(target("a", 1), || source.register(route(1), 1))
            .unwrap();
        let key = crate::ordered_notification_groups::SourceKey {
            peer_id: "peer".into(),
            generation: SourceGeneration::Counter(8),
            native_group: 1,
        };
        let member = Member {
            id: 10,
            source: registry_membership(membership),
            delivery: DeliveryMode::Notification,
        };
        let registry = Arc::new(Mutex::new(
            crate::ordered_notification_groups::OrderedGroupRegistry::default(),
        ));
        {
            let mut registry = registry.lock().unwrap();
            registry
                .install(
                    9,
                    key.clone(),
                    OrderedCollection::new(
                        "connection".into(),
                        "database".into(),
                        SourceGeneration::Counter(8),
                        9,
                        vec![member],
                        8,
                        4096,
                        1,
                    )
                    .unwrap(),
                )
                .unwrap();
            registry
                .with_collection(9, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(9, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
            registry
                .admit_source_loss(&key, &[registry_membership(membership)], Some(1))
                .unwrap();
        }
        let (sender, mut received) = mpsc::channel(8);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            false,
            registry.clone(),
            consumer,
            members,
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        source
            .publish_two_failures(
                btleplug::PlatformError::new("winrt", "first", "first failure").with("native", "a"),
                btleplug::PlatformError::new("winrt", "second", "second failure")
                    .with("native", "b"),
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if registry.lock().unwrap().pending_source_wake(&key).is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first failure did not wait for capacity");
        let held = source_observer
            .recv_transfer()
            .await
            .expect("failure must remain at source");
        assert!(
            matches!(held.record.as_ref(), IngressRecord::FailureOccurrence { error, .. } if error.code == "first")
        );
        assert!(matches!(
            registry.lock().unwrap().poll(9).unwrap(),
            CollectionPoll::Record(Record::Loss { .. })
        ));
        for expected in ["first", "second"] {
            let record = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let next = registry.lock().unwrap().poll(9).unwrap();
                    if !matches!(next, CollectionPoll::Empty) {
                        break next;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("failure did not reach collection");
            assert!(
                matches!(record, CollectionPoll::Record(Record::FailureObservation { failure: Some(failure), .. })
                if failure.platform_code.as_deref() == Some(expected))
            );
        }
        let fence = tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
        let RadioEvent::PeerNotificationSourceFence { ack } = fence else {
            panic!("expected terminal fence");
        };
        ack.acknowledge();
        assert!(task.await.unwrap().is_ok());
        let mut registry = registry.lock().unwrap();
        assert!(matches!(
            registry.poll(9).unwrap(),
            CollectionPoll::Terminal(_)
        ));
    }

    #[cfg(feature = "reclaim-test-support")]
    #[tokio::test]
    async fn retained_failure_offer_waits_for_full_read_and_recipient_ack() {
        use std::time::Duration;

        let (source, consumer) = btleplug::platform::PeerGroupTestSource::new(8, 16);
        let members = Arc::new(Members::default());
        members
            .register(target("a", 1), || source.register(route(1), 1))
            .unwrap();
        let (sender, mut received) = mpsc::channel(8);
        let store = Arc::new(PeerFailureRecoveryStore::new(9));
        let task = tokio::spawn(pump_group_with_failure_recovery(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            consumer,
            members,
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
            Some(Arc::clone(&store)),
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        source
            .publish_two_failures(
                btleplug::PlatformError::new("winrt", "first", "x".repeat(4 * 1024 * 1024 + 1))
                    .with("hresult", "0x80004005")
                    .with("gattStatus", "protocol-error"),
                btleplug::PlatformError::new("winrt", "second", "next"),
            )
            .unwrap();
        let Some(RadioEvent::PeerNotificationFailureRecoveryOffered {
            descriptor,
            members,
            teardown,
            ack,
        }) = received.recv().await
        else {
            panic!("expected retained failure offer");
        };
        assert_eq!(members.len(), 1);
        assert_eq!(
            teardown
                .platform
                .as_ref()
                .map(|detail| detail.domain.as_str()),
            Some("winrt")
        );
        assert_eq!(
            teardown
                .platform
                .as_ref()
                .map(|detail| detail.code.as_str()),
            Some("first")
        );
        assert_eq!(
            teardown
                .platform
                .as_ref()
                .and_then(|detail| detail.message.as_ref())
                .map(String::len),
            Some(4 * 1024 * 1024 + 1)
        );
        let platform = teardown
            .platform
            .as_ref()
            .expect("native platform evidence");
        assert_eq!(
            platform
                .metadata_entries
                .as_ref()
                .expect("native metadata order")
                .len(),
            2
        );
        assert_eq!(
            platform.metadata.get("hresult"),
            Some(&crate::errors::PlatformValue::Text("0x80004005".into()))
        );
        assert_eq!(
            platform.metadata.get("gattStatus"),
            Some(&crate::errors::PlatformValue::Text("protocol-error".into()))
        );
        ack.acknowledge();

        // Admission alone must not advance the source or expose the next
        // failure. A stale/incomplete recipient ACK also leaves it retained.
        assert!(store.acknowledge(&descriptor).is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), received.recv())
                .await
                .is_err()
        );

        let mut offset = 0;
        loop {
            let page = store.read(&descriptor, offset).unwrap();
            offset = page.next_offset;
            if page.done {
                break;
            }
        }
        // The source may advance only after the bounded pages are read.  Its
        // opaque begin receipt is repeatable for a journal retry, but a
        // foreign receipt cannot release the vendor-owned source cut.
        let receipt = store.prepare_ack(&descriptor).unwrap();
        // Preparing the durable receipt is deliberately not source
        // acknowledgement: central must persist it before native head
        // advancement, so this pre-begin crash window retains the first cut.
        assert!(
            store
                .begin_ack_with_receipt(&descriptor, "foreign-receipt")
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), received.recv())
                .await
                .is_err()
        );
        assert_eq!(
            store.begin_ack_with_receipt(&descriptor, &receipt).unwrap(),
            receipt
        );
        assert_eq!(
            store.begin_ack_with_receipt(&descriptor, &receipt).unwrap(),
            receipt
        );
        assert!(store.retire_ack(&descriptor, "foreign-receipt").is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), received.recv())
                .await
                .is_err()
        );
        store.retire_ack(&descriptor, &receipt).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), received.recv()).await.unwrap(),
            Some(RadioEvent::PeerNotificationFailureRecoveryOffered { descriptor: next, .. })
                if next.identity.transfer != descriptor.identity.transfer
        ));
        task.abort();
    }

    #[tokio::test]
    async fn failed_downstream_send_keeps_exact_native_occurrence() {
        let source = PeerNotificationIngress::new(8, 16, 128, 1);
        let members = Arc::new(Members::default());
        let membership = members
            .register(target("a", 1), || source.register(8, route(1), 1))
            .unwrap();
        let (sender, mut received) = mpsc::channel(8);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            source.clone(),
            members,
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        drop(received);
        source.admit(8, Some(membership), value(1, 42)).unwrap();
        assert!(task.await.unwrap().is_err());
        let retained = source
            .peek_transfer()
            .expect("native occurrence must remain owned");
        assert!(
            matches!(&*retained.record, IngressRecord::Value { value, .. } if value.value == [42])
        );
    }

    #[tokio::test]
    async fn retry_after_fanout_send_failure_does_not_readmit_the_same_occurrence() {
        use ubm_core::ordered_notifications::{
            DeliveryMode, Member, OrderedCollection, Poll as CollectionPoll, Record,
            SourceGeneration,
        };
        let source = PeerNotificationIngress::new(8, 16, 128, 1);
        let members = Arc::new(Members::default());
        let membership = members
            .register(target("a", 1), || source.register(8, route(1), 1))
            .unwrap();
        let key = crate::ordered_notification_groups::SourceKey {
            peer_id: "peer".into(),
            generation: SourceGeneration::Counter(8),
            native_group: 1,
        };
        let member = Member {
            id: 10,
            source: registry_membership(membership),
            delivery: DeliveryMode::Notification,
        };
        let registry = Arc::new(Mutex::new(
            crate::ordered_notification_groups::OrderedGroupRegistry::default(),
        ));
        {
            let mut registry = registry.lock().unwrap();
            registry
                .install(
                    9,
                    key,
                    OrderedCollection::new(
                        "connection".into(),
                        "database".into(),
                        SourceGeneration::Counter(8),
                        9,
                        vec![member],
                        8,
                        128,
                        4,
                    )
                    .unwrap(),
                )
                .unwrap();
            registry
                .with_collection(9, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(9, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
        let (sender, mut received) = mpsc::channel(8);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            registry.clone(),
            source.clone(),
            members.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        drop(received);
        source.admit(8, Some(membership), value(1, 42)).unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(
            matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Record(Record::Value { ordinal: 1, value, .. }) if value == [42])
        );
        let (sender, mut received) = mpsc::channel(8);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            registry.clone(),
            source.clone(),
            members,
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        assert!(
            matches!(received.recv().await, Some(RadioEvent::NotificationWithProvenance { value, .. }) if value == [42])
        );
        assert!(matches!(
            registry.lock().unwrap().poll(9).unwrap(),
            CollectionPoll::Empty
        ));
        assert!(source.peek_transfer().is_none());
        source.admit(8, Some(membership), value(1, 42)).unwrap();
        assert!(
            matches!(received.recv().await, Some(RadioEvent::NotificationWithProvenance { value, .. }) if value == [42])
        );
        assert!(
            matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Record(Record::Value { ordinal: 2, value, .. }) if value == [42])
        );
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn barrier_is_consumed_before_the_downstream_fence_is_visible() {
        let source = PeerNotificationIngress::new(8, 4, 32, 1);
        let members = Arc::new(Members::default());
        let barriers = Arc::new(Mutex::new(HashMap::new()));
        let (ack, _received_ack) = PeerSourceFenceAck::new();
        barriers.lock().unwrap().insert(42, ack);
        let (sender, mut received) = mpsc::channel(4);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            source.clone(),
            members,
            barriers,
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        source.barrier(8, 42).unwrap();
        let Some(RadioEvent::PeerNotificationSourceFence { ack }) = received.recv().await else {
            panic!("barrier fence expected")
        };
        source.close_after_barrier(8, 42).unwrap();
        ack.acknowledge();
        let Some(RadioEvent::PeerNotificationSourceFence { ack }) = received.recv().await else {
            panic!("terminal fence expected")
        };
        ack.acknowledge();
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn native_source_failure_keeps_ordered_metadata_through_peer_pump() {
        let source = PeerNotificationIngress::new(8, 16, 128, 1);
        let members = Arc::new(Members::default());
        members
            .register(target("a", 1), || source.register(8, route(1), 1))
            .unwrap();
        members
            .register(target("b", 2), || source.register(8, route(2), 2))
            .unwrap();
        let (sender, mut received) = mpsc::channel(8);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            source.clone(),
            members.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        source
            .fail_source(
                8,
                btleplug::PlatformError::new("native", "overflow", "source failed")
                    .with("originalMetadataStart", "2")
                    .with("nsErrorDomain", "first")
                    .with("nsErrorDomain", "second")
                    .with("originalMetadataStart", "collision"),
            )
            .unwrap();
        let Some(RadioEvent::PeerNotificationSourceFailed {
            error,
            members: snapshot,
            ..
        }) = received.recv().await
        else {
            panic!("source failure expected")
        };
        assert_eq!(snapshot.len(), 2);
        let platform = error.platform().unwrap();
        assert_eq!(
            platform.metadata.get("originalMetadataStart"),
            Some(&crate::errors::PlatformValue::Text("collision".into()))
        );
        let entries = platform.metadata_entries.as_ref().unwrap();
        let observed: Vec<_> = entries
            .iter()
            .map(|entry| (entry.key.as_str(), &entry.value))
            .collect();
        use crate::errors::PlatformValue::Text;
        assert_eq!(
            observed,
            vec![
                ("originalMetadataStart", &Text("2".into())),
                ("nsErrorDomain", &Text("first".into())),
                ("nsErrorDomain", &Text("second".into())),
                ("originalMetadataStart", &Text("collision".into())),
            ]
        );
        assert_eq!(error.operation(), "gatt.notification.source");
        let Some(RadioEvent::PeerNotificationSourceFence { ack }) = received.recv().await else {
            panic!("terminal fence expected")
        };
        ack.acknowledge();
        assert!(task.await.unwrap().is_ok());
        assert!(members.is_closed());
    }

    #[cfg(feature = "reclaim-test-support")]
    #[tokio::test]
    async fn vendor_source_group_failure_keeps_metadata_through_peer_pump() {
        let (source, consumer) = btleplug::platform::PeerGroupTestSource::new(8, 16);
        let members = Arc::new(Members::default());
        members
            .register(target("a", 1), || source.register(route(1), 1))
            .unwrap();
        members
            .register(target("b", 2), || source.register(route(2), 2))
            .unwrap();
        let mut failed = match value(1, 0) {
            NotificationEvent::Value(value) => value,
            _ => unreachable!(),
        };
        failed.source_failure = Some(
            btleplug::PlatformError::new("native", "overflow", "failure")
                .with("originalMetadataStart", "2")
                .with("nsErrorDomain", "first")
                .with("nsErrorDomain", "second")
                .with("originalMetadataStart", "collision"),
        );
        source.publish(1, failed).unwrap();
        let (sender, mut received) = mpsc::channel(8);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            consumer,
            members.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        let Some(RadioEvent::PeerNotificationSourceFailed {
            error,
            members: snapshot,
            ..
        }) = received.recv().await
        else {
            panic!("source failure expected")
        };
        assert_eq!(snapshot.len(), 2);
        let platform = error.platform().unwrap();
        let entries = platform.metadata_entries.as_ref().unwrap();
        let values: Vec<_> = entries
            .iter()
            .map(|entry| (entry.key.as_str(), &entry.value))
            .collect();
        use crate::errors::PlatformValue::Text;
        assert_eq!(
            values,
            vec![
                ("originalMetadataStart", &Text("2".into())),
                ("nsErrorDomain", &Text("first".into())),
                ("nsErrorDomain", &Text("second".into())),
                ("originalMetadataStart", &Text("collision".into()))
            ]
        );
        let Some(RadioEvent::PeerNotificationSourceFence { ack }) = received.recv().await else {
            panic!("terminal fence expected")
        };
        ack.acknowledge();
        assert!(task.await.unwrap().is_ok());
        assert!(members.is_closed());
    }

    #[tokio::test]
    async fn paused_pump_leaves_callbacks_in_native_fifo_until_resume() {
        let source = PeerNotificationIngress::new(8, 8, 128, 1);
        let members = Arc::new(Members::default());
        let member = members
            .register(target("a", 1), || source.register(8, route(1), 1))
            .unwrap();
        let (sender, mut received) = mpsc::channel(8);
        let (pause_tx, pause_rx) = tokio::sync::watch::channel(false);
        let (ack_tx, mut ack_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            source.clone(),
            members,
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Some((pause_rx, ack_tx)),
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { .. })
        ));
        source.admit(8, Some(member), value(1, 1)).unwrap();
        assert!(
            matches!(received.recv().await,Some(RadioEvent::NotificationWithProvenance{value,..}) if value==[1])
        );
        pause_tx.send_replace(true);
        while !*ack_rx.borrow_and_update() {
            ack_rx.changed().await.unwrap();
        }
        source.admit(8, Some(member), value(1, 2)).unwrap();
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        pause_tx.send_replace(false);
        assert!(
            matches!(received.recv().await,Some(RadioEvent::NotificationWithProvenance{value,..}) if value==[2])
        );
        while *ack_rx.borrow_and_update() {
            ack_rx.changed().await.unwrap();
        }
        pause_tx.send_replace(true);
        while !*ack_rx.borrow_and_update() {
            ack_rx.changed().await.unwrap();
        }
        source.admit(8, Some(member), value(1, 3)).unwrap();
        tokio::task::yield_now().await;
        assert!(received.try_recv().is_err());
        pause_tx.send_replace(false);
        assert!(
            matches!(received.recv().await,Some(RadioEvent::NotificationWithProvenance{value,..}) if value==[3])
        );
        task.abort();
    }

    #[tokio::test]
    async fn pump_keeps_peer_order_and_waits_for_central_fence() {
        let source = PeerNotificationIngress::new(8, 16, 128, 1);
        let members = Arc::new(Members::default());
        let first = members
            .register(target("a", 1), || source.register(8, route(1), 1))
            .unwrap();
        let second = members
            .register(target("b", 2), || source.register(8, route(2), 2))
            .unwrap();
        let (sender, mut received) = mpsc::channel(16);
        let queued = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(pump_group(
            "peer".into(),
            8,
            1,
            true,
            Arc::new(Mutex::new(
                crate::ordered_notification_groups::OrderedGroupRegistry::default(),
            )),
            source.clone(),
            members.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            sender,
            queued,
            Arc::new(AtomicU64::new(0)),
            None,
        ));
        assert!(matches!(
            received.recv().await,
            Some(RadioEvent::PeerNotificationSourceStarted { generation: 8, .. })
        ));
        source
            .admit_with_provenance(
                8,
                Some(first),
                value(1, 11),
                CallbackProvenance::Notification,
            )
            .unwrap();
        source
            .admit_with_provenance(
                8,
                Some(second),
                value(2, 22),
                CallbackProvenance::ReadOrNotification,
            )
            .unwrap();
        source
            .admit(8, None, NotificationEvent::Lagged { skipped_records: 3 })
            .unwrap();
        let fence = members
            .retire(first, || source.retire(8, route(1), first))
            .unwrap()
            .unwrap();
        assert!(
            matches!(received.recv().await, Some(RadioEvent::NotificationWithProvenance { epoch: 1, source_ordinal: 1, value, .. }) if value == [11])
        );
        assert!(
            matches!(received.recv().await, Some(RadioEvent::NotificationWithProvenance { epoch: 2, source_ordinal: 2, value, .. }) if value == [22])
        );
        assert!(
            matches!(received.recv().await, Some(RadioEvent::PeerNotificationsLost { lost: Some(3), members, .. }) if members.len() == 2)
        );
        let Some(RadioEvent::PeerNotificationSourceFence { ack }) = received.recv().await else {
            panic!("fence expected")
        };
        assert!(members.target(first).is_some());
        ack.acknowledge();
        fence.await.unwrap();
        assert!(members.target(first).is_none());
        assert!(members.target(second).is_some());
        task.abort();
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn failed_open_retains_generation_until_confirmed_retirement() {
        let mut state = SourceGeneration::default();
        let starts = AtomicUsize::new(0);
        let first = state
            .open(
                3,
                |_| async {
                    starts.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, &str>(())
                },
                |_| Err::<(), _>("open failed"),
            )
            .await;
        assert_eq!(first, Err(OpenGenerationError::Open("open failed")));
        let retry = state
            .open(
                3,
                |_| async {
                    panic!("started twice");
                    #[allow(unreachable_code)]
                    Ok::<_, &str>(())
                },
                |_| Ok::<_, &str>(7),
            )
            .await;
        assert_eq!(retry, Ok(7));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        let stale = state
            .open(4, |_| async { Ok::<_, &str>(()) }, |_| Ok::<_, &str>(()))
            .await;
        assert_eq!(stale, Err(OpenGenerationError::AlreadyOwned(3)));
        assert!(!state.retire(4));
        assert!(state.retire(3));
        assert_eq!(
            state
                .open(4, |_| async { Ok::<_, &str>(()) }, |_| Ok::<_, &str>(9))
                .await,
            Ok(9)
        );
    }
}

#[test]
fn registration_batch_blocks_mapping_until_registry_install_finishes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let members = Arc::new(Members::default());
    let installed = Arc::new(AtomicBool::new(false));
    let mut observer = None;
    let membership = members.batch(|batch| {
        let target = ForwardTarget {
            peer_id: "peer".into(),
            service_uuid: "service".into(),
            service_occurrence: 0,
            characteristic_uuid: "characteristic".into(),
            characteristic_occurrence: 0,
            epoch: 8,
        };
        let membership = batch
            .register(target, || {
                Ok::<_, ()>(Membership {
                    member: 5,
                    epoch: 9,
                })
            })
            .unwrap();
        let members = members.clone();
        let observed_install = installed.clone();
        observer = Some(std::thread::spawn(move || {
            assert!(members.target(membership).is_some());
            assert!(observed_install.load(Ordering::Acquire));
        }));
        installed.store(true, Ordering::Release);
        membership
    });
    observer.unwrap().join().unwrap();
    assert_eq!(members.target(membership).unwrap().epoch, 8);
}

#[tokio::test]
async fn pump_updates_collection_before_characteristic_fanout() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    use ubm_core::ordered_notifications::{
        DeliveryMode, Member, OrderedCollection, Poll as CollectionPoll, Record, SourceGeneration,
    };
    let source = PeerNotificationIngress::new(8, 16, 128, 1);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let members = Arc::new(Members::default());
    let target = ForwardTarget {
        peer_id: "peer".into(),
        service_uuid: "service".into(),
        service_occurrence: 0,
        characteristic_uuid: "characteristic".into(),
        characteristic_occurrence: 0,
        epoch: 4,
    };
    let membership = members
        .register(target, || source.register(8, route, 7))
        .unwrap();
    let registry = Arc::new(Mutex::new(
        crate::ordered_notification_groups::OrderedGroupRegistry::default(),
    ));
    let key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: ubm_core::ordered_notifications::SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: ubm_core::ordered_notifications::Membership {
            member: membership.member,
            epoch: membership.epoch,
        },
        delivery: DeliveryMode::Notification,
    };
    let owner = OrderedCollection::new(
        "connection".into(),
        "database".into(),
        SourceGeneration::Counter(8),
        9,
        vec![member],
        8,
        128,
        4,
    )
    .unwrap();
    {
        let mut registry = registry.lock().unwrap();
        registry.install(9, key, owner).unwrap();
        registry
            .with_collection(9, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(9, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        8,
        3,
        true,
        registry.clone(),
        source.clone(),
        members.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        None,
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    source
        .admit_with_provenance(
            8,
            Some(membership),
            NotificationEvent::Value(ValueNotification {
                uuid: uuid::Uuid::nil(),
                instance: 0,
                service_uuid: uuid::Uuid::nil(),
                service_instance: 0,
                value: vec![42],
                source_failure: None,
                lost_before: 0,
            }),
            CallbackProvenance::ReadOrNotification,
        )
        .unwrap();
    members
        .set_retirement_reason(ubm_core::ordered_notifications::EndReason::Invalidated)
        .unwrap();
    source.close(8, true).unwrap();
    assert!(
        matches!(receiver.recv().await, Some(RadioEvent::NotificationWithProvenance { source_ordinal: 1, value, .. }) if value == [42])
    );
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Record(Record::Value { ordinal: 1, member: 10, value, provenance: ubm_core::central::NotificationProvenance::ReadOrNotification, .. }) if value == [42])
    );
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceFailed {
            tail_uncertain: true,
            ..
        })
    ));
    let Some(RadioEvent::PeerNotificationSourceFence { ack }) = receiver.recv().await else {
        panic!("terminal fence expected")
    };
    assert!(!task.is_finished());
    ack.acknowledge();
    assert!(task.await.unwrap().is_ok());
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Terminal(terminal)
        if terminal.reason == ubm_core::ordered_notifications::EndReason::Invalidated && terminal.tail_uncertain)
    );
}

#[tokio::test]
async fn pump_holds_source_transfer_until_pressured_recipients_accept() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    use ubm_core::ordered_notifications::{
        DeliveryMode, Member, OrderedCollection, Poll as CollectionPoll, Record, SourceGeneration,
    };
    let ingress = PeerNotificationIngress::new(8, 16, 128, 1);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let members = Arc::new(Members::default());
    let target = ForwardTarget {
        peer_id: "peer".into(),
        service_uuid: "service".into(),
        service_occurrence: 0,
        characteristic_uuid: "characteristic".into(),
        characteristic_occurrence: 0,
        epoch: 4,
    };
    let membership = members
        .register(target, || ingress.register(8, route, 7))
        .unwrap();
    let key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: registry_membership(membership),
        delivery: DeliveryMode::Notification,
    };
    let registry = Arc::new(Mutex::new(
        crate::ordered_notification_groups::OrderedGroupRegistry::default(),
    ));
    for (id, limit) in [(9, 1), (10, 8)] {
        let mut registry = registry.lock().unwrap();
        registry
            .install(
                id,
                key.clone(),
                OrderedCollection::new(
                    "connection".into(),
                    "database".into(),
                    key.generation.clone(),
                    id,
                    vec![member],
                    8,
                    limit,
                    4,
                )
                .unwrap(),
            )
            .unwrap();
        registry
            .with_collection(id, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(id, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        8,
        3,
        true,
        registry.clone(),
        ingress.clone(),
        members,
        Arc::new(Mutex::new(HashMap::new())),
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        None,
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    ingress
        .admit_with_provenance(
            8,
            Some(membership),
            NotificationEvent::Value(ValueNotification {
                uuid: uuid::Uuid::nil(),
                instance: 0,
                service_uuid: uuid::Uuid::nil(),
                service_instance: 0,
                value: vec![1, 2],
                source_failure: None,
                lost_before: 0,
            }),
            CallbackProvenance::Notification,
        )
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                registry.lock().unwrap().poll(10).unwrap(),
                CollectionPoll::Record(Record::Value { ordinal: 1, .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        ingress.peek_transfer().is_some(),
        "the source must retain its transfer under pressure"
    );
    assert!(receiver.try_recv().is_err());
    assert!(matches!(
        registry.lock().unwrap().poll(10).unwrap(),
        CollectionPoll::Empty
    ));
    registry
        .lock()
        .unwrap()
        .with_collection(9, |owner| {
            owner.increase_byte_capacity(&key.generation, 9, 8)
        })
        .unwrap();
    assert!(
        matches!(tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv()).await.unwrap(), Some(RadioEvent::NotificationWithProvenance { value, .. }) if value == [1, 2])
    );
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Record(Record::Value { ordinal: 1, value, .. }) if value == [1, 2])
    );
    assert!(matches!(
        registry.lock().unwrap().poll(10).unwrap(),
        CollectionPoll::Empty
    ));
    task.abort();
}

#[tokio::test]
async fn source_accepted_value_precedes_close_even_when_the_pump_is_delayed() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    use ubm_core::ordered_notifications::{
        DeliveryMode, EndReason, Member, OrderedCollection, Poll as CollectionPoll, Record,
        SourceGeneration,
    };

    let ingress = PeerNotificationIngress::new(8, 16, 128, 1);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let members = Arc::new(Members::default());
    let target = ForwardTarget {
        peer_id: "peer".into(),
        service_uuid: "service".into(),
        service_occurrence: 0,
        characteristic_uuid: "characteristic".into(),
        characteristic_occurrence: 0,
        epoch: 4,
    };
    let membership = members
        .register(target, || ingress.register(8, route, 7))
        .unwrap();
    let key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: registry_membership(membership),
        delivery: DeliveryMode::Notification,
    };
    let registry = Arc::new(Mutex::new(
        crate::ordered_notification_groups::OrderedGroupRegistry::default(),
    ));
    {
        let mut registry = registry.lock().unwrap();
        registry
            .install(
                9,
                key,
                OrderedCollection::new(
                    "connection".into(),
                    "database".into(),
                    SourceGeneration::Counter(8),
                    9,
                    vec![member],
                    8,
                    128,
                    4,
                )
                .unwrap(),
            )
            .unwrap();
        registry
            .with_collection(9, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(9, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
    }
    ingress
        .admit_with_provenance(
            8,
            Some(membership),
            NotificationEvent::Value(ValueNotification {
                uuid: uuid::Uuid::nil(),
                instance: 0,
                service_uuid: uuid::Uuid::nil(),
                service_instance: 0,
                value: vec![42],
                source_failure: None,
                lost_before: 0,
            }),
            CallbackProvenance::Notification,
        )
        .unwrap();
    // Production cleanup places a source fence and waits for its downstream
    // acknowledgement before closing the collection. Keep the pump delayed
    // until after both the callback and the fence have entered the source.
    let barriers = Arc::new(Mutex::new(HashMap::new()));
    let (ack, acknowledged) = PeerSourceFenceAck::new();
    barriers.lock().unwrap().insert(42, ack);
    ingress.barrier(8, 42).unwrap();
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        8,
        3,
        true,
        registry.clone(),
        ingress,
        members,
        barriers,
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        None,
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    assert!(
        matches!(receiver.recv().await, Some(RadioEvent::NotificationWithProvenance { value, .. }) if value == [42])
    );
    let Some(RadioEvent::PeerNotificationSourceFence { ack }) = receiver.recv().await else {
        panic!("source fence expected")
    };
    ack.acknowledge();
    acknowledged.await.unwrap();
    registry
        .lock()
        .unwrap()
        .with_collection(9, |owner| owner.close(EndReason::Closed, false))
        .unwrap();
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Record(Record::Value { ordinal: 1, value, .. }) if value == [42])
    );
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Terminal(t) if t.reason == EndReason::Closed)
    );
    task.abort();
}

#[test]
fn collection_hook_preserves_source_rejected_value_bytes() {
    use ubm_core::ordered_notifications::{
        DeliveryMode, EndReason, Member, OrderedCollection, Poll as CollectionPoll,
        SourceGeneration, SourceRejection,
    };
    let native = Membership {
        member: 7,
        epoch: 2,
    };
    let source = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: ubm_core::ordered_notifications::SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: ubm_core::ordered_notifications::Membership {
            member: 7,
            epoch: 2,
        },
        delivery: DeliveryMode::Notification,
    };
    let owner = OrderedCollection::new(
        "connection".into(),
        "database".into(),
        SourceGeneration::Counter(8),
        9,
        vec![member],
        8,
        128,
        4,
    )
    .unwrap();
    let registry = Mutex::new(crate::ordered_notification_groups::OrderedGroupRegistry::default());
    registry
        .lock()
        .unwrap()
        .install(9, source.clone(), owner)
        .unwrap();
    let record = MappedRecord::Terminal {
        memberships: vec![native],
        members: Vec::new(),
        reason: TerminalReason::Overflow {
            rejected: RejectedRecord::Value { bytes: 37 },
            tail_uncertain: true,
        },
    };
    admit_to_collections(&registry, &source, &record, &Members::default()).unwrap();
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Terminal(terminal) if terminal.reason == EndReason::SourceOverflow && terminal.source_rejection == Some(SourceRejection::Value { bytes: 37 }) && terminal.tail_uncertain)
    );
}

#[tokio::test]
async fn pump_mapping_failure_retains_rejected_value_bytes() {
    use btleplug::api::{NotificationEvent, ValueNotification};
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    let source = PeerNotificationIngress::new(2, 8, 128, 0);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let member = source.register(2, route, 3).unwrap();
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        2,
        1,
        true,
        Arc::new(Mutex::new(
            crate::ordered_notification_groups::OrderedGroupRegistry::default(),
        )),
        source.clone(),
        Arc::new(Members::default()),
        Arc::new(Mutex::new(HashMap::new())),
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        None,
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    source
        .admit(
            2,
            Some(member),
            NotificationEvent::Value(ValueNotification {
                uuid: uuid::Uuid::nil(),
                instance: 0,
                service_uuid: uuid::Uuid::nil(),
                service_instance: 0,
                value: vec![1, 2, 3],
                source_failure: None,
                lost_before: 0,
            }),
        )
        .unwrap();
    let Err(failure) = task.await.unwrap() else {
        panic!("missing mapping must fail")
    };
    assert_eq!(
        failure.rejected,
        Some(ubm_core::ordered_notifications::SourceRejection::Value { bytes: 3 })
    );
}

#[test]
fn marker_ordinal_exhaustion_preserves_rejection_and_consumes_credit() {
    use ubm_core::ordered_notifications::{
        DeliveryMode, EndReason, Member, Membership as CoreMembership, OrderedCollection,
        Poll as CorePoll, SourceGeneration, SourceRejection,
    };
    let source = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(8),
        native_group: 3,
    };
    let membership = Membership {
        member: 7,
        epoch: 4,
    };
    let member = Member {
        id: 10,
        source: CoreMembership {
            member: 7,
            epoch: 4,
        },
        delivery: DeliveryMode::Notification,
    };
    let collection = OrderedCollection::new(
        "connection".into(),
        "database".into(),
        source.generation.clone(),
        9,
        vec![member],
        8,
        128,
        2,
    )
    .unwrap();
    let registry = Mutex::new(crate::ordered_notification_groups::OrderedGroupRegistry::default());
    {
        let mut registry = registry.lock().unwrap();
        registry.install(9, source.clone(), collection).unwrap();
        registry
            .with_collection(9, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(9, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
        registry.reserve_marker(9, &source, 0).unwrap();
    }
    let record = MappedRecord::Terminal {
        memberships: vec![membership],
        members: vec![],
        reason: TerminalReason::MarkerOrdinalExhausted {
            id: 0,
            tail_uncertain: true,
        },
    };
    admit_to_collections(&registry, &source, &record, &Members::default()).unwrap();
    let mut registry = registry.lock().unwrap();
    assert!(!registry.marker_reserved(9, &source, 0).unwrap());
    assert!(
        matches!(registry.poll(9).unwrap(), CorePoll::Terminal(terminal) if terminal.reason == EndReason::SourceOrdinalExhausted && terminal.source_rejection == Some(SourceRejection::Marker { marker_id: 0 }) && terminal.tail_uncertain)
    );
}

#[tokio::test]
async fn pump_retains_oversized_terminal_until_each_original_collection_accepts() {
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    use ubm_core::ordered_notifications::{
        DeliveryMode, EndReason, Member, OrderedCollection, Poll as CollectionPoll,
        SourceGeneration,
    };
    let ingress = PeerNotificationIngress::new(8, 16, 4096, 0);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let members = Arc::new(Members::default());
    let target = ForwardTarget {
        peer_id: "peer".into(),
        service_uuid: "service".into(),
        service_occurrence: 0,
        characteristic_uuid: "characteristic".into(),
        characteristic_occurrence: 0,
        epoch: 4,
    };
    let membership = members
        .register(target, || ingress.register(8, route, 7))
        .unwrap();
    let key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: registry_membership(membership),
        delivery: DeliveryMode::Notification,
    };
    let registry = Arc::new(Mutex::new(
        crate::ordered_notification_groups::OrderedGroupRegistry::default(),
    ));
    for (id, limit) in [(9, 128), (10, 4096)] {
        let owner = OrderedCollection::new(
            "connection".into(),
            "database".into(),
            key.generation.clone(),
            id,
            vec![member],
            8,
            limit,
            4,
        )
        .unwrap();
        let mut registry = registry.lock().unwrap();
        registry.install(id, key.clone(), owner).unwrap();
        registry
            .with_collection(id, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(id, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        8,
        3,
        true,
        registry.clone(),
        ingress.clone(),
        members.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        None,
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    ingress
        .fail_source(
            8,
            btleplug::PlatformError::new("test", "exact-code", "x".repeat(512)),
        )
        .unwrap();
    tokio::task::yield_now().await;
    assert!(matches!(
        registry.lock().unwrap().poll(9).unwrap(),
        CollectionPoll::Empty
    ));
    assert!(
        matches!(registry.lock().unwrap().poll(10).unwrap(), CollectionPoll::Terminal(t) if matches!(t.reason, EndReason::SourceFailed(ref f) if f.platform_code.as_deref() == Some("exact-code")))
    );
    assert!(
        receiver.try_recv().is_err(),
        "terminal fence must not be sent while evidence is refused"
    );
    // A later collection and a reconnect must not inherit the retained occurrence.
    let later = OrderedCollection::new(
        "connection".into(),
        "database".into(),
        key.generation.clone(),
        11,
        vec![member],
        8,
        4096,
        4,
    )
    .unwrap();
    let next_key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(9),
        native_group: 3,
    };
    let reconnected = OrderedCollection::new(
        "connection-next".into(),
        "database".into(),
        next_key.generation.clone(),
        12,
        vec![member],
        8,
        4096,
        4,
    )
    .unwrap();
    {
        let mut registry = registry.lock().unwrap();
        registry.install(11, key.clone(), later).unwrap();
        registry.install(12, next_key, reconnected).unwrap();
        for id in [11, 12] {
            registry
                .with_collection(id, |owner| owner.begin_enable(10))
                .unwrap();
            registry
                .with_collection(id, |owner| owner.enable_result(10, Some(true)))
                .unwrap();
        }
    }
    registry
        .lock()
        .unwrap()
        .with_collection(9, |owner| {
            owner.increase_byte_capacity(&key.generation, 9, 4096)
        })
        .unwrap();
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceFailed { .. })
    ));
    let Some(RadioEvent::PeerNotificationSourceFence { ack }) = receiver.recv().await else {
        panic!("terminal fence expected")
    };
    ack.acknowledge();
    assert!(task.await.unwrap().is_ok());
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Terminal(t) if matches!(t.reason, EndReason::SourceFailed(ref f) if f.platform_code.as_deref() == Some("exact-code")))
    );
    assert!(matches!(
        registry.lock().unwrap().poll(10).unwrap(),
        CollectionPoll::Closed
    ));
    for id in [11, 12] {
        assert!(matches!(
            registry.lock().unwrap().poll(id).unwrap(),
            CollectionPoll::Empty
        ));
    }
}

#[tokio::test]
async fn pump_retains_pending_terminal_across_collection_close() {
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    use ubm_core::ordered_notifications::{
        DeliveryMode, EndReason, Member, OrderedCollection, Poll as CollectionPoll,
        SourceGeneration,
    };
    let ingress = PeerNotificationIngress::new(8, 16, 4096, 0);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let members = Arc::new(Members::default());
    let target = ForwardTarget {
        peer_id: "peer".into(),
        service_uuid: "service".into(),
        service_occurrence: 0,
        characteristic_uuid: "characteristic".into(),
        characteristic_occurrence: 0,
        epoch: 4,
    };
    let membership = members
        .register(target, || ingress.register(8, route, 7))
        .unwrap();
    let key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: registry_membership(membership),
        delivery: DeliveryMode::Notification,
    };
    let registry = Arc::new(Mutex::new(
        crate::ordered_notification_groups::OrderedGroupRegistry::default(),
    ));
    for (id, limit) in [(9, 128), (10, 4096)] {
        let owner = OrderedCollection::new(
            "connection".into(),
            "database".into(),
            key.generation.clone(),
            id,
            vec![member],
            8,
            limit,
            4,
        )
        .unwrap();
        let mut registry = registry.lock().unwrap();
        registry.install(id, key.clone(), owner).unwrap();
        registry
            .with_collection(id, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(id, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        8,
        3,
        true,
        registry.clone(),
        ingress.clone(),
        members.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        None,
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    ingress
        .fail_source(
            8,
            btleplug::PlatformError::new("test", "exact-code", "x".repeat(512)),
        )
        .unwrap();
    tokio::task::yield_now().await;
    assert!(matches!(
        registry.lock().unwrap().poll(9).unwrap(),
        CollectionPoll::Empty
    ));
    assert!(
        matches!(registry.lock().unwrap().poll(10).unwrap(), CollectionPoll::Terminal(t) if matches!(t.reason, EndReason::SourceFailed(ref f) if f.platform_code.as_deref() == Some("exact-code")))
    );
    assert!(
        receiver.try_recv().is_err(),
        "terminal fence must not be sent while evidence is refused"
    );
    registry
        .lock()
        .unwrap()
        .with_collection(9, |owner| owner.close(EndReason::Closed, false))
        .unwrap();
    assert!(
        matches!(
            registry.lock().unwrap().poll(9).unwrap(),
            CollectionPoll::Empty
        ),
        "pending source evidence must precede the close terminal"
    );
    registry
        .lock()
        .unwrap()
        .with_collection(9, |owner| {
            owner.increase_byte_capacity(&key.generation, 9, 4096)
        })
        .unwrap();
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceFailed { .. })
    ));
    let Some(RadioEvent::PeerNotificationSourceFence { ack }) = receiver.recv().await else {
        panic!("terminal fence expected")
    };
    ack.acknowledge();
    assert!(task.await.unwrap().is_ok());
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Record(ubm_core::ordered_notifications::Record::FailureObservation { failure: Some(f), .. }) if f.platform_code.as_deref() == Some("exact-code"))
    );
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Terminal(t) if t.reason == EndReason::Closed)
    );
    assert!(matches!(
        registry.lock().unwrap().poll(10).unwrap(),
        CollectionPoll::Closed
    ));
}

#[tokio::test]
async fn pending_terminal_does_not_block_source_pause_and_resume() {
    use std::sync::{Arc, atomic::AtomicU64};
    use tokio::sync::mpsc;
    use ubm_core::ordered_notifications::{
        DeliveryMode, EndReason, Member, OrderedCollection, Poll as CollectionPoll,
        SourceGeneration,
    };
    let ingress = PeerNotificationIngress::new(8, 16, 4096, 0);
    let route = NotificationRoute {
        service_uuid: uuid::Uuid::nil(),
        service_instance: 0,
        characteristic_uuid: uuid::Uuid::nil(),
        characteristic_instance: 0,
    };
    let members = Arc::new(Members::default());
    let target = ForwardTarget {
        peer_id: "peer".into(),
        service_uuid: "service".into(),
        service_occurrence: 0,
        characteristic_uuid: "characteristic".into(),
        characteristic_occurrence: 0,
        epoch: 4,
    };
    let membership = members
        .register(target, || ingress.register(8, route, 7))
        .unwrap();
    let key = crate::ordered_notification_groups::SourceKey {
        peer_id: "peer".into(),
        generation: SourceGeneration::Counter(8),
        native_group: 3,
    };
    let member = Member {
        id: 10,
        source: registry_membership(membership),
        delivery: DeliveryMode::Notification,
    };
    let registry = Arc::new(Mutex::new(
        crate::ordered_notification_groups::OrderedGroupRegistry::default(),
    ));
    for (id, limit) in [(9, 128), (10, 4096)] {
        let owner = OrderedCollection::new(
            "connection".into(),
            "database".into(),
            key.generation.clone(),
            id,
            vec![member],
            8,
            limit,
            4,
        )
        .unwrap();
        let mut registry = registry.lock().unwrap();
        registry.install(id, key.clone(), owner).unwrap();
        registry
            .with_collection(id, |owner| owner.begin_enable(10))
            .unwrap();
        registry
            .with_collection(id, |owner| owner.enable_result(10, Some(true)))
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::channel(8);
    let (pause_request, pause_receiver) = tokio::sync::watch::channel(false);
    let (pause_ack_sender, mut pause_ack) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(pump_group(
        "peer".into(),
        8,
        3,
        true,
        registry.clone(),
        ingress.clone(),
        members.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        sender,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Some((pause_receiver, pause_ack_sender)),
    ));
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceStarted { .. })
    ));
    ingress
        .fail_source(
            8,
            btleplug::PlatformError::new("test", "exact-code", "x".repeat(512)),
        )
        .unwrap();
    tokio::task::yield_now().await;
    assert!(matches!(
        registry.lock().unwrap().poll(9).unwrap(),
        CollectionPoll::Empty
    ));
    assert!(
        matches!(registry.lock().unwrap().poll(10).unwrap(), CollectionPoll::Terminal(t) if matches!(t.reason, EndReason::SourceFailed(ref f) if f.platform_code.as_deref() == Some("exact-code")))
    );
    assert!(
        receiver.try_recv().is_err(),
        "terminal fence must not be sent while evidence is refused"
    );
    pause_request.send_replace(true);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !*pause_ack.borrow_and_update() {
            pause_ack.changed().await.unwrap();
        }
    })
    .await
    .expect("pump did not pause");
    pause_request.send_replace(false);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while *pause_ack.borrow_and_update() {
            pause_ack.changed().await.unwrap();
        }
    })
    .await
    .expect("pump did not resume");
    registry
        .lock()
        .unwrap()
        .with_collection(9, |owner| {
            owner.increase_byte_capacity(&key.generation, 9, 4096)
        })
        .unwrap();
    assert!(matches!(
        receiver.recv().await,
        Some(RadioEvent::PeerNotificationSourceFailed { .. })
    ));
    let Some(RadioEvent::PeerNotificationSourceFence { ack }) = receiver.recv().await else {
        panic!("terminal fence expected")
    };
    ack.acknowledge();
    assert!(task.await.unwrap().is_ok());
    assert!(
        matches!(registry.lock().unwrap().poll(9).unwrap(), CollectionPoll::Terminal(t) if matches!(t.reason, EndReason::SourceFailed(ref f) if f.platform_code.as_deref() == Some("exact-code")))
    );
    assert!(matches!(
        registry.lock().unwrap().poll(10).unwrap(),
        CollectionPoll::Closed
    ));
}
