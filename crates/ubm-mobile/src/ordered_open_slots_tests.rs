use super::{CoordinatorRegistry, Error, GroupMapping, OpenRequest};
use std::sync::{Arc, Barrier};

struct V2BoundaryRadio {
    owner: crate::OrderedAdmissionOwner,
    host: std::sync::OnceLock<std::sync::Weak<crate::MobileHost>>,
    requests: std::sync::Mutex<Vec<crate::RadioRequest>>,
    outcome: u8,
}

static TEST_DROPPED_RETIREMENT: std::sync::Mutex<Option<crate::OrderedSlotIdentity>> =
    std::sync::Mutex::new(None);

struct V2SessionRadio {
    owner: crate::OrderedAdmissionOwner,
    host: std::sync::OnceLock<std::sync::Weak<crate::MobileHost>>,
    requests: std::sync::Mutex<Vec<crate::RadioRequest>>,
}

impl crate::PlatformRadio for V2SessionRadio {
    fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
        Some(crate::OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn submit(&self, request: crate::RadioRequest) {
        use crate::{
            OrderedOpenCompletionV2 as Completion, OrderedOpenRequestV2 as Request, RadioCompletion,
        };
        self.requests.lock().unwrap().push(request.clone());
        let answer = match &request {
            crate::RadioRequest::OrderedOpenV2(Request::Open { .. }) => return,
            crate::RadioRequest::OrderedOpenV2(Request::Recover { identity, .. }) => {
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: crate::OrderedOpenOutcomeV2::Failed {
                        original_failure: crate::PlatformFailure::new(
                            crate::FailureKind::Platform,
                            "fenced",
                        ),
                        ownership: crate::OrderedOpenOwnershipV2::NotCreated,
                    },
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { identity, .. }) => {
                RadioCompletion::OrderedOpenV2(Completion::Retired(crate::OrderedOpenRetired {
                    slot_identity: identity.slot_identity(),
                }))
            }
            crate::RadioRequest::Discover { .. } => RadioCompletion::DiscoveredScoped {
                services: crate::test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "session-graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            },
            _ => match crate::test_common::polar_responder(&request) {
                crate::test_common::Reply::Now(answer) => answer,
                crate::test_common::Reply::Hold => return,
            },
        };
        self.host
            .get()
            .unwrap()
            .upgrade()
            .unwrap()
            .complete(request.id(), answer);
    }
    fn cancel(&self, _: u64) {}
}

impl crate::PlatformRadio for V2BoundaryRadio {
    fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
        Some(crate::OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn submit(&self, request: crate::RadioRequest) {
        use crate::{
            OrderedOpenCompletionV2 as Completion, OrderedOpenOutcomeV2 as Outcome,
            OrderedOpenOwnershipV2 as Ownership, OrderedOpenRequestV2 as Request, RadioCompletion,
        };
        self.requests.lock().unwrap().push(request.clone());
        if self.outcome == 9
            && let crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { identity, .. }) =
                &request
        {
            let mut retired = TEST_DROPPED_RETIREMENT.lock().unwrap();
            match *retired {
                None => {
                    *retired = Some(identity.slot_identity());
                    return;
                }
                Some(slot) => assert_eq!(slot, identity.slot_identity()),
            }
        }
        if ((self.outcome == 3 || self.outcome == 7 || self.outcome == 8)
            && matches!(
                request,
                crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { .. })
            ))
            || (self.outcome == 4
                && matches!(
                    request,
                    crate::RadioRequest::OrderedOpenV2(
                        crate::OrderedOpenRequestV2::Acknowledge { .. }
                    )
                ))
        {
            return;
        }
        let response = match &request {
            crate::RadioRequest::OrderedOpenV2(Request::Recover { identity, .. })
                if self.outcome == 7 || self.outcome == 8 =>
            {
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: Outcome::Failed {
                        original_failure: crate::PlatformFailure::new(
                            crate::FailureKind::Platform,
                            "fenced before open",
                        ),
                        ownership: Ownership::NotCreated,
                    },
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Recover { identity, .. })
                if self.outcome == 3 =>
            {
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: Outcome::Pending,
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Open { .. }) if self.outcome == 6 => {
                RadioCompletion::Failed(crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "transport failed",
                ))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Open { identity, .. }) => {
                let member = crate::OrderedRegistration {
                    member_id: 7,
                    source_member: 17,
                    epoch: 3,
                };
                let outcome = match self.outcome {
                    0 | 4 | 9 => Outcome::Opened {
                        source_generation: crate::OrderedSourceGeneration::Counter(9),
                        members: vec![member],
                    },
                    1 => Outcome::Failed {
                        original_failure: crate::PlatformFailure::new(
                            crate::FailureKind::Platform,
                            "original open failure",
                        ),
                        ownership: Ownership::NotCreated,
                    },
                    _ => Outcome::Failed {
                        original_failure: crate::PlatformFailure::new(
                            crate::FailureKind::Platform,
                            "original open failure",
                        ),
                        ownership: Ownership::Created {
                            source_generation: crate::OrderedSourceGeneration::Counter(9),
                            members: vec![member],
                        },
                    },
                };
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome,
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { .. })
                if self.outcome == 5 =>
            {
                RadioCompletion::Failed(crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "retirement failed",
                ))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { identity, .. }) => {
                RadioCompletion::OrderedOpenV2(Completion::Retired(crate::OrderedOpenRetired {
                    slot_identity: identity.slot_identity(),
                }))
            }
            crate::RadioRequest::OrderedRemove { .. }
            | crate::RadioRequest::OrderedForgetRemovalReceipt { .. } => RadioCompletion::Unit,
            crate::RadioRequest::Close { .. } => RadioCompletion::Closed(Vec::new()),
            _ => panic!("unexpected request: {request:?}"),
        };
        self.host
            .get()
            .unwrap()
            .upgrade()
            .unwrap()
            .complete(request.id(), response);
    }
    fn cancel(&self, _: u64) {}
}

#[tokio::test]
async fn abandoned_no_effect_open_keeps_cleanup_debt_after_native_retirement() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009117").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 8,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "debt-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    host.inner.stop_ordered_recovery_for_test();
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        host.inner
            .radio
            .abandoned_v2_opens()
            .iter()
            .any(|(id, _)| *id == 42)
    );
    host.inner
        .radio
        .recover_ordered_source("peer", 42)
        .await
        .unwrap();
    assert!(host.inner.radio.retained_ordered_open(42).is_none());
    assert!(
        host.inner
            .radio
            .abandoned_v2_opens()
            .iter()
            .any(|(id, _)| *id == 42),
        "central cleanup debt must survive native receipt retirement"
    );
    host.inner.radio.record_v2_recovery_error(
        42,
        ubm_desktop::errors::DesktopError::new(
            ubm_core::contracts::BleErrorCode::PlatformFailure,
            ubm_core::contracts::BleErrorDomain::Cleanup,
            "retained-central-cleanup",
        ),
    );
    let receipt = host.shutdown().await;
    assert!(receipt.contains("retained-central-cleanup"), "{receipt}");
}

#[tokio::test]
async fn v2_cancelled_open_is_recovered_by_host_without_a_caller() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009108").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 8,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "driver-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while host.inner.ordered_group(42).is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host recovery did not retire the canceled open");
    assert!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
    );
}

#[tokio::test]
async fn v2_lost_retirement_reply_is_retried_by_host() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009109").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 9,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "retry-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    assert!(
        TEST_DROPPED_RETIREMENT.lock().unwrap().is_some(),
        "native retired before its first reply was dropped"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !host
            .ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host did not retry retirement");
    let attempts: Vec<_> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|request| match request {
            crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge {
                id,
                identity,
            }) => Some((*id, identity.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].0, attempts[1].0);
    assert_eq!(attempts[0].1, attempts[1].1);
    assert!(
        host.inner
            .radio
            .retained_ordered_open(42)
            .unwrap()
            .acknowledged
    );
    assert!(
        host.inner.ordered_group(42).is_some(),
        "created source ownership must remain retained"
    );
}

#[tokio::test]
async fn v2_recovery_continues_beyond_attempt_bound_and_keeps_late_open_routable() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009112").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 3,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "bounded-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    for _ in 0..70 {
        assert!(matches!(
            host.inner
                .radio
                .recover_ordered_source("peer", 42)
                .await
                .unwrap(),
            ubm_desktop::boundary::OrderedSourceRecovery::Pending
        ));
    }
    let original = radio.requests.lock().unwrap()[0].clone();
    let crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { id, identity }) =
        original
    else {
        panic!("expected open")
    };
    assert_eq!(
        host.complete(
            id,
            crate::RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(
                crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: crate::OrderedOpenOutcomeV2::Opened {
                        source_generation: crate::OrderedSourceGeneration::Counter(9),
                        members: vec![crate::OrderedRegistration {
                            member_id: 7,
                            source_member: 17,
                            epoch: 3
                        }]
                    },
                }
            ))
        ),
        crate::CompletionStatus::Late
    );
    assert!(matches!(
        host.inner
            .radio
            .recover_ordered_source("peer", 42)
            .await
            .unwrap(),
        ubm_desktop::boundary::OrderedSourceRecovery::Ready(_)
    ));
    assert!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
    );
    assert!(host.inner.ordered_group(42).is_some());
}

#[tokio::test]
async fn v2_shutdown_cannot_claim_owner_quiescence_from_an_empty_map() {
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009110").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 0,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "quiescence-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let report: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
    assert_eq!(report["state"], "release-failed");
    assert!(!host.inner.ordered_wakes.is_closed());
}

#[tokio::test]
async fn v2_two_public_sessions_keep_distinct_native_groups_on_cancel() {
    use crate::test_common::{self as common, call, ok};
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009111").unwrap();
    let mut hosts = Vec::new();
    let mut radios = Vec::new();
    let mut sessions = Vec::new();
    let mut opens = Vec::new();
    let mut collections = Vec::new();
    for index in 0..2 {
        let radio = Arc::new(V2SessionRadio {
            owner,
            host: std::sync::OnceLock::new(),
            requests: std::sync::Mutex::new(Vec::new()),
        });
        let host = Arc::new(
            crate::MobileHost::open(
                radio.clone(),
                Arc::new(HostWake),
                crate::HostOptions {
                    platform: crate::MobilePlatform::Android,
                    owner: format!("session-{index}"),
                    adapter_label: "test".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
        let session = host.open_session("public-v2").unwrap();
        ok(&call(
            &session,
            "scan.start",
            r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
        )
        .await);
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: common::POLAR.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
        common::drain_until(&session, |records| {
            !common::of_type(records, "adv").is_empty()
        })
        .await;
        let connection = ok(&call(&session, "connection.connect", &serde_json::json!({"peerId": common::POLAR, "lease":"lease-1", "operationId":"connect"}).to_string()).await);
        let database = ok(&call(&session, "gatt.discover", &serde_json::json!({"peerId": common::POLAR, "lease":"lease-1", "operationId":"discover"}).to_string()).await);
        let reserved = ok(&call(
            &session,
            "gatt.ordered.reserve",
            &serde_json::json!({"peerId": common::POLAR}).to_string(),
        )
        .await);
        collections.push(reserved["collectionId"].clone());
        let args = serde_json::json!({"wireVersion":2,"peerId":common::POLAR,"collectionId":reserved["collectionId"],"connectionGeneration":connection["connectionGeneration"],"databaseGeneration":database["databaseGeneration"],"publicConnectionGeneration":"c","publicDatabaseGeneration":"d","members":[{"memberId":"0","publicMemberId":"a","selector":common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":128,"maxControls":8,"operationId":"ordered-open"}).to_string();
        let opening_session = session.clone();
        opens.push(tokio::spawn(async move {
            call(&opening_session, "gatt.ordered.open", &args).await
        }));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !radio.requests.lock().unwrap().iter().any(|request| {
                matches!(
                    request,
                    crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { .. })
                )
            }) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("public open did not reach native boundary");
        hosts.push(host);
        radios.push(radio);
        sessions.push(session);
    }
    let identities: Vec<_> = radios
        .iter()
        .map(|radio| {
            radio
                .requests
                .lock()
                .unwrap()
                .iter()
                .find_map(|request| match request {
                    crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open {
                        identity,
                        ..
                    }) => Some(identity.clone()),
                    _ => None,
                })
                .unwrap()
        })
        .collect();
    assert_eq!(collections[0], collections[1]);
    assert_ne!(identities[0].native_group_id, identities[1].native_group_id);
    assert_ne!(identities[0].slot, identities[1].slot);
    for session in &sessions {
        ok(&call(session, "op.cancel", r#"{"operationId":"ordered-open"}"#).await);
    }
    for opening in opens {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), opening)
            .await
            .expect("public cancel did not settle")
            .unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !hosts[0]
            .ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("canceled public opens did not retire");
}

#[tokio::test]
async fn v2_cancelled_open_recovers_with_exact_fence_and_retires() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009107").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 7,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "recover-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let recovered = host
        .inner
        .radio
        .recover_ordered_source("peer", 42)
        .await
        .unwrap();
    assert!(matches!(
        recovered,
        ubm_desktop::boundary::OrderedSourceRecovery::NotStarted
    ));
    let requests = radio.requests.lock().unwrap().clone();
    let [
        crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open {
            id: open_id,
            identity,
        }),
        crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Recover {
            id: recovery_id,
            identity: recovered,
        }),
        crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge {
            id: ack_id,
            identity: acknowledged,
        }),
    ] = requests.as_slice()
    else {
        panic!("unexpected requests: {requests:?}")
    };
    assert_eq!(identity, recovered);
    assert_eq!(identity, acknowledged);
    assert_ne!(open_id, recovery_id);
    assert_ne!(recovery_id, ack_id);
    assert!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
    );
    assert!(host.inner.ordered_group(42).is_none());
}

fn v2_source_request() -> ubm_desktop::boundary::OrderedSourceRequest {
    use ubm_desktop::boundary::{
        GattSnapshotIdentity, OrderedSourceMember, OrderedSourceRequest,
        PreparedNotificationActivation,
    };
    OrderedSourceRequest {
        peer_id: "peer".into(),
        snapshot_identity: Some(GattSnapshotIdentity {
            owner: "snapshot".into(),
            attachment: 2,
            revision: 3,
        }),
        connection_generation: "connection".into(),
        database_generation: "database".into(),
        collection_id: 42,
        members: vec![OrderedSourceMember {
            member_id: 7,
            activation: PreparedNotificationActivation {
                scope: (
                    "peer".into(),
                    "service".into(),
                    2,
                    "characteristic".into(),
                    3,
                ),
                activation_epoch: 4,
                requested: None,
                preferred: None,
            },
            delivery: ubm_core::ordered_notifications::DeliveryMode::Notification,
        }],
        max_items: 8,
        max_bytes: 128,
        max_controls: 8,
    }
}

async fn v2_boundary_open(outcome: u8) {
    use ubm_desktop::RadioBoundary;
    use ubm_desktop::boundary::OrderedSourceCleanup;
    let owner = crate::OrderedAdmissionOwner::negotiate_v2(&format!(
        "12345678-1234-4abc-8def-{:012x}",
        9000 + u64::from(outcome)
    ))
    .unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "v2-boundary".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let result = host
        .inner
        .radio
        .prepare_ordered_source(v2_source_request())
        .await;
    let coordinator = host.ordered_open_coordinator().unwrap().unwrap();
    let requests = radio.requests.lock().unwrap();
    let crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { id, identity }) =
        &requests[0]
    else {
        panic!("expected v2 open")
    };
    assert_eq!(*id, identity.original_request_id);
    assert_eq!(identity.peer_id, "peer");
    assert_eq!(
        identity.spec,
        crate::OrderedOpenSpec {
            connection_generation: "connection".into(),
            database_generation: "database".into(),
            snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "snapshot".into(),
                attachment: 2,
                revision: 3
            },
            members: vec![crate::OrderedMember {
                member_id: 7,
                instance: crate::Instance {
                    peer_id: "peer".into(),
                    service_uuid: "service".into(),
                    service_occurrence: 2,
                    characteristic_uuid: "characteristic".into(),
                    characteristic_occurrence: 3
                }
            }],
            item_capacity: 8,
            byte_capacity: 128,
        }
    );
    assert_eq!(
        identity.native_group_id,
        host.inner
            .ordered_group(42)
            .map_or(identity.native_group_id, |mapping| mapping.native_group_id)
    );
    assert!(
        matches!(&requests[1], crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge { identity: ack, .. }) if ack == identity)
    );
    assert!(coordinator.outstanding().is_empty());
    match (outcome, result) {
        (0, Ok(lease)) => {
            assert_eq!(lease.collection_id, 42);
            assert_eq!(lease.native_group, identity.native_group_id);
            assert_eq!(
                host.ingest(crate::RadioIngress::OrderedWake {
                    group_id: identity.native_group_id,
                    generation: lease.source_generation.clone()
                }),
                crate::IngressStatus::Accepted
            );
            host.inner.attach_ordered_source(&lease).unwrap();
        }
        (1, Err(failure)) => {
            assert!(failure.cleanup.is_none());
            assert_eq!(
                failure.error.detail(),
                Some("platform: original open failure")
            );
            assert!(host.inner.ordered_group(42).is_none());
        }
        (2, Err(failure)) => {
            assert_eq!(
                failure.error.detail(),
                Some("platform: original open failure")
            );
            assert!(
                matches!(failure.cleanup, Some(OrderedSourceCleanup::Lease(lease)) if lease.collection_id == 42 && lease.native_group == identity.native_group_id)
            );
        }
        (_, other) => panic!("unexpected result: {other:?}"),
    }
}

#[tokio::test]
async fn v2_open_uses_real_host_radio_boundary() {
    v2_boundary_open(0).await;
}
#[tokio::test]
async fn v2_no_effect_uses_real_host_radio_boundary() {
    v2_boundary_open(1).await;
}
#[tokio::test]
async fn v2_failed_created_uses_real_host_radio_boundary() {
    v2_boundary_open(2).await;
}

#[tokio::test]
async fn v2_cancelled_open_retains_late_exact_ownership() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009003").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 3,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "cancelled-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    let request = radio.requests.lock().unwrap()[0].clone();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { id, identity }) =
        request
    else {
        panic!("expected v2 open")
    };
    let result = crate::RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(
        crate::OrderedOpenResultV2 {
            slot_identity: identity.slot_identity(),
            outcome: crate::OrderedOpenOutcomeV2::Opened {
                source_generation: crate::OrderedSourceGeneration::Counter(9),
                members: vec![crate::OrderedRegistration {
                    member_id: 7,
                    source_member: 17,
                    epoch: 3,
                }],
            },
        },
    ));
    assert_eq!(
        host.complete(id, result.clone()),
        crate::CompletionStatus::Late
    );
    let outstanding = host
        .ordered_open_coordinator()
        .unwrap()
        .unwrap()
        .outstanding();
    assert_eq!(outstanding.len(), 1);
    assert_eq!(outstanding[0].disposition, Some(result));
    assert!(matches!(
        host.inner
            .radio
            .retained_ordered_open(42)
            .unwrap()
            .completion,
        Some(crate::RadioCompletion::OrderedOpened { .. })
    ));
    assert!(host.inner.ordered_group(42).is_some());
}

#[tokio::test]
async fn v2_untyped_failure_keeps_debt_for_late_exact_open() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009009").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 6,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "untyped-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let failure = host
        .inner
        .radio
        .prepare_ordered_source(v2_source_request())
        .await
        .unwrap_err();
    assert!(matches!(
        failure.cleanup,
        Some(ubm_desktop::boundary::OrderedSourceCleanup::Indeterminate { .. })
    ));
    let request = radio.requests.lock().unwrap()[0].clone();
    let crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { id, identity }) =
        request
    else {
        panic!("expected v2 open")
    };
    let result = crate::RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Result(
        crate::OrderedOpenResultV2 {
            slot_identity: identity.slot_identity(),
            outcome: crate::OrderedOpenOutcomeV2::Opened {
                source_generation: crate::OrderedSourceGeneration::Counter(9),
                members: vec![crate::OrderedRegistration {
                    member_id: 7,
                    source_member: 17,
                    epoch: 3,
                }],
            },
        },
    ));
    assert_eq!(
        host.complete(id, result.clone()),
        crate::CompletionStatus::Late
    );
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()[0]
            .disposition,
        Some(result)
    );
    assert!(matches!(
        host.inner
            .radio
            .retained_ordered_open(42)
            .unwrap()
            .completion,
        Some(crate::RadioCompletion::OrderedOpened { .. })
    ));
}

#[tokio::test]
async fn v2_lost_retirement_never_returns_credit() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009004").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 4,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "lost-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let request = radio.requests.lock().unwrap()[1].clone();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Acknowledge {
        id,
        identity,
    }) = request
    else {
        panic!("expected acknowledgement")
    };
    let mut wrong = identity.slot_identity();
    wrong.slot_generation += 1;
    assert_eq!(
        host.complete(
            id,
            crate::RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Retired(
                crate::OrderedOpenRetired {
                    slot_identity: wrong
                }
            ))
        ),
        crate::CompletionStatus::Mismatched
    );
    assert_eq!(
        host.complete(
            id,
            crate::RadioCompletion::OrderedOpenV2(crate::OrderedOpenCompletionV2::Retired(
                crate::OrderedOpenRetired {
                    slot_identity: identity.slot_identity()
                }
            ))
        ),
        crate::CompletionStatus::Late
    );
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .len(),
        1
    );
    assert!(
        !host
            .inner
            .radio
            .retained_ordered_open(42)
            .unwrap()
            .acknowledged
    );
}

#[tokio::test]
async fn v2_raw_open_cannot_bypass_coordinator() {
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009005").unwrap();
    let platform = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 3,
    });
    let radio = crate::ForeignRadio::new(
        platform.clone(),
        crate::MobilePlatform::Android,
        "test".into(),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(25),
        radio.call(|id| {
            crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open {
                id,
                identity: crate::OrderedOpenIdentity {
                    version: 2,
                    owner_incarnation: owner,
                    slot: 0,
                    slot_generation: 1,
                    original_request_id: id,
                    native_group_id: 1,
                    peer_id: "peer".into(),
                    spec: mobile_request(1).fingerprint,
                },
            })
        }),
    )
    .await;
    assert!(
        matches!(result, Ok(Err(_))),
        "uncoordinated open must be rejected before dispatch"
    );
    assert!(platform.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn v2_failed_created_release_forgets_only_after_native_removal() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009006").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 2,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "release-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let failure = host
        .inner
        .radio
        .prepare_ordered_source(v2_source_request())
        .await
        .unwrap_err();
    let cleanup = failure.cleanup.unwrap();
    let mapping = host.inner.ordered_group(42).unwrap();
    assert_eq!(
        host.inner
            .radio
            .release_ordered_source(&cleanup)
            .await
            .unwrap(),
        ubm_desktop::boundary::OrderedSourceRelease::Released
    );
    let requests = radio.requests.lock().unwrap();
    assert!(
        matches!(&requests[2], crate::RadioRequest::OrderedRemove { group_id, .. } if *group_id == mapping.native_group_id)
    );
    assert!(
        matches!(&requests[3], crate::RadioRequest::OrderedForgetRemovalReceipt { group_id, .. } if *group_id == mapping.native_group_id)
    );
    assert!(host.inner.ordered_group(42).is_none());
    assert!(host.inner.radio.retained_ordered_open(42).is_none());
}

#[tokio::test]
async fn v2_failed_created_preserves_original_error_when_retirement_fails() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009007").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 5,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "failure-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let failure = host
        .inner
        .radio
        .prepare_ordered_source(v2_source_request())
        .await
        .unwrap_err();
    assert_eq!(
        failure.error.detail(),
        Some("platform: original open failure")
    );
    assert!(matches!(
        failure.cleanup,
        Some(ubm_desktop::boundary::OrderedSourceCleanup::Lease(_))
    ));
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .len(),
        1
    );
    assert!(
        !host
            .inner
            .radio
            .retained_ordered_open(42)
            .unwrap()
            .acknowledged
    );
}

#[tokio::test]
async fn v2_shutdown_reports_unresolved_open_debt() {
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009008").unwrap();
    let radio = Arc::new(V2BoundaryRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        outcome: 3,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "shutdown-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let opening_host = host.clone();
    let task = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let report: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
    assert_eq!(report["state"], "release-failed");
    let mapping = host.inner.ordered_group(42).unwrap();
    assert_eq!(
        host.ingest(crate::RadioIngress::OrderedWake {
            generation: crate::OrderedSourceGeneration::Counter(9),
            group_id: mapping.native_group_id,
        }),
        crate::IngressStatus::Accepted
    );
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .len(),
        1
    );
}

#[test]
fn owner_wide_groups_distinguish_hosts_and_never_recycle() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let a = registry
        .attach_or_register([21; 16], 2, Arc::new(()))
        .unwrap();
    let b = registry
        .attach_or_register([21; 16], 2, Arc::new(()))
        .unwrap();
    let other = registry.register([22; 16], 2).unwrap();
    let host_a = a.register_host().unwrap();
    let host_b = b.register_host().unwrap();
    let first = a.allocate_group(host_a, 0).unwrap();
    let second = b.allocate_group(host_b, 0).unwrap();
    assert_ne!(first.native_group_id, second.native_group_id);
    assert_eq!(a.group(host_a, 0), Some(first));
    assert_eq!(a.allocate_group(host_a, 0), Err(Error::GroupAlreadyMapped));
    assert!(other.group(host_a, 0).is_none());
    assert_eq!(
        a.forget_unsubmitted_group(second, first.native_group_id),
        Err(Error::IdentityMismatch)
    );
    a.forget_unsubmitted_group(first, first.native_group_id)
        .unwrap();
    let next = a.allocate_group(host_a, 0).unwrap();
    assert_ne!(next.native_group_id, first.native_group_id);
    assert_eq!(a.group(host_b, 0), Some(second));
}

#[test]
fn v2_exact_identity_and_retirement_are_checked() {
    use crate::{OrderedAdmissionOwner, OrderedOpenIdentity, OrderedOpenSpec, OrderedSlotIdentity};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-123456780001").unwrap();
    assert_eq!(owner.to_string(), "12345678-1234-4abc-8def-123456780001");
    let identity = OrderedOpenIdentity {
        version: 2,
        owner_incarnation: owner,
        slot: 0,
        slot_generation: 1,
        original_request_id: 5,
        native_group_id: 9,
        peer_id: "Peer".into(),
        spec: OrderedOpenSpec {
            connection_generation: "c".into(),
            database_generation: "d".into(),
            snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "o".into(),
                attachment: 1,
                revision: 2,
            },
            members: vec![crate::OrderedMember {
                member_id: 8,
                instance: crate::Instance {
                    peer_id: "Peer".into(),
                    service_uuid: "s".into(),
                    service_occurrence: 1,
                    characteristic_uuid: "c".into(),
                    characteristic_occurrence: 2,
                },
            }],
            item_capacity: 2,
            byte_capacity: 32,
        },
    };
    assert!(identity.validate().is_ok());
    let mut different = identity.clone();
    different.spec.members[0].instance.peer_id = "peer".into();
    assert!(different.validate().is_err());
    different = identity.clone();
    different.spec.members[0].instance.characteristic_occurrence += 1;
    assert_ne!(different, identity);
    different.slot = 64;
    assert!(different.validate().is_err());
    let slot = OrderedSlotIdentity {
        owner_incarnation: owner,
        slot: 0,
        slot_generation: 1,
    };
    assert_eq!(identity.slot_identity(), slot);
    let registry = CoordinatorRegistry::<crate::OrderedOpenSpec, String>::default();
    let coordinator = registry.register(owner.bytes(), 1).unwrap();
    let host = coordinator.register_host().unwrap();
    let mapping = coordinator.allocate_group(host, 41).unwrap();
    let request = OpenRequest {
        request_id: 5,
        group_id: mapping.native_group_id,
        peer: identity.peer_id.clone(),
        fingerprint: identity.spec.clone(),
        local_group: Some(mapping),
    };
    let lease = coordinator.acquire(request).unwrap();
    let wrong_owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-123456780002").unwrap();
    assert_eq!(
        coordinator.begin_dispatch_v2(&lease, wrong_owner),
        Err(Error::IdentityMismatch)
    );
    assert!(!coordinator.outstanding()[0].dispatched);
    let wire = coordinator.begin_dispatch_v2(&lease, owner).unwrap();
    assert_eq!(wire.spec, identity.spec);
    assert_eq!(wire.native_group_id, mapping.native_group_id);
    assert_eq!(lease.request.local_group.unwrap().collection_id, 41);
    assert_eq!(
        lease.wire_identity(wrong_owner),
        Err(Error::IdentityMismatch)
    );
    assert!(
        OrderedAdmissionOwner::negotiate_slot(1, "12345678-1234-4abc-8def-123456780001").is_err()
    );
    assert!(
        OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-12345678000A").is_err()
    );
    let open = crate::OrderedOpenRequestV2::Open {
        id: 5,
        identity: identity.clone(),
    };
    let retry = crate::OrderedOpenRequestV2::Recover {
        id: 6,
        identity: identity.clone(),
    };
    let ack = crate::OrderedOpenRequestV2::Acknowledge {
        id: 7,
        identity: identity.clone(),
    };
    let result = crate::OrderedOpenCompletionV2::Result(crate::OrderedOpenResultV2 {
        slot_identity: slot,
        outcome: crate::OrderedOpenOutcomeV2::Pending,
    });
    if let crate::OrderedOpenCompletionV2::Result(pending) = &result {
        assert_eq!(pending.terminal_for(&identity), None);
    }
    let retired = crate::OrderedOpenCompletionV2::Retired(crate::OrderedOpenRetired {
        slot_identity: slot,
    });
    assert!(result.answers(&open));
    assert!(result.answers(&retry));
    assert!(!result.answers(&ack));
    assert!(retired.answers(&ack));
    assert!(!retired.answers(&retry));
    let wrong = crate::OrderedOpenCompletionV2::Retired(crate::OrderedOpenRetired {
        slot_identity: crate::OrderedSlotIdentity {
            slot_generation: 2,
            ..slot
        },
    });
    assert!(!wrong.answers(&ack));
    let bad_attempt = crate::OrderedOpenRequestV2::Recover {
        id: 5,
        identity: identity.clone(),
    };
    assert!(!result.answers(&bad_attempt));
    let failure = crate::PlatformFailure::new(crate::FailureKind::Platform, "installation failed");
    let ownership = crate::OrderedOpenOwnershipV2::Created {
        source_generation: crate::OrderedSourceGeneration::Counter(8),
        members: vec![],
    };
    let failed = crate::OrderedOpenResultV2 {
        slot_identity: slot,
        outcome: crate::OrderedOpenOutcomeV2::Failed {
            original_failure: failure.clone(),
            ownership: ownership.clone(),
        },
    };
    assert_eq!(
        failed.terminal_for(&identity),
        Some(crate::OrderedOpenTerminalV2::Failed {
            original_failure: failure,
            ownership
        })
    );
}

#[test]
fn group_mapping_survives_dispatch_until_positive_cleanup_proof() {
    let registry = CoordinatorRegistry::<Vec<u8>, crate::OrderedOpenTerminalV2>::default();
    let owner = registry.register([23; 16], 1).unwrap();
    let host = owner.register_host().unwrap();
    let mapping = owner.allocate_group(host, 0).unwrap();
    let mut req = request(7);
    req.group_id = mapping.native_group_id;
    req.local_group = Some(mapping);
    let lease = owner.acquire(req).unwrap();
    assert_eq!(
        owner.forget_unsubmitted_group(mapping, mapping.native_group_id),
        Err(Error::OwnerBusy)
    );
    owner.begin_dispatch(&lease).unwrap();
    owner
        .retain_terminal(
            &lease,
            lease.identity,
            crate::OrderedOpenTerminalV2::Failed {
                original_failure: crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "fenced",
                ),
                ownership: crate::OrderedOpenOwnershipV2::NotCreated,
            },
        )
        .unwrap();
    owner
        .confirm_retired(&lease, retirement_for_test(lease.identity))
        .unwrap();
    assert_eq!(
        owner.forget_unsubmitted_group(mapping, mapping.native_group_id),
        Err(Error::AlreadyDispatched)
    );
    assert_eq!(owner.group(host, 0), Some(mapping));
    owner
        .forget_no_effect_group(mapping, lease.identity)
        .unwrap();
    assert_eq!(owner.group(host, 0), None);
}

#[test]
fn radio_no_effect_forgetting_requires_matching_retired_slot_and_outcome() {
    use crate::{
        OrderedOpenCompletionV2, OrderedOpenOutcomeV2, OrderedOpenOwnershipV2, OrderedOpenResultV2,
        RadioCompletion,
    };
    for case in ["wrong-generation", "wrong-owner", "created", "not-created"] {
        let registry = CoordinatorRegistry::<Vec<u8>, RadioCompletion>::default();
        let owner = registry.register([23; 16], 1).unwrap();
        let host = owner.register_host().unwrap();
        let mapping = owner.allocate_group(host, 0).unwrap();
        let mut req = request(7);
        req.group_id = mapping.native_group_id;
        req.local_group = Some(mapping);
        let lease = owner.acquire(req).unwrap();
        owner.begin_dispatch(&lease).unwrap();
        let mut echoed = retirement_for_test(lease.identity).slot_identity;
        if case == "wrong-generation" {
            echoed.slot_generation += 1;
        }
        if case == "wrong-owner" {
            echoed.owner_incarnation = crate::OrderedAdmissionOwner::from_test_bytes([24; 16]);
        }
        let ownership = if case == "created" {
            OrderedOpenOwnershipV2::Created {
                source_generation: crate::OrderedSourceGeneration::Counter(4),
                members: vec![],
            }
        } else {
            OrderedOpenOwnershipV2::NotCreated
        };
        let disposition =
            RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Result(OrderedOpenResultV2 {
                slot_identity: echoed,
                outcome: OrderedOpenOutcomeV2::Failed {
                    original_failure: crate::PlatformFailure::new(
                        crate::FailureKind::Platform,
                        "fenced",
                    ),
                    ownership,
                },
            }));
        owner
            .retain_terminal(&lease, lease.identity, disposition)
            .unwrap();
        owner
            .confirm_retired(&lease, retirement_for_test(lease.identity))
            .unwrap();
        let result = owner.forget_no_effect_radio_group(mapping, lease.identity);
        assert_eq!(
            result,
            if case == "not-created" {
                Ok(())
            } else {
                Err(Error::IdentityMismatch)
            }
        );
        assert_eq!(owner.group(host, 0).is_none(), case == "not-created");
    }
}

#[test]
fn installed_group_remains_after_slot_retirement_until_removal_receipt_is_forgotten() {
    let registry = CoordinatorRegistry::<Vec<u8>, crate::OrderedOpenTerminalV2>::default();
    let owner = registry.register([25; 16], 1).unwrap();
    let host = owner.register_host().unwrap();
    let mapping = owner.allocate_group(host, 0).unwrap();
    assert_eq!(
        owner.forget_removed_group(mapping, mapping.native_group_id),
        Err(Error::NotTerminal)
    );
    let mut req = request(3);
    req.group_id = mapping.native_group_id;
    req.local_group = Some(mapping);
    let lease = owner.acquire(req.clone()).unwrap();
    assert_eq!(owner.acquire(req), Err(Error::GroupAlreadyMapped));
    owner.begin_dispatch(&lease).unwrap();
    owner
        .retain_terminal(
            &lease,
            lease.identity,
            crate::OrderedOpenTerminalV2::Failed {
                original_failure: crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "failed after install",
                ),
                ownership: crate::OrderedOpenOwnershipV2::Created {
                    source_generation: crate::OrderedSourceGeneration::Counter(4),
                    members: vec![],
                },
            },
        )
        .unwrap();
    owner
        .confirm_retired(&lease, retirement_for_test(lease.identity))
        .unwrap();
    assert_eq!(
        owner.forget_no_effect_group(mapping, lease.identity),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(owner.group(host, 0), Some(mapping));
    assert_eq!(
        owner.forget_removed_group(mapping, mapping.native_group_id + 1),
        Err(Error::IdentityMismatch)
    );
    owner
        .forget_removed_group(mapping, mapping.native_group_id)
        .unwrap();
    assert!(owner.group(host, 0).is_none());
}

#[test]
fn native_group_and_host_allocators_fail_closed_at_exhaustion() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([26; 16], 1).unwrap();
    let host = owner.register_host().unwrap();
    {
        let mut state = owner.state.lock().unwrap();
        state.next_group = u64::MAX;
        state.next_host = u64::MAX;
    }
    assert_eq!(owner.allocate_group(host, 0), Err(Error::Exhausted));
    assert_eq!(owner.register_host(), Err(Error::Exhausted));
    assert!(owner.group(host, 0).is_none());
}

#[test]
fn fenced_no_effect_result_wins_over_late_original_callback() {
    let registry = CoordinatorRegistry::<Vec<u8>, crate::OrderedOpenTerminalV2>::default();
    let owner = registry.register([27; 16], 1).unwrap();
    let old = owner.acquire(request(1)).unwrap();
    owner.begin_dispatch(&old).unwrap();
    let fenced = crate::OrderedOpenTerminalV2::Failed {
        original_failure: crate::PlatformFailure::new(
            crate::FailureKind::Platform,
            "fenced before execution",
        ),
        ownership: crate::OrderedOpenOwnershipV2::NotCreated,
    };
    owner
        .retain_terminal(&old, old.identity, fenced.clone())
        .unwrap();
    let late = crate::OrderedOpenTerminalV2::Opened {
        source_generation: crate::OrderedSourceGeneration::Counter(1),
        members: vec![],
    };
    assert_eq!(
        owner.retain_terminal(&old, old.identity, late.clone()),
        Err(Error::ConflictingDisposition)
    );
    assert_eq!(owner.outstanding()[0].disposition, Some(fenced));
    owner
        .confirm_retired(&old, retirement_for_test(old.identity))
        .unwrap();
    let current = owner.acquire(request(2)).unwrap();
    owner.begin_dispatch(&current).unwrap();
    assert_eq!(
        owner.retain_terminal(&old, old.identity, late),
        Err(Error::IdentityMismatch)
    );
    assert!(owner.outstanding()[0].disposition.is_none());
}

#[test]
fn only_exact_typed_retirement_releases_credit() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([24; 16], 1).unwrap();
    let lease = owner.acquire(request(1)).unwrap();
    owner.begin_dispatch(&lease).unwrap();
    owner
        .retain_terminal(&lease, lease.identity, "done".into())
        .unwrap();
    let receipt = retirement_for_test(lease.identity);
    let wrong = crate::OrderedOpenRetired {
        slot_identity: crate::OrderedSlotIdentity {
            slot_generation: receipt.slot_identity.slot_generation + 1,
            ..receipt.slot_identity
        },
    };
    assert_eq!(
        owner.confirm_retired(&lease, wrong),
        Err(Error::IdentityMismatch)
    );
    let wrong_slot = crate::OrderedOpenRetired {
        slot_identity: crate::OrderedSlotIdentity {
            slot: 1,
            ..receipt.slot_identity
        },
    };
    assert_eq!(
        owner.confirm_retired(&lease, wrong_slot),
        Err(Error::IdentityMismatch)
    );
    let wrong_owner = crate::OrderedOpenRetired {
        slot_identity: crate::OrderedSlotIdentity {
            owner_incarnation: crate::OrderedAdmissionOwner::from_test_bytes([25; 16]),
            ..receipt.slot_identity
        },
    };
    assert_eq!(
        owner.confirm_retired(&lease, wrong_owner),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(owner.acquire(request(2)), Err(Error::Capacity));
    owner.confirm_retired(&lease, receipt).unwrap();
    assert!(owner.acquire(request(2)).is_ok());
}

fn retirement_for_test(identity: super::SlotIdentity) -> crate::OrderedOpenRetired {
    crate::OrderedOpenRetired {
        slot_identity: crate::OrderedSlotIdentity {
            owner_incarnation: crate::OrderedAdmissionOwner::from_test_bytes(identity.incarnation),
            slot: identity.slot,
            slot_generation: identity.generation,
        },
    }
}

fn request(id: u64) -> OpenRequest<Vec<u8>> {
    OpenRequest {
        request_id: id,
        group_id: id + 10,
        peer: format!("peer-{id}"),
        fingerprint: vec![id as u8, 42],
        local_group: None,
    }
}

#[test]
fn retirement_with_debt_rejects_new_attachments() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([91; 16], 1).unwrap();
    let lease = owner.acquire(request(1)).unwrap();
    owner.begin_dispatch(&lease).unwrap();
    registry.begin_retirement(&owner).unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&owner),
        Err(Error::OwnerBusy)
    );
    assert!(matches!(
        registry.attach([91; 16]),
        Err(Error::OwnerRetired)
    ));
    assert!(matches!(
        owner.acquire(request(2)),
        Err(Error::OwnerRetired)
    ));
}

#[test]
fn quiescence_must_follow_retirement_fence() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([94; 16], 1).unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&owner),
        Err(Error::OwnerBusy)
    );
    assert!(registry.attach([94; 16]).is_ok());
    registry.begin_retirement(&owner).unwrap();
    registry.retire_after_native_quiescence(&owner).unwrap();
}

#[test]
fn retirement_fences_previously_reserved_dispatch() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([97; 16], 1).unwrap();
    let lease = owner.acquire(request(1)).unwrap();
    registry.begin_retirement(&owner).unwrap();
    assert_eq!(owner.begin_dispatch(&lease), Err(Error::OwnerRetired));
    owner.release_unsubmitted(&lease).unwrap();
    registry.retire_after_native_quiescence(&owner).unwrap();
}

#[test]
fn registry_releases_cleanup_anchor_only_after_quiescence_and_settlement() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let anchor = Arc::new(());
    let weak = Arc::downgrade(&anchor);
    let owner = registry
        .attach_or_register([92; 16], 1, anchor.clone())
        .unwrap();
    drop(anchor);
    let lease = owner.acquire(request(1)).unwrap();
    owner.begin_dispatch(&lease).unwrap();
    registry.begin_retirement(&owner).unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&owner),
        Err(Error::OwnerBusy)
    );
    assert!(weak.upgrade().is_some());
    owner
        .retain_terminal(&lease, lease.identity, "cleaned".into())
        .unwrap();
    owner
        .confirm_retired(&lease, retirement_for_test(lease.identity))
        .unwrap();
    assert!(
        weak.upgrade().is_some(),
        "settled debt alone is not quiescence"
    );
    let retained: Arc<dyn std::any::Any + Send + Sync> = weak.upgrade().unwrap();
    registry.confirm_cleanup(&owner, &retained).unwrap();
    drop(retained);
    registry.retire_after_native_quiescence(&owner).unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn quiescence_does_not_discard_unsettled_host_cleanup() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([93; 16], 1).unwrap();
    let host: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    registry.add_anchor(&owner, host.clone()).unwrap();
    registry.begin_retirement(&owner).unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&owner),
        Err(Error::OwnerBusy)
    );
    let other: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    assert_eq!(
        registry.confirm_cleanup(&owner, &other),
        Err(Error::IdentityMismatch)
    );
    registry.confirm_cleanup(&owner, &host).unwrap();
    registry.retire_after_native_quiescence(&owner).unwrap();
}

#[test]
fn quiescence_does_not_discard_unsettled_radio_cleanup() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let radio: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    let owner = registry
        .attach_or_register([95; 16], 1, radio.clone())
        .unwrap();
    registry.begin_retirement(&owner).unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&owner),
        Err(Error::OwnerBusy)
    );
    registry.confirm_cleanup(&owner, &radio).unwrap();
    registry.retire_after_native_quiescence(&owner).unwrap();
}

#[test]
fn unpublished_radio_can_settle_before_retirement() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let radio: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    let owner = registry
        .attach_or_register([96; 16], 1, radio.clone())
        .unwrap();
    registry.confirm_unpublished(&owner, &radio).unwrap();
    registry.begin_retirement(&owner).unwrap();
    registry.retire_after_native_quiescence(&owner).unwrap();
}

#[test]
fn host_construction_finishes_only_for_a_radio_attached_before_retirement() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let radio: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    let host: Arc<dyn std::any::Any + Send + Sync> = Arc::new(());
    let owner = registry
        .attach_or_register([98; 16], 1, radio.clone())
        .unwrap();
    registry.begin_retirement(&owner).unwrap();
    assert!(matches!(
        registry.attach_or_register([98; 16], 1, Arc::new(())),
        Err(Error::OwnerRetired)
    ));
    registry
        .add_host_anchor(&owner, &radio, host.clone())
        .unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&owner),
        Err(Error::OwnerBusy)
    );
    registry.confirm_cleanup(&owner, &radio).unwrap();
    registry.confirm_cleanup(&owner, &host).unwrap();
    registry.retire_after_native_quiescence(&owner).unwrap();
}

struct HostRadio(crate::OrderedAdmissionOwner);
impl crate::PlatformRadio for HostRadio {
    fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
        Some(self.0)
    }
    fn submit(&self, _: crate::RadioRequest) {
        panic!("slot tests do not dispatch native requests");
    }
    fn cancel(&self, _: u64) {}
}
struct HostWake;
impl crate::WakeSink for HostWake {
    fn wake(&self, _: u64) {}
}

async fn mobile_host(owner: crate::OrderedAdmissionOwner) -> crate::MobileHost {
    crate::MobileHost::open(
        Arc::new(HostRadio(owner)),
        Arc::new(HostWake),
        crate::HostOptions {
            platform: crate::MobilePlatform::Android,
            owner: "slot-test".into(),
            adapter_label: "test".into(),
        },
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap()
}

fn mobile_request(id: u64) -> OpenRequest<crate::OrderedOpenSpec> {
    OpenRequest {
        request_id: id,
        group_id: id + 1000,
        peer: format!("peer-{id}"),
        fingerprint: crate::OrderedOpenSpec {
            connection_generation: format!("connection-{id}"),
            database_generation: "database".into(),
            snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "graph".into(),
                attachment: 1,
                revision: 1,
            },
            members: vec![],
            item_capacity: 16,
            byte_capacity: 1024,
        },
        local_group: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mobile_hosts_share_exact_spec_for_v2_wire_identity() {
    let id =
        crate::OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456780003").unwrap();
    let a = mobile_host(id).await;
    let b = mobile_host(id).await;
    let coordinator = a.ordered_open_coordinator().unwrap().unwrap();
    let same = b.ordered_open_coordinator().unwrap().unwrap();
    assert!(Arc::ptr_eq(&coordinator, &same));
    let host_a = coordinator.register_host().unwrap();
    let host_b = same.register_host().unwrap();
    let mapping_a = coordinator.allocate_group(host_a, 7).unwrap();
    let mapping_b = same.allocate_group(host_b, 7).unwrap();
    assert_ne!(mapping_a.native_group_id, mapping_b.native_group_id);

    let mut request = mobile_request(42);
    request.group_id = mapping_a.native_group_id;
    request.local_group = Some(mapping_a);
    request.fingerprint.members.push(crate::OrderedMember {
        member_id: 9,
        instance: crate::Instance {
            peer_id: request.peer.clone(),
            service_uuid: "Service".into(),
            service_occurrence: 2,
            characteristic_uuid: "Characteristic".into(),
            characteristic_occurrence: 3,
        },
    });
    let original = request.clone();
    let lease = coordinator.acquire(request).unwrap();
    let mut altered = lease.clone();
    altered.request.fingerprint.members[0]
        .instance
        .characteristic_occurrence += 1;
    assert_eq!(
        same.begin_dispatch_v2(&altered, id),
        Err(Error::IdentityMismatch)
    );
    assert!(!coordinator.outstanding()[0].dispatched);
    let wire = same.begin_dispatch_v2(&lease, id).unwrap();
    assert_eq!(wire.spec, original.fingerprint);
    assert_eq!(wire.original_request_id, original.request_id);
    assert_eq!(wire.native_group_id, mapping_a.native_group_id);
    assert_eq!(wire.peer_id, original.peer);
    assert_eq!(wire.slot_identity().slot, lease.identity.slot);
    assert_eq!(
        wire.slot_identity().slot_generation,
        lease.identity.generation
    );

    coordinator
        .retain_terminal(
            &lease,
            lease.identity,
            crate::RadioCompletion::OrderedOpened {
                generation: crate::OrderedSourceGeneration::Counter(1),
                members: vec![],
            },
        )
        .unwrap();
    // Model the matching native retirement and subsequent group removal proofs.
    coordinator
        .confirm_retired(&lease, retirement_for_test(lease.identity))
        .unwrap();
    coordinator
        .forget_removed_group(mapping_a, mapping_a.native_group_id)
        .unwrap();
    same.forget_unsubmitted_group(mapping_b, mapping_b.native_group_id)
        .unwrap();
    a.begin_ordered_owner_retirement().unwrap();
    a.confirm_ordered_host_cleanup().unwrap();
    b.confirm_ordered_host_cleanup().unwrap();
    a.retire_ordered_owner_after_native_quiescence().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mobile_hosts_share_owner_capacity_and_preserve_debt_across_replacement() {
    let id =
        crate::OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456780001").unwrap();
    let replacement_id =
        crate::OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456780002").unwrap();
    let a = mobile_host(id).await;
    let b = mobile_host(id).await;
    let direct = crate::ForeignRadio::new(
        Arc::new(HostRadio(id)),
        crate::MobilePlatform::Android,
        "direct".into(),
    );
    let replacement = mobile_host(replacement_id).await;
    let owner = a.ordered_open_coordinator().unwrap().unwrap();
    let same = b.ordered_open_coordinator().unwrap().unwrap();
    let old_host = Arc::downgrade(&b.inner);
    let new = replacement.ordered_open_coordinator().unwrap().unwrap();
    assert!(Arc::ptr_eq(&owner, &same));
    assert!(Arc::ptr_eq(
        &owner,
        &direct.ordered_open_coordinator().unwrap().unwrap()
    ));
    assert!(!Arc::ptr_eq(&owner, &new));
    let barrier = Arc::new(tokio::sync::Barrier::new(261));
    let mut tasks = Vec::new();
    for i in 0..260 {
        let host = if i % 2 == 0 { a.clone() } else { b.clone() };
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            host.ordered_open_coordinator()
                .unwrap()
                .unwrap()
                .acquire(mobile_request(i))
        }));
    }
    barrier.wait().await;
    let mut leases = Vec::new();
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(lease) => leases.push(lease),
            Err(Error::Capacity) => refused += 1,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!((leases.len(), refused), (64, 196));
    let identities: std::collections::HashSet<_> = leases
        .iter()
        .map(|l| {
            (
                l.identity.incarnation,
                l.identity.slot,
                l.identity.generation,
            )
        })
        .collect();
    assert_eq!(identities.len(), 64);
    let cancelled = leases.pop().unwrap();
    owner.release_unsubmitted(&cancelled).unwrap();
    assert_eq!(
        owner.begin_dispatch(&cancelled),
        Err(Error::IdentityMismatch)
    );
    let recovered = owner.acquire(mobile_request(999)).unwrap();
    assert_eq!(recovered.identity, cancelled.identity);
    owner.begin_dispatch(&recovered).unwrap();
    owner
        .retain_terminal(
            &recovered,
            recovered.identity,
            crate::RadioCompletion::OrderedOpenFailed {
                failure: crate::PlatformFailure::new(
                    crate::radio::FailureKind::Platform,
                    "refused",
                ),
                ownership: crate::OrderedOpenOwnership::NotCreated,
            },
        )
        .unwrap();
    assert_eq!(
        owner.outstanding().len(),
        64,
        "lost acknowledgement must retain credit"
    );
    drop(b);
    assert!(
        old_host.upgrade().is_some(),
        "replacement must retain the old host cleanup owner"
    );
    a.begin_ordered_owner_retirement().unwrap();
    assert!(matches!(
        owner.acquire(mobile_request(1000)),
        Err(Error::OwnerRetired)
    ));
    assert!(
        crate::MobileHost::open(
            Arc::new(HostRadio(id)),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "retiring".into(),
                adapter_label: "test".into()
            },
            tokio::runtime::Handle::current()
        )
        .await
        .is_err()
    );
    assert_eq!(
        a.retire_ordered_owner_after_native_quiescence(),
        Err(Error::OwnerBusy)
    );
    let replacement_lease = new.acquire(mobile_request(1001)).unwrap();
    new.release_unsubmitted(&replacement_lease).unwrap();
    for lease in leases {
        owner.release_unsubmitted(&lease).unwrap();
    }
    owner
        .confirm_retired(&recovered, retirement_for_test(recovered.identity))
        .unwrap();
    assert_eq!(owner.outstanding().len(), 0);
    assert_eq!(
        a.retire_ordered_owner_after_native_quiescence(),
        Err(Error::OwnerBusy),
        "each old host cleanup must settle too"
    );
    // Model explicit, per-host cleanup proof. Slot retirement alone does not
    // establish it, and the production native proof is not yet wired.
    a.confirm_ordered_host_cleanup().unwrap();
    let retained_b = crate::MobileHost {
        inner: old_host.upgrade().unwrap(),
    };
    retained_b.confirm_ordered_host_cleanup().unwrap();
    assert_eq!(
        a.retire_ordered_owner_after_native_quiescence(),
        Err(Error::OwnerBusy)
    );
    direct.confirm_ordered_radio_cleanup().unwrap();
    // This call models an exact positive native quiescence response.
    a.retire_ordered_owner_after_native_quiescence().unwrap();
    assert!(matches!(
        same.acquire(mobile_request(1002)),
        Err(Error::OwnerRetired)
    ));
    replacement.begin_ordered_owner_retirement().unwrap();
    replacement.confirm_ordered_host_cleanup().unwrap();
    replacement
        .retire_ordered_owner_after_native_quiescence()
        .unwrap();
}

#[test]
fn concurrent_callers_share_one_bound_before_dispatch() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([1; 16], 4).unwrap();
    assert_eq!(
        registry.register([1; 16], 4).err(),
        Some(Error::AlreadyRegistered)
    );
    let gate = Arc::new(Barrier::new(25));
    let handles: Vec<_> = (0..24)
        .map(|id| {
            let client = registry.attach([1; 16]).unwrap();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                client.acquire(request(id))
            })
        })
        .collect();
    gate.wait();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 4);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(Error::Capacity)))
            .count(),
        20
    );
    assert_eq!(owner.outstanding().len(), 4);
}

#[test]
fn terminal_debt_survives_lost_ack_and_reuses_slot_many_times() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([2; 16], 1).unwrap();
    for id in 0..300 {
        let lease = owner.acquire(request(id)).unwrap();
        owner.begin_dispatch(&lease).unwrap();
        assert_eq!(
            owner.confirm_retired(&lease, retirement_for_test(lease.identity)),
            Err(Error::NotTerminal)
        );
        owner
            .retain_terminal(&lease, lease.identity, format!("outcome-{id}"))
            .unwrap();
        assert_eq!(owner.acquire(request(999)), Err(Error::Capacity));
        assert_eq!(
            owner.outstanding()[0].disposition.as_deref(),
            Some(format!("outcome-{id}").as_str())
        );
        owner
            .confirm_retired(&lease, retirement_for_test(lease.identity))
            .unwrap();
        assert_eq!(lease.identity.generation, id + 1);
    }
}

#[test]
fn local_cancellation_is_fenced_from_later_reservation() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([3; 16], 1).unwrap();
    let old = owner.acquire(request(1)).unwrap();
    owner.release_unsubmitted(&old).unwrap();
    let next = owner.acquire(request(1)).unwrap();
    assert_eq!(old.identity, next.identity);
    assert_eq!(owner.begin_dispatch(&old), Err(Error::IdentityMismatch));
    owner.begin_dispatch(&next).unwrap();
    assert_eq!(
        owner.release_unsubmitted(&next),
        Err(Error::AlreadyDispatched)
    );
}

#[test]
fn old_and_conflicting_messages_cannot_mutate_reused_slot() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([4; 16], 1).unwrap();
    let old = owner.acquire(request(1)).unwrap();
    owner.begin_dispatch(&old).unwrap();
    owner
        .retain_terminal(&old, old.identity, "first".into())
        .unwrap();
    owner
        .confirm_retired(&old, retirement_for_test(old.identity))
        .unwrap();
    let current = owner.acquire(request(2)).unwrap();
    owner.begin_dispatch(&current).unwrap();
    let mut wrong = current.clone();
    wrong.request.peer.push('x');
    assert_eq!(
        owner.retain_terminal(&wrong, wrong.identity, "wrong peer".into()),
        Err(Error::IdentityMismatch)
    );
    wrong = current.clone();
    wrong.request.group_id += 1;
    assert_eq!(
        owner.retain_terminal(&wrong, wrong.identity, "wrong group".into()),
        Err(Error::IdentityMismatch)
    );
    wrong = current.clone();
    wrong.request.request_id += 1;
    assert_eq!(
        owner.retain_terminal(&wrong, wrong.identity, "wrong request".into()),
        Err(Error::IdentityMismatch)
    );
    wrong = current.clone();
    wrong.request.fingerprint.push(0);
    assert_eq!(
        owner.retain_terminal(&wrong, wrong.identity, "wrong fingerprint".into()),
        Err(Error::IdentityMismatch)
    );
    wrong = current.clone();
    wrong.identity.generation += 1;
    assert_eq!(
        owner.retain_terminal(&wrong, wrong.identity, "future".into()),
        Err(Error::IdentityMismatch)
    );
    wrong = current.clone();
    wrong.identity.incarnation = [9; 16];
    assert_eq!(
        owner.retain_terminal(&wrong, wrong.identity, "host".into()),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(
        owner.retain_terminal(&old, old.identity, "old completion or fence".into()),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(
        owner.confirm_retired(&old, retirement_for_test(old.identity)),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(owner.outstanding()[0].disposition, None);
}

#[test]
fn host_replacement_keeps_old_debt_and_namespaces_independent() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let old = registry.register([5; 16], 1).unwrap();
    let lease = old.acquire(request(1)).unwrap();
    old.begin_dispatch(&lease).unwrap();
    drop(old);
    let replacement = registry.register([6; 16], 1).unwrap();
    let new = replacement.acquire(request(2)).unwrap();
    assert_eq!(
        replacement.retain_terminal(&lease, lease.identity, "old".into()),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(registry.attach([5; 16]).unwrap().outstanding().len(), 1);
    assert_eq!(replacement.outstanding()[0].lease, new);
    assert_eq!(
        registry.attach([7; 16]).err(),
        Some(Error::UnknownIncarnation)
    );
}

#[test]
fn quiescent_owner_retirement_fences_stale_clients_and_preserves_other_owners() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let old = registry.register([21; 16], 1).unwrap();
    let client = registry.attach([21; 16]).unwrap();
    let other = registry.register([22; 16], 1).unwrap();
    let lease = client.acquire(request(1)).unwrap();
    client.begin_dispatch(&lease).unwrap();
    registry.begin_retirement(&old).unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&old),
        Err(Error::OwnerBusy)
    );
    client
        .retain_terminal(&lease, lease.identity, "created".into())
        .unwrap();
    assert_eq!(
        registry.retire_after_native_quiescence(&old),
        Err(Error::OwnerBusy)
    );
    client
        .confirm_retired(&lease, retirement_for_test(lease.identity))
        .unwrap();
    registry.retire_after_native_quiescence(&old).unwrap();
    assert_eq!(
        registry.attach([21; 16]).err(),
        Some(Error::UnknownIncarnation)
    );
    assert_eq!(client.acquire(request(2)), Err(Error::OwnerRetired));
    assert!(other.acquire(request(3)).is_ok());
}

#[test]
fn retirement_and_acquisition_have_one_winner() {
    for id in 0..100 {
        let registry = Arc::new(CoordinatorRegistry::<Vec<u8>, String>::default());
        let owner = registry.register([23; 16], 1).unwrap();
        let gate = Arc::new(Barrier::new(3));
        let client = owner.clone();
        let acquire_gate = gate.clone();
        let acquire = std::thread::spawn(move || {
            acquire_gate.wait();
            client.acquire(request(id))
        });
        let retiring_registry = registry.clone();
        let retiring_owner = owner.clone();
        let retire_gate = gate.clone();
        let retire = std::thread::spawn(move || {
            retire_gate.wait();
            retiring_registry.begin_retirement(&retiring_owner).unwrap();
            retiring_registry.retire_after_native_quiescence(&retiring_owner)
        });
        gate.wait();
        match (acquire.join().unwrap(), retire.join().unwrap()) {
            (Ok(lease), Err(Error::OwnerBusy)) => {
                owner.release_unsubmitted(&lease).unwrap();
                registry.retire_after_native_quiescence(&owner).unwrap();
            }
            (Err(Error::OwnerRetired), Ok(())) => {}
            other => panic!("unexpected race result: {other:?}"),
        }
        assert_eq!(owner.acquire(request(200)), Err(Error::OwnerRetired));
    }
}

#[test]
fn observed_old_or_future_receipts_cannot_affect_current_lease() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([8; 16], 1).unwrap();
    let old = owner.acquire(request(1)).unwrap();
    owner.begin_dispatch(&old).unwrap();
    owner
        .retain_terminal(&old, old.identity, "old".into())
        .unwrap();
    owner
        .confirm_retired(&old, retirement_for_test(old.identity))
        .unwrap();
    let current = owner.acquire(request(2)).unwrap();
    owner.begin_dispatch(&current).unwrap();
    assert_eq!(
        owner.retain_terminal(&current, old.identity, "late fence or completion".into()),
        Err(Error::IdentityMismatch)
    );
    let mut future = current.identity;
    future.generation += 1;
    assert_eq!(
        owner.retain_terminal(&current, future, "future".into()),
        Err(Error::IdentityMismatch)
    );
    owner
        .retain_terminal(&current, current.identity, "current".into())
        .unwrap();
    assert_eq!(
        owner.confirm_retired(&current, retirement_for_test(old.identity)),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(
        owner.confirm_retired(&current, retirement_for_test(future)),
        Err(Error::IdentityMismatch)
    );
    assert_eq!(
        owner.outstanding()[0].disposition.as_deref(),
        Some("current")
    );
    assert_eq!(
        owner.retain_terminal(&current, current.identity, "conflict".into()),
        Err(Error::ConflictingDisposition)
    );
    owner
        .confirm_retired(&current, retirement_for_test(current.identity))
        .unwrap();
}

#[test]
fn generation_and_local_nonce_exhaustion_fail_without_wrap() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([10; 16], 2).unwrap();
    {
        let mut state = owner.state.lock().unwrap();
        state.slots[0].generation = u64::MAX;
        state.slots[1].generation = u64::MAX - 1;
    }
    let last = owner.acquire(request(1)).unwrap();
    assert_eq!(last.identity.generation, u64::MAX);
    owner.begin_dispatch(&last).unwrap();
    owner
        .retain_terminal(&last, last.identity, "last".into())
        .unwrap();
    owner
        .confirm_retired(&last, retirement_for_test(last.identity))
        .unwrap();
    assert_eq!(owner.acquire(request(2)), Err(Error::Exhausted));
    let other = registry.register([11; 16], 1).unwrap();
    other.state.lock().unwrap().next_nonce = u64::MAX;
    assert_eq!(other.acquire(request(3)), Err(Error::Exhausted));
    assert!(other.outstanding().is_empty());
}

#[test]
fn cancellation_and_dispatch_race_has_one_winner() {
    for id in 0..100 {
        let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
        let owner = registry.register([12; 16], 1).unwrap();
        let lease = owner.acquire(request(id)).unwrap();
        let gate = Arc::new(Barrier::new(3));
        let sender = owner.clone();
        let sender_lease = lease.clone();
        let sender_gate = gate.clone();
        let send = std::thread::spawn(move || {
            sender_gate.wait();
            sender.begin_dispatch(&sender_lease)
        });
        let cancel_owner = owner.clone();
        let cancel_lease = lease.clone();
        let cancel_gate = gate.clone();
        let cancel = std::thread::spawn(move || {
            cancel_gate.wait();
            cancel_owner.release_unsubmitted(&cancel_lease)
        });
        gate.wait();
        match (send.join().unwrap(), cancel.join().unwrap()) {
            (Ok(identity), Err(Error::AlreadyDispatched)) => {
                assert_eq!(identity, lease.identity);
                assert!(owner.outstanding()[0].dispatched);
            }
            (Err(Error::IdentityMismatch), Ok(())) => assert!(owner.outstanding().is_empty()),
            other => panic!("unexpected race result: {other:?}"),
        }
    }
}

#[test]
fn occupied_slot_is_capacity_even_if_another_slot_is_exhausted() {
    let registry = CoordinatorRegistry::<Vec<u8>, String>::default();
    let owner = registry.register([13; 16], 2).unwrap();
    owner.state.lock().unwrap().slots[0].generation = u64::MAX;
    let current = owner.acquire(request(1)).unwrap();
    assert_eq!(owner.acquire(request(2)), Err(Error::Capacity));
    owner.release_unsubmitted(&current).unwrap();
    assert!(owner.acquire(request(3)).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_hosts_reserve_colliding_local_groups_without_releasing_live_debt() {
    let id =
        crate::OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456780004").unwrap();
    let a = mobile_host(id).await;
    let b = mobile_host(id).await;
    let coordinator = a.ordered_open_coordinator().unwrap().unwrap();
    let first = a.inner.reserve_ordered_group(7).unwrap().unwrap();
    let second = b.inner.reserve_ordered_group(7).unwrap().unwrap();
    assert_ne!(first.native_group_id, second.native_group_id);
    assert_eq!(a.inner.ordered_group(7), Some(first));
    assert_eq!(b.inner.ordered_group(7), Some(second));
    assert_eq!(
        a.inner.reserve_ordered_group(7),
        Err(Error::GroupAlreadyMapped)
    );
    assert_eq!(
        a.inner.forget_unsubmitted_ordered_group(second),
        Err(Error::IdentityMismatch)
    );

    let mut request = mobile_request(42);
    request.group_id = first.native_group_id;
    request.local_group = Some(first);
    let lease = coordinator.acquire(request).unwrap();
    assert_eq!(
        a.inner.forget_unsubmitted_ordered_group(first),
        Err(Error::OwnerBusy)
    );
    assert_eq!(a.inner.ordered_group(7), Some(first));
    coordinator.release_unsubmitted(&lease).unwrap();
    a.inner.forget_unsubmitted_ordered_group(first).unwrap();
    assert_eq!(a.inner.ordered_group(7), None);
    assert_eq!(b.inner.ordered_group(7), Some(second));
    b.inner.forget_unsubmitted_ordered_group(second).unwrap();
    a.begin_ordered_owner_retirement().unwrap();
    a.confirm_ordered_host_cleanup().unwrap();
    b.confirm_ordered_host_cleanup().unwrap();
    a.retire_ordered_owner_after_native_quiescence().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mapped_source_attachment_is_host_scoped_and_preserves_other_owner() {
    use ubm_desktop::boundary::{OrderedSourceIngress, OrderedSourceLease};
    let id =
        crate::OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456780005").unwrap();
    let a = mobile_host(id).await;
    let b = mobile_host(id).await;
    let first = a.inner.reserve_ordered_group(42).unwrap().unwrap();
    let second = b.inner.reserve_ordered_group(42).unwrap().unwrap();
    let lease = |mapping: GroupMapping| OrderedSourceLease {
        peer_id: "peer".into(),
        connection_generation: "connection".into(),
        collection_id: mapping.collection_id,
        native_group: mapping.native_group_id,
        source_generation: crate::OrderedSourceGeneration::Counter(1),
        members: vec![],
        ingress: OrderedSourceIngress::CentralRetainedBatch,
    };
    let lease_a = lease(first);
    let lease_b = lease(second);
    a.inner.ordered_wakes.begin(first.native_group_id).unwrap();
    b.inner.ordered_wakes.begin(second.native_group_id).unwrap();
    assert!(a.inner.attach_ordered_source(&lease_b).is_err());
    a.inner.attach_ordered_source(&lease_a).unwrap();
    b.inner.attach_ordered_source(&lease_b).unwrap();
    assert!(a.inner.ordered_consumer_for_lease(&lease_a).is_some());
    assert!(b.inner.ordered_consumer_for_lease(&lease_b).is_some());
    a.inner.retire_ordered_source(&lease_a);
    assert!(a.inner.ordered_consumer_for_lease(&lease_a).is_none());
    assert!(b.inner.ordered_consumer_for_lease(&lease_b).is_some());
    assert_eq!(a.inner.ordered_group(42), Some(first));
    b.inner.retire_ordered_source(&lease_b);
    a.inner.forget_unsubmitted_ordered_group(first).unwrap();
    b.inner.forget_unsubmitted_ordered_group(second).unwrap();
    a.begin_ordered_owner_retirement().unwrap();
    a.confirm_ordered_host_cleanup().unwrap();
    b.confirm_ordered_host_cleanup().unwrap();
    a.retire_ordered_owner_after_native_quiescence().unwrap();
}

// Models the native slot's retained terminal and idempotent retirement through
// the actual MobileHost completion boundary, while withholding transport replies.
struct LostV2RepliesRadio {
    owner: crate::OrderedAdmissionOwner,
    host: std::sync::OnceLock<std::sync::Weak<crate::MobileHost>>,
    requests: std::sync::Mutex<Vec<crate::RadioRequest>>,
    slot: std::sync::Mutex<Option<crate::OrderedOpenIdentity>>,
    retired: std::sync::atomic::AtomicBool,
}

impl crate::PlatformRadio for LostV2RepliesRadio {
    fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
        Some(crate::OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn submit(&self, request: crate::RadioRequest) {
        use crate::{
            OrderedOpenCompletionV2 as Completion, OrderedOpenRequestV2 as Request, RadioCompletion,
        };
        let mut requests = self.requests.lock().unwrap();
        requests.push(request.clone());
        let response = match &request {
            crate::RadioRequest::OrderedOpenV2(Request::Open { identity, .. }) => {
                *self.slot.lock().unwrap() = Some(identity.clone());
                return;
            }
            crate::RadioRequest::OrderedOpenV2(Request::Recover { identity, .. }) => {
                assert_eq!(self.slot.lock().unwrap().as_ref(), Some(identity));
                let count = requests
                    .iter()
                    .filter(|r| {
                        matches!(
                            r,
                            crate::RadioRequest::OrderedOpenV2(Request::Recover { .. })
                        )
                    })
                    .count();
                if count <= 70 {
                    return;
                }
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: crate::OrderedOpenOutcomeV2::Failed {
                        original_failure: crate::PlatformFailure::new(
                            crate::FailureKind::Platform,
                            "fenced",
                        ),
                        ownership: crate::OrderedOpenOwnershipV2::NotCreated,
                    },
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { identity, .. }) => {
                assert_eq!(self.slot.lock().unwrap().as_ref(), Some(identity));
                self.retired
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let count = requests
                    .iter()
                    .filter(|r| {
                        matches!(
                            r,
                            crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { .. })
                        )
                    })
                    .count();
                if count <= 70 {
                    return;
                }
                RadioCompletion::OrderedOpenV2(Completion::Retired(crate::OrderedOpenRetired {
                    slot_identity: identity.slot_identity(),
                }))
            }
            crate::RadioRequest::Discover { .. } => RadioCompletion::DiscoveredScoped {
                services: crate::test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "lost-session-graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            },
            _ => match crate::test_common::polar_responder(&request) {
                crate::test_common::Reply::Now(answer) => answer,
                crate::test_common::Reply::Hold => return,
            },
        };
        drop(requests);
        self.host
            .get()
            .unwrap()
            .upgrade()
            .unwrap()
            .complete(request.id(), response);
    }
    fn cancel(&self, _: u64) {}
}

#[tokio::test(start_paused = true)]
async fn v2_lost_recovery_and_retirement_replies_remain_bounded_and_retryable() {
    use crate::{
        OrderedOpenCompletionV2 as Completion, OrderedOpenRequestV2 as Request, RadioCompletion,
    };
    use ubm_desktop::RadioBoundary;
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009113").unwrap();
    let radio = Arc::new(LostV2RepliesRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        slot: std::sync::Mutex::new(None),
        retired: std::sync::atomic::AtomicBool::new(false),
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "lost-replies-v2".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    host.inner.stop_ordered_recovery_for_test();
    let opening_host = host.clone();
    let opening = tokio::spawn(async move {
        opening_host
            .inner
            .radio
            .prepare_ordered_source(v2_source_request())
            .await
    });
    while radio.requests.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    opening.abort();
    assert!(opening.await.unwrap_err().is_cancelled());

    let recovering_host = host.clone();
    let recovering = tokio::spawn(async move {
        recovering_host
            .inner
            .radio
            .recover_ordered_source("peer", 42)
            .await
    });
    while radio.requests.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let current = radio.requests.lock().unwrap().last().unwrap().clone();
    let crate::RadioRequest::OrderedOpenV2(Request::Recover { id, identity }) = current else {
        unreachable!()
    };
    let concurrent = host
        .inner
        .radio
        .call(|id| {
            crate::RadioRequest::OrderedOpenV2(Request::Recover {
                id,
                identity: identity.clone(),
            })
        })
        .await;
    assert!(concurrent.is_err());
    assert_eq!(host.inner.radio.counters().pending_requests, 1);
    assert_eq!(radio.requests.lock().unwrap().len(), 2);
    let mut wrong_slot = identity.slot_identity();
    wrong_slot.slot_generation += 1;
    assert_eq!(
        host.complete(
            id,
            RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                slot_identity: wrong_slot,
                outcome: crate::OrderedOpenOutcomeV2::Pending,
            }))
        ),
        crate::CompletionStatus::Mismatched
    );
    recovering.abort();
    assert!(recovering.await.unwrap_err().is_cancelled());

    // Cancellation and each timeout drop a waiter while native retains ownership.
    for _ in 0..69 {
        assert!(
            host.inner
                .radio
                .recover_ordered_source("peer", 42)
                .await
                .is_err()
        );
        assert!(host.inner.radio.retained_v2_attempt_count() <= 2);
        assert_eq!(host.inner.radio.counters().pending_requests, 0);
    }
    let first_recovery = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| {
            matches!(
                r,
                crate::RadioRequest::OrderedOpenV2(Request::Recover { .. })
            )
        })
        .unwrap()
        .clone();
    let crate::RadioRequest::OrderedOpenV2(Request::Recover {
        id: old_id,
        identity,
    }) = first_recovery
    else {
        unreachable!()
    };
    let old_answer =
        RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
            slot_identity: identity.slot_identity(),
            outcome: crate::OrderedOpenOutcomeV2::Failed {
                original_failure: crate::PlatformFailure::new(
                    crate::FailureKind::Platform,
                    "fenced",
                ),
                ownership: crate::OrderedOpenOwnershipV2::NotCreated,
            },
        }));
    assert_eq!(
        host.complete(old_id, old_answer),
        crate::CompletionStatus::Late
    );
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .len(),
        1
    );
    let interrupted: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
    assert_eq!(interrupted["state"], "release-failed");
    assert!(!host.inner.ordered_wakes.is_closed());
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .len(),
        1
    );

    // The 71st fence returns the retained terminal, then 70 retirement
    // responses are lost after native has already retired the slot.
    for _ in 0..70 {
        assert!(
            host.inner
                .radio
                .recover_ordered_source("peer", 42)
                .await
                .is_err()
        );
        assert!(host.inner.radio.retained_v2_attempt_count() <= 3);
        assert_eq!(host.inner.radio.counters().pending_requests, 0);
        assert_eq!(
            host.ordered_open_coordinator()
                .unwrap()
                .unwrap()
                .outstanding()
                .len(),
            1
        );
    }
    assert!(radio.retired.load(std::sync::atomic::Ordering::SeqCst));
    let first_ack = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| {
            matches!(
                r,
                crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { .. })
            )
        })
        .unwrap()
        .clone();
    let crate::RadioRequest::OrderedOpenV2(Request::Acknowledge {
        id: old_ack,
        identity,
    }) = first_ack
    else {
        unreachable!()
    };
    assert_eq!(
        host.complete(
            old_ack,
            RadioCompletion::OrderedOpenV2(Completion::Retired(crate::OrderedOpenRetired {
                slot_identity: identity.slot_identity()
            }))
        ),
        crate::CompletionStatus::Late
    );
    assert_eq!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .len(),
        1
    );
    assert!(matches!(
        host.inner
            .radio
            .recover_ordered_source("peer", 42)
            .await
            .unwrap(),
        ubm_desktop::boundary::OrderedSourceRecovery::NotStarted
    ));
    assert!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
    );
    assert_eq!(host.inner.radio.retained_v2_attempt_count(), 0);
    let ids: Vec<_> = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(crate::RadioRequest::id)
        .collect();
    assert_eq!(
        ids.iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        ids.len()
    );
    let report: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
    assert_eq!(
        report["state"], "release-failed",
        "native quiescence is still unproved"
    );
}

#[tokio::test(start_paused = true)]
async fn public_cancel_and_dispose_recover_after_many_lost_replies() {
    use crate::test_common::{self as common, call, ok};
    let owner =
        crate::OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000009114").unwrap();
    let radio = Arc::new(LostV2RepliesRadio {
        owner,
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        slot: std::sync::Mutex::new(None),
        retired: std::sync::atomic::AtomicBool::new(false),
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "public-lost-replies".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let session = host.open_session("public-lost-replies").unwrap();
    ok(&call(
        &session,
        "scan.start",
        r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
    )
    .await);
    host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
        peer_id: common::POLAR.into(),
        service_uuids: vec!["180D".into()],
        ..crate::Advertisement::default()
    }));
    common::drain_until(&session, |records| {
        !common::of_type(records, "adv").is_empty()
    })
    .await;
    let connection = ok(&call(
        &session,
        "connection.connect",
        &serde_json::json!({"peerId":common::POLAR,"lease":"lease-1","operationId":"connect"})
            .to_string(),
    )
    .await);
    let database = ok(&call(
        &session,
        "gatt.discover",
        &serde_json::json!({"peerId":common::POLAR,"lease":"lease-1","operationId":"discover"})
            .to_string(),
    )
    .await);
    let reserved = ok(&call(
        &session,
        "gatt.ordered.reserve",
        &serde_json::json!({"peerId":common::POLAR}).to_string(),
    )
    .await);
    let args = serde_json::json!({"wireVersion":2,"peerId":common::POLAR,"collectionId":reserved["collectionId"],"connectionGeneration":connection["connectionGeneration"],"databaseGeneration":database["databaseGeneration"],"publicConnectionGeneration":"c","publicDatabaseGeneration":"d","members":[{"memberId":"0","publicMemberId":"a","selector":common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":128,"maxControls":8,"operationId":"ordered-open"}).to_string();
    let opening_session = session.clone();
    let opening =
        tokio::spawn(async move { call(&opening_session, "gatt.ordered.open", &args).await });
    while !radio.requests.lock().unwrap().iter().any(|r| {
        matches!(
            r,
            crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { .. })
        )
    }) {
        assert!(
            !opening.is_finished(),
            "public open failed before native admission"
        );
        tokio::task::yield_now().await;
    }
    ok(&call(&session, "op.cancel", r#"{"operationId":"ordered-open"}"#).await);
    opening.await.unwrap();
    let disposed = ok(&call(&session, "session.dispose", "{}").await);
    assert!(matches!(
        disposed["state"].as_str(),
        Some("released" | "release-failed")
    ));
    assert!(
        !host
            .ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
    );
    for _ in 0..500 {
        if host
            .ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty()
        {
            break;
        }
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(host.inner.radio.retained_v2_attempt_count() <= 3);
    }
    assert!(
        host.ordered_open_coordinator()
            .unwrap()
            .unwrap()
            .outstanding()
            .is_empty(),
        "background recovery did not retire the public session's open"
    );
    let collection_id: u64 = reserved["collectionId"].as_str().unwrap().parse().unwrap();
    assert!(
        host.inner
            .central
            .ordered_batch_cleanup_complete(common::POLAR, collection_id)
            .await
            .unwrap(),
        "native retirement must also release the central claims and admission"
    );
    let (fences, acks) = {
        let requests = radio.requests.lock().unwrap();
        let fences = requests
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Recover { .. })
                )
            })
            .count();
        let acks = requests
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    crate::RadioRequest::OrderedOpenV2(
                        crate::OrderedOpenRequestV2::Acknowledge { .. }
                    )
                )
            })
            .count();
        (fences, acks)
    };
    assert!(fences > 70 && acks > 70, "fences={fences}, acks={acks}");
    let report: serde_json::Value = serde_json::from_str(&host.shutdown().await).unwrap();
    assert_eq!(report["state"], "release-failed");
}

// Two abandoned public opens share one recovery pump. One native source never
// answers its barrier; the other initially loses recovery replies.
struct IndependentRecoveryRadio {
    owner: crate::OrderedAdmissionOwner,
    host: std::sync::OnceLock<std::sync::Weak<crate::MobileHost>>,
    requests: std::sync::Mutex<Vec<crate::RadioRequest>>,
    second_ready: std::sync::atomic::AtomicBool,
    second_created: bool,
}

impl crate::PlatformRadio for IndependentRecoveryRadio {
    fn ordered_admission_owner(&self) -> Option<crate::OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<crate::OrderedAdmissionNegotiation> {
        Some(crate::OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn submit(&self, request: crate::RadioRequest) {
        use crate::{
            OrderedOpenCompletionV2 as Completion, OrderedOpenRequestV2 as Request, RadioCompletion,
        };
        self.requests.lock().unwrap().push(request.clone());
        let answer = match &request {
            crate::RadioRequest::OrderedOpenV2(Request::Open { .. }) => return,
            crate::RadioRequest::OrderedOpenV2(Request::Recover { identity, .. })
                if identity.peer_id == "stalled-peer" =>
            {
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: crate::OrderedOpenOutcomeV2::Opened {
                        source_generation: crate::OrderedSourceGeneration::Counter(9),
                        members: identity
                            .spec
                            .members
                            .iter()
                            .map(|member| crate::OrderedRegistration {
                                member_id: member.member_id,
                                source_member: member.member_id + 17,
                                epoch: 3,
                            })
                            .collect(),
                    },
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Recover { identity, .. }) => {
                if !self.second_ready.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                RadioCompletion::OrderedOpenV2(Completion::Result(crate::OrderedOpenResultV2 {
                    slot_identity: identity.slot_identity(),
                    outcome: if self.second_created {
                        crate::OrderedOpenOutcomeV2::Opened {
                            source_generation: crate::OrderedSourceGeneration::Counter(10),
                            members: identity
                                .spec
                                .members
                                .iter()
                                .map(|member| crate::OrderedRegistration {
                                    member_id: member.member_id,
                                    source_member: member.member_id + 17,
                                    epoch: 3,
                                })
                                .collect(),
                        }
                    } else {
                        crate::OrderedOpenOutcomeV2::Failed {
                            original_failure: crate::PlatformFailure::new(
                                crate::FailureKind::Platform,
                                "fenced",
                            ),
                            ownership: crate::OrderedOpenOwnershipV2::NotCreated,
                        }
                    },
                }))
            }
            crate::RadioRequest::OrderedOpenV2(Request::Acknowledge { identity, .. }) => {
                RadioCompletion::OrderedOpenV2(Completion::Retired(crate::OrderedOpenRetired {
                    slot_identity: identity.slot_identity(),
                }))
            }
            crate::RadioRequest::OrderedBarrier { .. } => return,
            crate::RadioRequest::Discover { .. } => RadioCompletion::DiscoveredScoped {
                services: crate::test_common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "independent-graph".into(),
                    attachment: 1,
                    revision: 1,
                },
            },
            _ => match crate::test_common::polar_responder(&request) {
                crate::test_common::Reply::Now(answer) => answer,
                crate::test_common::Reply::Hold => return,
            },
        };
        self.host
            .get()
            .unwrap()
            .upgrade()
            .unwrap()
            .complete(request.id(), answer);
    }
    fn cancel(&self, _: u64) {}
}

async fn independent_recovery_with_stalled_fence(second_created: bool) {
    use crate::test_common::{self as common, call, ok};
    let radio = Arc::new(IndependentRecoveryRadio {
        owner: crate::OrderedAdmissionOwner::negotiate_v2(if second_created {
            "12345678-1234-4abc-8def-000000009116"
        } else {
            "12345678-1234-4abc-8def-000000009115"
        })
        .unwrap(),
        host: std::sync::OnceLock::new(),
        requests: std::sync::Mutex::new(Vec::new()),
        second_ready: std::sync::atomic::AtomicBool::new(false),
        second_created,
    });
    let host = Arc::new(
        crate::MobileHost::open(
            radio.clone(),
            Arc::new(HostWake),
            crate::HostOptions {
                platform: crate::MobilePlatform::Android,
                owner: "independent-recovery".into(),
                adapter_label: "test".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let session = host.open_session("independent-recovery").unwrap();
    ok(&call(
        &session,
        "scan.start",
        r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
    )
    .await);
    for peer in ["stalled-peer", "other-peer"] {
        host.ingest(crate::RadioIngress::Advertisement(crate::Advertisement {
            peer_id: peer.into(),
            service_uuids: vec!["180D".into()],
            ..crate::Advertisement::default()
        }));
    }
    common::drain_until(&session, |records| {
        common::of_type(records, "adv").len() >= 2
    })
    .await;
    let mut groups = Vec::new();
    for peer in ["stalled-peer", "other-peer"] {
        let connection = ok(&call(&session, "connection.connect", &serde_json::json!({"peerId":peer,"lease":peer,"operationId":format!("connect-{peer}")}).to_string()).await);
        let database = ok(&call(&session, "gatt.discover", &serde_json::json!({"peerId":peer,"lease":peer,"operationId":format!("discover-{peer}")}).to_string()).await);
        let reserved = ok(&call(
            &session,
            "gatt.ordered.reserve",
            &serde_json::json!({"peerId":peer}).to_string(),
        )
        .await);
        let group: u64 = reserved["collectionId"].as_str().unwrap().parse().unwrap();
        groups.push(group);
        let operation = format!("open-{peer}");
        let args = serde_json::json!({"wireVersion":2,"peerId":peer,"collectionId":reserved["collectionId"],"connectionGeneration":connection["connectionGeneration"],"databaseGeneration":database["databaseGeneration"],"publicConnectionGeneration":"c","publicDatabaseGeneration":"d","members":[{"memberId":"0","publicMemberId":"a","selector":common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":128,"maxControls":8,"operationId":operation}).to_string();
        let open_session = session.clone();
        let opening =
            tokio::spawn(async move { call(&open_session, "gatt.ordered.open", &args).await });
        while !radio.requests.lock().unwrap().iter().any(|request| matches!(request, crate::RadioRequest::OrderedOpenV2(crate::OrderedOpenRequestV2::Open { identity, .. }) if identity.peer_id == peer)) {
            assert!(!opening.is_finished(), "open failed before admission");
            tokio::task::yield_now().await;
        }
        ok(&call(
            &session,
            "op.cancel",
            &serde_json::json!({"operationId":operation}).to_string(),
        )
        .await);
        opening.await.unwrap();
    }
    for _ in 0..50 {
        if radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r, crate::RadioRequest::OrderedBarrier { .. }))
        {
            break;
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert!(
        radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r, crate::RadioRequest::OrderedBarrier { .. })),
        "created source did not reach its held fence"
    );
    radio
        .second_ready
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..100 {
        let barriers: std::collections::HashSet<_> = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                crate::RadioRequest::OrderedBarrier { group_id, .. } => Some(*group_id),
                _ => None,
            })
            .collect();
        if (second_created && barriers.len() >= 2)
            || (!second_created && host.inner.radio.retained_ordered_open(groups[1]).is_none())
        {
            break;
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
    }
    assert!(
        host.inner.radio.retained_ordered_open(groups[0]).is_some(),
        "stalled source ownership must remain retained"
    );
    if second_created {
        let barriers: std::collections::HashSet<_> = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                crate::RadioRequest::OrderedBarrier { group_id, .. } => Some(*group_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            barriers.len(),
            2,
            "another created source must reach its own fence"
        );
    } else {
        assert!(
            host.inner.radio.retained_ordered_open(groups[1]).is_none(),
            "another abandoned open must retire despite the stalled fence"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                host.inner
                    .central
                    .ordered_batch_cleanup_complete("other-peer", groups[1])
            )
            .await
            .unwrap()
            .unwrap()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_created_source_does_not_stop_another_abandoned_open() {
    independent_recovery_with_stalled_fence(false).await;
}

#[tokio::test(start_paused = true)]
async fn stalled_created_source_does_not_block_another_source_fence() {
    independent_recovery_with_stalled_fence(true).await;
}
