//! Native-owner retirement contract at the real MobileHost/session boundary.
//!
//! These are deterministic native-boundary tests, not OS or physical-radio
//! evidence. JNI binding-scoped completion is covered separately by the JVM
//! scan evidence harness.

mod common;

use common::{Reply, call, ok, parse};
use serde_json::json;
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use ubm_mobile::{
    FailureKind, HostOptions, MobileHost, MobilePlatform, OrderedAdmissionNegotiation,
    OrderedAdmissionOwner, OrderedOpenCompletionV2, OrderedOpenIdentity, OrderedOpenOutcomeV2,
    OrderedOpenOwnershipV2, OrderedOpenRequestV2, OrderedOpenResultV2, OrderedOpenRetired,
    PlatformFailure, PlatformRadio, RadioCompletion, RadioIngress, RadioRequest, RequestKind,
};

const OWNER: &str = "12345678-1234-4abc-8def-000000005221";

// Represents an open which has entered the native source factory and cannot
// truthfully be declared NotCreated until its execution has settled.
struct ExecutingOwner {
    owner: OrderedAdmissionOwner,
    host: OnceLock<Weak<MobileHost>>,
    settled: AtomicBool,
    requests: Mutex<Vec<RadioRequest>>,
}

impl ExecutingOwner {
    fn open_identity(&self) -> Option<OrderedOpenIdentity> {
        self.requests.lock().unwrap().iter().find_map(|r| match r {
            RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Open { identity, .. }) => {
                Some(identity.clone())
            }
            _ => None,
        })
    }
}

impl PlatformRadio for ExecutingOwner {
    fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<OrderedAdmissionNegotiation> {
        Some(OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn submit(&self, request: RadioRequest) {
        self.requests.lock().unwrap().push(request.clone());
        let completion = match &request {
            RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Open { .. }) => return,
            RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Recover { identity, .. }) => {
                let outcome = if self.settled.load(Ordering::SeqCst) {
                    OrderedOpenOutcomeV2::Failed {
                        original_failure: PlatformFailure::new(
                            FailureKind::Platform,
                            "factory settled without creating a source",
                        ),
                        ownership: OrderedOpenOwnershipV2::NotCreated,
                    }
                } else {
                    OrderedOpenOutcomeV2::Pending
                };
                RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Result(
                    OrderedOpenResultV2 {
                        slot_identity: identity.slot_identity(),
                        outcome,
                    },
                ))
            }
            RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Acknowledge { identity, .. }) => {
                assert!(
                    self.settled.load(Ordering::SeqCst),
                    "cannot acknowledge an executing open"
                );
                RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Retired(
                    OrderedOpenRetired {
                        slot_identity: identity.slot_identity(),
                    },
                ))
            }
            RadioRequest::Discover { .. } => RadioCompletion::DiscoveredScoped {
                services: common::polar_services(),
                identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "native-522".into(),
                    attachment: 1,
                    revision: 1,
                },
            },
            _ => match common::polar_responder(&request) {
                Reply::Now(value) => value,
                Reply::Hold => return,
            },
        };
        self.host
            .get()
            .and_then(Weak::upgrade)
            .expect("live host route")
            .complete(request.id(), completion);
    }
    fn cancel(&self, _: u64) {}
}

async fn until(message: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect(message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_session_admission_before_awaiting_disposal() {
    let radio = common::Scripted::new(Box::new(|request| match request {
        RadioRequest::StopScan { .. } => Reply::Hold,
        other => common::polar_responder(other),
    }));
    let (host, _) = common::open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("existing").unwrap();
    ok(&call(
        &session,
        "scan.start",
        r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
    )
    .await);
    let shutdown = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    until("disposal did not reach scan stop", || {
        !radio.held_of(RequestKind::StopScan).is_empty()
    })
    .await;
    let opened = host.open_session("too-late");
    let id = radio.held_of(RequestKind::StopScan)[0];
    radio.answer(id, RadioCompletion::Unit);
    let receipt = shutdown.await.unwrap();
    assert!(
        opened.is_err(),
        "shutdown admitted a session outside its disposal snapshot"
    );
    assert_eq!(parse(&receipt)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_execution_survives_failed_shutdown_and_exact_slot_evidence_is_retried() {
    let radio = Arc::new(ExecutingOwner {
        owner: OrderedAdmissionOwner::negotiate_v2(OWNER).unwrap(),
        host: OnceLock::new(),
        settled: AtomicBool::new(false),
        requests: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "quiescence-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let session = host.open_session("held-execution").unwrap();
    ok(&call(
        &session,
        "scan.start",
        r#"{"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}"#,
    )
    .await);
    host.ingest(RadioIngress::Advertisement(ubm_mobile::Advertisement {
        peer_id: common::POLAR.into(),
        service_uuids: vec!["180D".into()],
        ..Default::default()
    }));
    common::drain_until(&session, |records| {
        !common::of_type(records, "adv").is_empty()
    })
    .await;
    let connection = ok(&call(
        &session,
        "connection.connect",
        &json!({"peerId":common::POLAR,"lease":"lease","operationId":"connect"}).to_string(),
    )
    .await);
    let database = ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId":common::POLAR,"lease":"lease","operationId":"discover"}).to_string(),
    )
    .await);
    let reserved = ok(&call(
        &session,
        "gatt.ordered.reserve",
        &json!({"peerId":common::POLAR}).to_string(),
    )
    .await);
    let args = json!({"wireVersion":2,"peerId":common::POLAR,"collectionId":reserved["collectionId"],"connectionGeneration":connection["connectionGeneration"],"databaseGeneration":database["databaseGeneration"],"publicConnectionGeneration":"c","publicDatabaseGeneration":"d","members":[{"memberId":"0","publicMemberId":"a","selector":common::selector(),"requestedDelivery":"prefer-notification"}],"maxItems":8,"maxBytes":128,"maxControls":8,"operationId":"ordered-open"}).to_string();
    let opening = tokio::spawn({
        let session = session.clone();
        async move { call(&session, "gatt.ordered.open", &args).await }
    });
    until("native open was not submitted", || {
        radio.open_identity().is_some()
    })
    .await;
    let identity = radio.open_identity().unwrap();
    ok(&call(&session, "op.cancel", r#"{"operationId":"ordered-open"}"#).await);
    let _ = tokio::time::timeout(Duration::from_secs(5), opening)
        .await
        .unwrap()
        .unwrap();
    let first = parse(
        &tokio::time::timeout(Duration::from_secs(8), host.shutdown())
            .await
            .expect("shutdown must report unresolved work"),
    );
    assert_eq!(
        first["state"], "release-failed",
        "executing source factory cannot be quiescent: {first}"
    );
    assert!(
        first["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["resourceKind"] == "ordered-admission"),
        "open debt must remain visible: {first}"
    );
    assert!(!radio.requests.lock().unwrap().iter().any(|r| matches!(
        r,
        RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Acknowledge { .. })
    )));

    // The real recovery driver must still route native replies after failure.
    // The retained identity, including slot generation and descriptor, must
    // remain identical through recovery and acknowledgement.
    radio.settled.store(true, Ordering::SeqCst);
    until("recovery route did not acknowledge the exact settled open", || radio.requests.lock().unwrap().iter().any(|r| matches!(r,
        RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Acknowledge { identity: got, .. }) if got == &identity))).await;
    {
        let requests = radio.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .filter_map(|r| match r {
                    RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Recover {
                        identity: got,
                        ..
                    }) => Some(got),
                    _ => None,
                })
                .all(|got| got == &identity)
        );
    }
    // A slot retirement is not an owner quiescence receipt.
    let retry = parse(
        &tokio::time::timeout(Duration::from_secs(8), host.shutdown())
            .await
            .unwrap(),
    );
    assert_eq!(
        retry["state"], "release-failed",
        "slot evidence alone cannot prove owner quiescence: {retry}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_host_callback_does_not_complete_a_replacement_host_request() {
    // This establishes host-bound delivery only. Production binding routing is
    // a separate boundary and is intentionally not simulated here.
    let old = common::Scripted::new(Box::new(|r| {
        if matches!(r, RadioRequest::AdapterState { .. }) {
            Reply::Hold
        } else {
            common::polar_responder(r)
        }
    }));
    let new = common::Scripted::new(Box::new(|r| {
        if matches!(r, RadioRequest::AdapterState { .. }) {
            Reply::Hold
        } else {
            common::polar_responder(r)
        }
    }));
    let (old_host, _) = common::open(&old, MobilePlatform::Android).await;
    let old_session = old_host.open_session("old").unwrap();
    let old_call = tokio::spawn(async move { call(&old_session, "adapter.state", "{}").await });
    until("old request not held", || {
        !old.held_of(RequestKind::AdapterState).is_empty()
    })
    .await;
    let (new_host, _) = common::open(&new, MobilePlatform::Android).await;
    let new_session = new_host.open_session("replacement").unwrap();
    let new_call = tokio::spawn(async move { call(&new_session, "adapter.state", "{}").await });
    until("replacement request not held", || {
        !new.held_of(RequestKind::AdapterState).is_empty()
    })
    .await;
    let old_id = old.held_of(RequestKind::AdapterState)[0];
    let new_id = new.held_of(RequestKind::AdapterState)[0];
    old.answer(old_id, RadioCompletion::Adapter(common::adapter_on()));
    ok(&tokio::time::timeout(Duration::from_secs(3), old_call)
        .await
        .unwrap()
        .unwrap());
    assert_eq!(
        old_host.complete(old_id, RadioCompletion::Adapter(common::adapter_on())),
        ubm_mobile::CompletionStatus::Late
    );
    assert!(
        !new_call.is_finished(),
        "old host callback must not settle replacement"
    );
    new.answer(new_id, RadioCompletion::Adapter(common::adapter_on()));
    ok(&tokio::time::timeout(Duration::from_secs(3), new_call)
        .await
        .unwrap()
        .unwrap());
    let _ = old_host.shutdown().await;
    let _ = new_host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_shutdown_retries_native_cleanup_and_waits_for_its_completion() {
    let mut disconnects = 0;
    let radio = common::Scripted::new(Box::new(move |request| match request {
        RadioRequest::Connect { .. } => Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            "connection refused",
        ))),
        RadioRequest::Disconnect { .. } => {
            disconnects += 1;
            if disconnects <= 2 {
                Reply::Now(RadioCompletion::Failed(PlatformFailure::new(
                    FailureKind::Platform,
                    "disconnect refused",
                )))
            } else {
                Reply::Hold
            }
        }
        other => common::polar_responder(other),
    }));
    let (host, _) = common::open(&radio, MobilePlatform::Android).await;
    let session = host.open_session("cleanup-debt").unwrap();
    common::failure(
        &call(
            &session,
            "connection.connect",
            &json!({"peerId":common::POLAR,"lease":"failed","operationId":"connect"}).to_string(),
        )
        .await,
    );
    let first = parse(
        &tokio::time::timeout(Duration::from_secs(5), host.shutdown())
            .await
            .unwrap(),
    );
    assert_eq!(first["state"], "release-failed");
    assert!(
        first["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["resourceKind"] == "connection")
    );

    let retry = tokio::spawn({
        let host = Arc::clone(&host);
        async move { host.shutdown().await }
    });
    until("retry did not reach native cleanup", || {
        !radio.held_of(RequestKind::Disconnect).is_empty()
    })
    .await;
    assert!(
        !retry.is_finished(),
        "held cleanup cannot establish release"
    );
    let id = radio.held_of(RequestKind::Disconnect)[0];
    assert_eq!(
        radio.answer(id, RadioCompletion::Unit),
        ubm_mobile::CompletionStatus::Delivered
    );
    let receipt = parse(
        &tokio::time::timeout(Duration::from_secs(5), retry)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(receipt, json!({"state":"released", "failures":[]}));
    assert_eq!(parse(&host.shutdown().await), receipt);
    assert_eq!(radio.count(RequestKind::Disconnect), 3);
}

#[test]
fn owner_transport_v4_negotiates_without_changing_slot_identity_version() {
    // The new owner retirement protocol is distinct from v2 slot identities.
    let negotiation = OrderedAdmissionNegotiation::negotiate(4, OWNER)
        .expect("native owner transport v4 must support quiescence receipts");
    assert_eq!(negotiation.version, 4);
    assert_eq!(negotiation.owner.to_string(), OWNER);
    assert_eq!(ubm_mobile::ORDERED_ADMISSION_SLOT_VERSION, 2);
}

struct MissingRetirementOwner {
    owner: OrderedAdmissionOwner,
}

impl PlatformRadio for MissingRetirementOwner {
    fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<OrderedAdmissionNegotiation> {
        Some(OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn ordered_owner_retirement_version(&self) -> u32 {
        4
    }
    fn submit(&self, _: RadioRequest) {
        panic!("an incomplete v4 owner negotiation must not submit a native request");
    }
    fn cancel(&self, _: u64) {}
}

#[tokio::test]
async fn retirement_version_without_owner_identity_fails_closed_without_panicking() {
    let radio = Arc::new(MissingRetirementOwner {
        owner: OrderedAdmissionOwner::negotiate_v2(OWNER).unwrap(),
    });
    let host = MobileHost::open(
        radio,
        Arc::new(common::Wakes::default()),
        HostOptions {
            platform: MobilePlatform::Android,
            owner: "quiescence-522-incomplete-v4".into(),
            adapter_label: "scripted".into(),
        },
        tokio::runtime::Handle::current(),
    )
    .await
    .unwrap();
    let receipt = parse(&host.shutdown().await);
    assert_eq!(receipt["state"], "release-failed");
    assert!(
        receipt["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|failure| failure["resourceKind"] == "ordered-owner"),
        "missing v4 owner identity must become an actionable cleanup failure: {receipt}"
    );
}

struct RetiringOwner {
    owner: OrderedAdmissionOwner,
    host: OnceLock<Weak<MobileHost>>,
    fences: Mutex<Vec<RadioRequest>>,
    cancelled: Mutex<Vec<u64>>,
}

impl PlatformRadio for RetiringOwner {
    fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn ordered_admission_negotiation(&self) -> Option<OrderedAdmissionNegotiation> {
        Some(OrderedAdmissionNegotiation {
            version: 2,
            owner: self.owner,
        })
    }
    fn ordered_owner_retirement_version(&self) -> u32 {
        4
    }
    fn ordered_owner_retirement_owner(&self) -> Option<OrderedAdmissionOwner> {
        Some(self.owner)
    }
    fn submit(&self, request: RadioRequest) {
        if matches!(
            request,
            RadioRequest::OwnerFence { .. } | RadioRequest::OwnerStatus { .. }
        ) {
            self.fences.lock().unwrap().push(request);
            return;
        }
        if let Reply::Now(answer) = common::polar_responder(&request) {
            self.host
                .get()
                .and_then(Weak::upgrade)
                .unwrap()
                .complete(request.id(), answer);
        }
    }
    fn cancel(&self, id: u64) {
        self.cancelled.lock().unwrap().push(id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_waiter_and_failed_fence_keep_exact_owner_for_retry() {
    use ubm_mobile::{
        CompletionStatus, OwnerRetirementRejection, OwnerRetirementResult, OwnerRetirementState,
    };
    let radio = Arc::new(RetiringOwner {
        owner: OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005222").unwrap(),
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "retry-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let first = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    until("first fence not submitted", || {
        radio.fences.lock().unwrap().len() == 1
    })
    .await;
    let first_request = radio.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation: owner,
        retirement_id: retirement,
        ..
    } = first_request
    else {
        panic!("expected fence")
    };
    first.abort();
    let retry = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(radio.fences.lock().unwrap().len(), 1);
    assert!(
        !radio
            .cancelled
            .lock()
            .unwrap()
            .contains(&first_request.id())
    );
    let receipt = |owner_incarnation, retirement_id, state| {
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state,
        })
    };
    assert_eq!(
        host.complete(
            first_request.id(),
            receipt(
                owner,
                retirement,
                OwnerRetirementState::Rejected {
                    reason: OwnerRetirementRejection::WrongOwner
                }
            )
        ),
        CompletionStatus::Delivered
    );
    until("settled fence was not retried", || {
        radio.fences.lock().unwrap().len() == 2
    })
    .await;
    let second = radio.fences.lock().unwrap()[1].clone();
    assert!(
        matches!(second, RadioRequest::OwnerFence { owner_incarnation, retirement_id, .. } if owner_incarnation == owner && retirement_id == retirement)
    );
    let wrong =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005223").unwrap();
    assert_eq!(
        host.complete(
            second.id(),
            receipt(owner, wrong, OwnerRetirementState::Quiescent)
        ),
        CompletionStatus::Mismatched
    );
    assert_eq!(
        host.complete(
            second.id(),
            receipt(owner, retirement, OwnerRetirementState::Quiescent)
        ),
        CompletionStatus::Delivered
    );
    let _ = retry.await.unwrap();
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutting_one_host_does_not_fence_another_host_on_the_same_owner() {
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005224").unwrap();
    let make_radio = || {
        Arc::new(RetiringOwner {
            owner,
            host: OnceLock::new(),
            fences: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        })
    };
    let first = make_radio();
    let second = make_radio();
    let open = |radio: Arc<RetiringOwner>| async move {
        let host = Arc::new(
            MobileHost::open(
                radio.clone(),
                Arc::new(common::Wakes::default()),
                HostOptions {
                    platform: MobilePlatform::Android,
                    owner: "shared-owner-522".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
        host
    };
    let a = open(first.clone()).await;
    let b = open(second.clone()).await;
    let receipt = parse(&a.shutdown().await);
    assert_eq!(receipt["state"], "release-failed");
    assert!(first.fences.lock().unwrap().is_empty());
    assert!(second.fences.lock().unwrap().is_empty());
    assert!(b.open_session("still-live").is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_settled_hosts_allow_one_final_owner_retirement() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005225").unwrap();
    let make_radio = || {
        Arc::new(RetiringOwner {
            owner,
            host: OnceLock::new(),
            fences: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        })
    };
    let first = make_radio();
    let second = make_radio();
    let open = |radio: Arc<RetiringOwner>| async move {
        let host = Arc::new(
            MobileHost::open(
                radio.clone(),
                Arc::new(common::Wakes::default()),
                HostOptions {
                    platform: MobilePlatform::Android,
                    owner: "shared-owner-retire-522".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
        host
    };
    let a = open(first.clone()).await;
    let b = open(second.clone()).await;
    assert_eq!(parse(&a.shutdown().await)["state"], "release-failed");
    assert!(b.open_session("still-live").is_ok());
    let last = tokio::spawn({
        let host = b.clone();
        async move { host.shutdown().await }
    });
    until("final owner fence not submitted", || {
        !second.fences.lock().unwrap().is_empty()
    })
    .await;
    let request = second.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = request
    else {
        panic!("expected fence")
    };
    b.complete(
        request.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    assert_eq!(parse(&last.await.unwrap())["state"], "released");
    assert_eq!(parse(&a.shutdown().await)["state"], "released");
    assert!(first.fences.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_host_shutdowns_elect_only_one_native_retiree() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005226").unwrap();
    let make_radio = || {
        Arc::new(RetiringOwner {
            owner,
            host: OnceLock::new(),
            fences: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        })
    };
    let first = make_radio();
    let second = make_radio();
    let open = |radio: Arc<RetiringOwner>| async move {
        let host = Arc::new(
            MobileHost::open(
                radio.clone(),
                Arc::new(common::Wakes::default()),
                HostOptions {
                    platform: MobilePlatform::Android,
                    owner: "concurrent-owner-522".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
        host
    };
    let a = open(first.clone()).await;
    let b = open(second.clone()).await;
    let ta = tokio::spawn({
        let host = a.clone();
        async move { host.shutdown().await }
    });
    let tb = tokio::spawn({
        let host = b.clone();
        async move { host.shutdown().await }
    });
    until("no host elected for native retirement", || {
        !first.fences.lock().unwrap().is_empty() || !second.fences.lock().unwrap().is_empty()
    })
    .await;
    let (radio, host, other) = if !first.fences.lock().unwrap().is_empty() {
        (&first, &a, &second)
    } else {
        (&second, &b, &first)
    };
    assert!(other.fences.lock().unwrap().is_empty());
    let request = radio.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = request
    else {
        panic!("expected fence")
    };
    host.complete(
        request.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    let _ = ta.await.unwrap();
    let _ = tb.await.unwrap();
    assert_eq!(parse(&a.shutdown().await)["state"], "released");
    assert_eq!(parse(&b.shutdown().await)["state"], "released");
    assert!(other.fences.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_elected_waiter_transfers_exact_retirement_to_other_settled_host() {
    use ubm_mobile::{OwnerRetirementRejection, OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005227").unwrap();
    let make_radio = || {
        Arc::new(RetiringOwner {
            owner,
            host: OnceLock::new(),
            fences: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        })
    };
    let first = make_radio();
    let second = make_radio();
    let open = |radio: Arc<RetiringOwner>| async move {
        let host = Arc::new(
            MobileHost::open(
                radio.clone(),
                Arc::new(common::Wakes::default()),
                HostOptions {
                    platform: MobilePlatform::Android,
                    owner: "transfer-owner-522".into(),
                    adapter_label: "scripted".into(),
                },
                tokio::runtime::Handle::current(),
            )
            .await
            .unwrap(),
        );
        assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
        host
    };
    let a = open(first.clone()).await;
    let b = open(second.clone()).await;
    assert_eq!(parse(&a.shutdown().await)["state"], "release-failed");
    let elected = tokio::spawn({
        let host = b.clone();
        async move { host.shutdown().await }
    });
    until("elected fence not submitted", || {
        second.fences.lock().unwrap().len() == 1
    })
    .await;
    let initial = second.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = initial
    else {
        panic!("expected fence")
    };
    elected.abort();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(first.fences.lock().unwrap().is_empty());
    assert_eq!(
        second.fences.lock().unwrap().len(),
        1,
        "unsettled native attempt overlapped"
    );
    b.complete(
        initial.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Rejected {
                reason: OwnerRetirementRejection::WrongOwner,
            },
        }),
    );
    until("shared owner did not autonomously retry", || {
        second.fences.lock().unwrap().len() >= 2
    })
    .await;
    let retry = second.fences.lock().unwrap()[1].clone();
    assert!(
        matches!(retry, RadioRequest::OwnerFence { owner_incarnation: o, retirement_id: r, .. } if o == owner_incarnation && r == retirement_id)
    );
    b.complete(
        retry.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::FencedPending {
                outstanding_classes: vec![],
                failures: vec![],
            },
        }),
    );
    until("shared owner did not autonomously poll", || {
        second.fences.lock().unwrap().len() >= 3
    })
    .await;
    let status = second.fences.lock().unwrap()[2].clone();
    assert!(
        matches!(status, RadioRequest::OwnerStatus { owner_incarnation: o, retirement_id: r, .. } if o == owner_incarnation && r == retirement_id)
    );
    b.complete(
        status.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    assert_eq!(parse(&b.shutdown().await)["state"], "released");
    assert_eq!(parse(&a.shutdown().await)["state"], "released");
    assert!(first.fences.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_retirement_waits_for_native_settlement_then_retries_autonomously() {
    use ubm_mobile::{OwnerRetirementRejection, OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005229").unwrap();
    let radio = Arc::new(RetiringOwner {
        owner,
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "autonomous-owner-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let waiter = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    until("first fence not submitted", || {
        radio.fences.lock().unwrap().len() == 1
    })
    .await;
    let first = radio.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = first
    else {
        panic!("expected fence")
    };
    waiter.abort();
    let other = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(
        radio.fences.lock().unwrap().len(),
        1,
        "native attempts overlapped without settlement"
    );
    other.abort();
    host.complete(
        first.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Rejected {
                reason: OwnerRetirementRejection::WrongOwner,
            },
        }),
    );
    until("owner did not autonomously retry settled failure", || {
        radio.fences.lock().unwrap().len() >= 2
    })
    .await;
    let second = radio.fences.lock().unwrap()[1].clone();
    assert!(
        matches!(second, RadioRequest::OwnerFence { owner_incarnation: o, retirement_id: r, .. } if o == owner_incarnation && r == retirement_id)
    );
    host.complete(
        second.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::FencedPending {
                outstanding_classes: vec![],
                failures: vec![],
            },
        }),
    );
    until("owner did not autonomously poll pending fence", || {
        radio.fences.lock().unwrap().len() >= 3
    })
    .await;
    let third = radio.fences.lock().unwrap()[2].clone();
    assert!(
        matches!(third, RadioRequest::OwnerStatus { owner_incarnation: o, retirement_id: r, .. } if o == owner_incarnation && r == retirement_id)
    );
    host.complete(
        third.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    assert_eq!(
        parse(
            &tokio::time::timeout(Duration::from_secs(5), host.shutdown())
                .await
                .unwrap()
        )["state"],
        "released"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_failure_does_not_authorize_another_native_attempt() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005230").unwrap();
    let radio = Arc::new(RetiringOwner {
        owner,
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "ambiguous-owner-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let first = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    until("first fence not submitted", || {
        radio.fences.lock().unwrap().len() == 1
    })
    .await;
    let request = radio.fences.lock().unwrap()[0].clone();
    host.complete(
        request.id(),
        RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            "native settlement unknown",
        )),
    );
    assert_eq!(parse(&first.await.unwrap())["state"], "release-failed");
    let second = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        radio.fences.lock().unwrap().len(),
        1,
        "an ambiguous error does not establish settlement"
    );
    second.abort();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = request
    else {
        panic!("expected fence")
    };
    let wrong =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005299").unwrap();
    assert_eq!(
        host.complete(
            request.id(),
            RadioCompletion::OwnerRetirement(OwnerRetirementResult {
                owner_incarnation,
                retirement_id: wrong,
                state: OwnerRetirementState::Quiescent,
            })
        ),
        ubm_mobile::CompletionStatus::Mismatched
    );
    assert_eq!(
        host.complete(
            request.id(),
            RadioCompletion::OwnerRetirement(OwnerRetirementResult {
                owner_incarnation,
                retirement_id,
                state: OwnerRetirementState::Quiescent,
            })
        ),
        ubm_mobile::CompletionStatus::Delivered
    );
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_panic_after_dispatch_cannot_authorize_an_overlapping_attempt() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    struct PanickingRadio(RetiringOwner);
    impl PlatformRadio for PanickingRadio {
        fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
            Some(self.0.owner)
        }
        fn ordered_admission_negotiation(&self) -> Option<OrderedAdmissionNegotiation> {
            Some(OrderedAdmissionNegotiation {
                version: 2,
                owner: self.0.owner,
            })
        }
        fn ordered_owner_retirement_version(&self) -> u32 {
            4
        }
        fn ordered_owner_retirement_owner(&self) -> Option<OrderedAdmissionOwner> {
            Some(self.0.owner)
        }
        fn submit(&self, request: RadioRequest) {
            if matches!(
                request,
                RadioRequest::OwnerFence { .. } | RadioRequest::OwnerStatus { .. }
            ) {
                self.0.fences.lock().unwrap().push(request);
                panic!("injected callback failure after native dispatch");
            }
            self.0.submit(request);
        }
        fn cancel(&self, id: u64) {
            self.0.cancel(id);
        }
    }
    let radio = Arc::new(PanickingRadio(RetiringOwner {
        owner: OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005231").unwrap(),
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    }));
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "panic-owner-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.0.host.set(Arc::downgrade(&host)).is_ok());
    assert_eq!(parse(&host.shutdown().await)["state"], "release-failed");
    assert_eq!(parse(&host.shutdown().await)["state"], "release-failed");
    assert_eq!(
        radio.0.fences.lock().unwrap().len(),
        1,
        "callback panic does not establish native settlement"
    );
    let request = radio.0.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = request
    else {
        panic!("expected fence")
    };
    assert_eq!(
        host.complete(
            request.id(),
            RadioCompletion::OwnerRetirement(OwnerRetirementResult {
                owner_incarnation,
                retirement_id,
                state: OwnerRetirementState::Quiescent,
            })
        ),
        ubm_mobile::CompletionStatus::Delivered
    );
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_pending_receipt_preserves_failures_and_resumes_status_polling() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005234").unwrap();
    let radio = Arc::new(RetiringOwner {
        owner,
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "late-pending-owner-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let first = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    until("first fence not submitted", || {
        radio.fences.lock().unwrap().len() == 1
    })
    .await;
    let request = radio.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = request
    else {
        panic!("expected fence")
    };
    host.complete(
        request.id(),
        RadioCompletion::Failed(PlatformFailure::new(
            FailureKind::Platform,
            "unknown submit",
        )),
    );
    assert_eq!(parse(&first.await.unwrap())["state"], "release-failed");
    let mut first_failure =
        PlatformFailure::new(FailureKind::PermissionDenied, "native cleanup first");
    first_failure.gatt_status = Some(7);
    first_failure.native_domain = Some("CBErrorDomain".into());
    first_failure.native_code = Some(101);
    let mut second_failure =
        PlatformFailure::new(FailureKind::Unsupported, "native cleanup second");
    second_failure.gatt_status = Some(19);
    second_failure.native_domain = Some("CBATTErrorDomain".into());
    second_failure.native_code = Some(202);
    assert_eq!(
        host.complete(
            request.id(),
            RadioCompletion::OwnerRetirement(OwnerRetirementResult {
                owner_incarnation,
                retirement_id,
                state: OwnerRetirementState::FencedPending {
                    outstanding_classes: vec![],
                    failures: vec![first_failure, second_failure],
                },
            })
        ),
        ubm_mobile::CompletionStatus::Delivered
    );
    until("late pending receipt did not resume status polling", || {
        radio.fences.lock().unwrap().len() == 2
    })
    .await;
    let status = radio.fences.lock().unwrap()[1].clone();
    assert!(matches!(status, RadioRequest::OwnerStatus { .. }));
    let pending = parse(&host.shutdown().await);
    let failures = pending["failures"].as_array().expect("cleanup failures");
    let first = failures
        .iter()
        .find(|failure| {
            failure["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("native cleanup first"))
        })
        .expect("first native failure must survive: {pending}");
    assert_eq!(first["code"], "permission.denied");
    assert_eq!(first["platform"]["domain"], "CBErrorDomain");
    assert_eq!(first["platform"]["code"], "101");
    let second = failures
        .iter()
        .find(|failure| {
            failure["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("native cleanup second"))
        })
        .expect("second native failure must survive: {pending}");
    assert_eq!(second["code"], "capability.unsupported");
    assert_eq!(second["platform"]["domain"], "CBATTErrorDomain");
    assert_eq!(second["platform"]["code"], "202");
    host.complete(
        status.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_not_dispatched_failure_is_retried_without_a_caller() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005232").unwrap();
    let radio = Arc::new(RetiringOwner {
        owner,
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "not-dispatched-owner-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let waiter = tokio::spawn({
        let host = host.clone();
        async move { host.shutdown().await }
    });
    until("first fence not submitted", || {
        radio.fences.lock().unwrap().len() == 1
    })
    .await;
    let first = radio.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = first
    else {
        panic!("expected fence")
    };
    waiter.abort();
    host.complete(
        first.id(),
        RadioCompletion::Failed(PlatformFailure::not_dispatched(
            FailureKind::Busy,
            "dispatch refused",
        )),
    );
    until("known no-effect failure was not retried", || {
        radio.fences.lock().unwrap().len() >= 2
    })
    .await;
    let retry = radio.fences.lock().unwrap()[1].clone();
    assert!(
        matches!(retry, RadioRequest::OwnerFence { owner_incarnation: o, retirement_id: r, .. } if o == owner_incarnation && r == retirement_id)
    );
    let pending = parse(&host.shutdown().await);
    assert_eq!(pending["state"], "release-failed");
    assert!(
        pending.to_string().contains("dispatch refused"),
        "previous cleanup failure disappeared: {pending}"
    );
    host.complete(
        retry.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdown_callers_do_not_bypass_retirement_backoff() {
    use ubm_mobile::{OwnerRetirementResult, OwnerRetirementState};
    let owner =
        OrderedAdmissionOwner::negotiate_v2("12345678-1234-4abc-8def-000000005233").unwrap();
    let radio = Arc::new(RetiringOwner {
        owner,
        host: OnceLock::new(),
        fences: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "coalesced-owner-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    assert!(radio.host.set(Arc::downgrade(&host)).is_ok());
    let mut callers = Vec::new();
    for _ in 0..8 {
        let host = host.clone();
        callers.push(tokio::spawn(async move { host.shutdown().await }));
    }
    until("first fence not submitted", || {
        radio.fences.lock().unwrap().len() == 1
    })
    .await;
    for caller in callers {
        assert_eq!(parse(&caller.await.unwrap())["state"], "release-failed");
    }
    let first = radio.fences.lock().unwrap()[0].clone();
    let RadioRequest::OwnerFence {
        owner_incarnation,
        retirement_id,
        ..
    } = first
    else {
        panic!("expected fence")
    };
    host.complete(
        first.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::FencedPending {
                outstanding_classes: vec![],
                failures: vec![],
            },
        }),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        radio.fences.lock().unwrap().len(),
        1,
        "waiting callers bypassed retry backoff"
    );
    until("supervisor did not poll", || {
        radio.fences.lock().unwrap().len() == 2
    })
    .await;
    let status = radio.fences.lock().unwrap()[1].clone();
    let mut more_callers = Vec::new();
    for _ in 0..8 {
        let host = host.clone();
        more_callers.push(tokio::spawn(async move { host.shutdown().await }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        radio.fences.lock().unwrap().len(),
        2,
        "callers overlapped a held status request"
    );
    host.complete(
        status.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::FencedPending {
                outstanding_classes: vec![],
                failures: vec![],
            },
        }),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        radio.fences.lock().unwrap().len(),
        2,
        "callers bypassed backoff after a pending status"
    );
    until("supervisor did not retry pending status", || {
        radio.fences.lock().unwrap().len() == 3
    })
    .await;
    let status = radio.fences.lock().unwrap()[2].clone();
    host.complete(
        status.id(),
        RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation,
            retirement_id,
            state: OwnerRetirementState::Quiescent,
        }),
    );
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    for caller in more_callers {
        let _ = caller.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_error_survives_a_later_cleanup_panic() {
    struct PanicOnCancel {
        inner: Arc<common::Scripted>,
        panic_id: Mutex<Option<u64>>,
    }
    impl PlatformRadio for PanicOnCancel {
        fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
            self.inner.ordered_admission_owner()
        }
        fn submit(&self, request: RadioRequest) {
            self.inner.submit(request);
        }
        fn cancel(&self, id: u64) {
            if *self.panic_id.lock().unwrap() == Some(id) {
                panic!("injected later cleanup panic");
            }
            self.inner.cancel(id);
        }
    }
    let scripted = common::Scripted::new(Box::new(|request| match request {
        RadioRequest::AssociateCompanion { .. } => Reply::Hold,
        other => common::polar_responder(other),
    }));
    let radio = Arc::new(PanicOnCancel {
        inner: scripted.clone(),
        panic_id: Mutex::new(None),
    });
    let host = Arc::new(
        MobileHost::open(
            radio.clone(),
            Arc::new(common::Wakes::default()),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "cleanup-panic-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    scripted.bind_host(&host);
    let first = host.open_session("first").unwrap();
    let second = host.open_session("second").unwrap();
    let mut requests = Vec::new();
    for session in [first, second] {
        let pending = tokio::spawn({
            let session = session.clone();
            async move {
                call(
                    &session,
                    "companion.associate",
                    r#"{"name":"Polar","operationId":"choice"}"#,
                )
                .await
            }
        });
        until("companion request not submitted", || {
            scripted.held_of(RequestKind::AssociateCompanion).len() > requests.len()
        })
        .await;
        requests = scripted.held_of(RequestKind::AssociateCompanion);
        ok(&call(&session, "op.cancel", r#"{"operationId":"choice"}"#).await);
        let _ = pending.await.unwrap();
    }
    *radio.panic_id.lock().unwrap() = requests.last().copied();
    let receipt = parse(&host.shutdown().await);
    assert_eq!(receipt["state"], "release-failed");
    let failures = receipt["failures"].to_string();
    assert!(
        failures.contains("companion.associate.release"),
        "earlier cleanup failure was lost: {failures}"
    );
    assert!(
        failures.contains("shutdown task failed"),
        "panic was not reported: {failures}"
    );
}
