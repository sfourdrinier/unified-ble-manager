//! Public-session scan attribution regressions.
mod common;

use common::*;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use ubm_mobile::{
    Advertisement, HostOptions, MobileHost, MobilePlatform, PlatformRadio, RadioIngress, WakeSink,
};

struct Wake;
impl WakeSink for Wake {
    fn wake(&self, _: u64) {}
}

async fn fixture() -> (Arc<MobileHost>, Arc<Scripted>) {
    let radio = Scripted::polar();
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "scan-identity-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    (host, radio)
}

async fn collect(session: &ubm_mobile::MobileSession, kind: &str, count: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut records = Vec::new();
        loop {
            records.extend(
                parse(&session.drain(256, 65536))["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            if records.iter().filter(|record| record["t"] == kind).count() >= count {
                return records;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected scan records")
}

#[tokio::test]
async fn filtered_member_receives_partial_observation_for_public_evidence_matcher() {
    let (host, _) = fixture().await;
    let session = host.open_session("filtered").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":["180d"],"duplicatePolicy":"all","operationId":"scan"}).to_string(),
    )
    .await);
    for services in [vec![HR_SERVICE.to_owned()], vec![]] {
        host.ingest(RadioIngress::Advertisement(Advertisement {
            peer_id: POLAR.into(),
            service_uuids: services,
            ..Advertisement::default()
        }));
    }
    let records = collect(&session, "adv", 2).await;
    let advertisements = of_type(&records, "adv");
    assert_eq!(advertisements.len(), 2);
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn scan_failure_retains_platform_cause_in_public_terminal() {
    let (host, _) = fixture().await;
    let session = host.open_session("failure").unwrap();
    assert_eq!(
        parse(&session.negotiate_scan_end_version("[1,2]").unwrap())["version"],
        2
    );
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}).to_string(),
    )
    .await);
    host.ingest(RadioIngress::ScanFailed {
        detail: "adapter powered off".into(),
    });
    let records = collect(&session, "scan-end", 1).await;
    let terminal = of_type(&records, "scan-end")[0];
    assert_eq!(terminal["reason"], "source-failed");
    assert_eq!(terminal["version"], 2);
    assert_eq!(terminal["startOperationId"], "scan");
    assert_eq!(terminal["source"], "radio");
    assert!(
        terminal["generation"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    assert!(
        terminal["error"]
            .to_string()
            .contains("adapter powered off"),
        "{terminal}"
    );
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observation_captured_before_join_is_not_delivered_to_new_member() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct ReleaseOnDrop(Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_one();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let (lock, ready) = &self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            }
        }
    }
    let radio = Scripted::polar();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release_on_drop = ReleaseOnDrop(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "scan-join-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let a = host.open_session("a").unwrap();
    let b = host.open_session("b").unwrap();
    let args = |id| json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":id}).to_string();
    ok(&call(&a, "scan.start", &args("a")).await);
    let _ = a.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: "first".into(),
        ..Advertisement::default()
    }));
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .expect("first observation holds pump");
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: "before-join".into(),
        ..Advertisement::default()
    }));
    ok(
        &tokio::time::timeout(Duration::from_secs(1), call(&b, "scan.start", &args("b")))
            .await
            .expect("covered member joins while pump is held"),
    );
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_one();
    let records = collect(&a, "adv", 2).await;
    assert!(records.iter().any(|r| r["peerId"] == "before-join"));
    let records_b = parse(&b.drain(256, 65536));
    assert!(
        !records_b["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["t"] == "adv" && r["peerId"] == "before-join"),
        "{records_b}"
    );
    ok(&call(&a, "session.dispose", "{}").await);
    ok(&call(&b, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn observation_during_held_start_reaches_the_successful_member() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) {
            tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "early-scan-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("early").unwrap();
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "scan.start",
                &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"early"})
                    .to_string(),
            )
            .await
        }
    });
    let id = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .unwrap()
        .unwrap();
    host.ingest(RadioIngress::Advertisement(Advertisement {
        peer_id: "early".into(),
        ..Advertisement::default()
    }));
    tokio::task::yield_now().await;
    radio.answer(id, ubm_mobile::RadioCompletion::Unit);
    ok(&pending.await.unwrap());
    let records = collect(&session, "adv", 1).await;
    assert!(records.iter().any(|r| r["peerId"] == "early"));
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn cancelled_held_start_releases_captured_ingress_before_seal() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) && tx.is_some() {
            tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-cancel-fence".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("cancel-start").unwrap();
    let start = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "scan.start",
                &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"held-start"})
                    .to_string(),
            )
            .await
        }
    });
    let generation = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: generation,
            advertisement: Advertisement {
                peer_id: "before-cancel".into(),
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    assert_eq!(
        ok(&call(&session, "session.quiesce", "{}").await)["state"],
        "pending"
    );
    assert_eq!(
        ok(&call(
            &session,
            "op.cancel",
            &json!({"operationId":"held-start"}).to_string()
        )
        .await)["state"],
        "cancellation-requested"
    );
    let (error, _) = failure(&start.await.unwrap());
    assert_eq!(error["code"], "operation.aborted");
    let records = collect(&session, "adv", 1).await;
    assert_eq!(of_type(&records, "adv")[0]["peerId"], "before-cancel");
    let sealed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let answer = ok(&call(&session, "session.quiesce", "{}").await);
            if answer["state"] == "sealed" {
                break answer;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(sealed["afterCutoffItems"], 0);
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn failure_captured_after_cutoff_does_not_issue_a_terminal() {
    let (host, radio) = fixture().await;
    let session = host.open_session("sealed-scan").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}).to_string(),
    )
    .await);
    assert_eq!(
        ok(&call(&session, "session.quiesce", "{}").await)["state"],
        "sealed"
    );
    assert_eq!(
        host.ingest(RadioIngress::ScanFailed {
            detail: "after cutoff".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r, ubm_mobile::RadioRequest::StopScan { .. }))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let records = parse(&session.drain(256, 65536));
    assert!(
        of_type(records["records"].as_array().unwrap(), "scan-end").is_empty(),
        "{records}"
    );
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn old_physical_scan_identity_cannot_target_replacement_membership() {
    let (host, radio) = fixture().await;
    let session = host.open_session("replacement").unwrap();
    let args = |id| json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":id}).to_string();
    let first = ok(&call(&session, "scan.start", &args("first")).await);
    let old = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId":first["operationId"]}).to_string(),
    )
    .await);
    ok(&call(&session, "scan.start", &args("second")).await);
    let new = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    assert_ne!(old, new);
    let ad = |peer: &str| Advertisement {
        peer_id: peer.into(),
        ..Advertisement::default()
    };
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: old,
            advertisement: ad("stale")
        }),
        ubm_mobile::IngressStatus::Dropped(ubm_mobile::IngressClass::Advertisement)
    );
    assert!(matches!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: old,
            detail: "old failure".into()
        }),
        ubm_mobile::IngressStatus::ScanFailureRefused { error, .. } if error.detail() == Some("old failure")
    ));
    host.ingest(RadioIngress::AdvertisementScoped {
        scan_request_id: new,
        advertisement: ad("current"),
    });
    let records = collect(&session, "adv", 1).await;
    assert!(records.iter().any(|r| r["peerId"] == "current"));
    assert!(!records.iter().any(|r| r["peerId"] == "stale"));
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn exact_scan_identity_is_not_transferable_between_hosts() {
    let (a, radio_a) = fixture().await;
    let (b, radio_b) = fixture().await;
    let sa = a.open_session("a").unwrap();
    let sb = b.open_session("b").unwrap();
    let args = json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}).to_string();
    ok(&call(&sa, "scan.start", &args).await);
    ok(&call(&sb, "scan.start", &args).await);
    let scan_id = |radio: &Scripted| {
        radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .find_map(|r| match r {
                ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap()
    };
    let (id_a, id_b) = (scan_id(&radio_a), scan_id(&radio_b));
    assert_ne!(id_a, id_b);
    assert_eq!(
        b.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: id_a,
            advertisement: Advertisement {
                peer_id: "foreign".into(),
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Dropped(ubm_mobile::IngressClass::Advertisement)
    );
    b.ingest(RadioIngress::AdvertisementScoped {
        scan_request_id: id_b,
        advertisement: Advertisement {
            peer_id: "own".into(),
            ..Advertisement::default()
        },
    });
    let records = collect(&sb, "adv", 1).await;
    assert!(!records.iter().any(|r| r["peerId"] == "foreign"));
    ok(&call(&sa, "session.dispose", "{}").await);
    ok(&call(&sb, "session.dispose", "{}").await);
    assert_eq!(parse(&a.shutdown().await)["state"], "released");
    assert_eq!(parse(&b.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn held_start_has_a_bounded_counted_advertisement_queue() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) {
            tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Apple,
                owner: "scan-bound-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("bound").unwrap();
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "scan.start",
                &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"held"})
                    .to_string(),
            )
            .await
        }
    });
    let id = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .unwrap()
        .unwrap();
    let mut dropped = 0;
    for n in 0..513 {
        if matches!(
            host.ingest(RadioIngress::AdvertisementScoped {
                scan_request_id: id,
                advertisement: Advertisement {
                    peer_id: format!("peer-{n}"),
                    ..Advertisement::default()
                },
            }),
            ubm_mobile::IngressStatus::Dropped(ubm_mobile::IngressClass::Advertisement)
        ) {
            dropped += 1;
        }
    }
    assert!(dropped > 0, "the bounded queue must report saturation");
    radio.answer(id, ubm_mobile::RadioCompletion::Unit);
    ok(&pending.await.unwrap());
    let records = collect(&session, "adv", 1).await;
    assert!(records.iter().any(|r| r["t"] == "adv"));
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_failure_observed_before_join_does_not_admit_a_new_member() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let (lock, ready) = &self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            }
        }
    }
    let radio = Scripted::polar();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "failure-join-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let a = host.open_session("a").unwrap();
    let b = host.open_session("b").unwrap();
    let args = |id| json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":id}).to_string();
    ok(&call(&a, "scan.start", &args("a")).await);
    let generation = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    let _ = a.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::AdvertisementScoped {
        scan_request_id: generation,
        advertisement: Advertisement {
            peer_id: "gate".into(),
            ..Advertisement::default()
        },
    });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "platform scan failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    let reply = tokio::time::timeout(Duration::from_secs(1), call(&b, "scan.start", &args("b")))
        .await
        .unwrap();
    assert_eq!(
        parse(&reply)["ok"],
        false,
        "failed physical scan must not admit a later member: {reply}"
    );
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    let records = collect(&a, "scan-end", 1).await;
    assert_eq!(of_type(&records, "scan-end").len(), 1);
    ok(&call(&a, "session.dispose", "{}").await);
    ok(&call(&b, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_failure_keeps_callback_membership_after_stop() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let (lock, ready) = &self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            }
        }
    }
    let radio = Scripted::polar();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "failure-stop-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("a").unwrap();
    session.negotiate_scan_end_version("[1,2]").unwrap();
    let started = ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"start"}).to_string(),
    )
    .await);
    let generation = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    let _ = session.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::AdvertisementScoped {
        scan_request_id: generation,
        advertisement: Advertisement {
            peer_id: "gate".into(),
            ..Advertisement::default()
        },
    });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "failure before stop".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId":started["operationId"]}).to_string(),
    )
    .await);
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    let records = collect(&session, "scan-end", 1).await;
    let terminal = of_type(&records, "scan-end")[0];
    assert_eq!(terminal["operationId"], started["operationId"]);
    assert_eq!(terminal["startOperationId"], "start");
    assert_eq!(terminal["generation"], generation.to_string());
    assert_eq!(terminal["error"]["detail"], "failure before stop");
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn failure_during_start_keeps_pending_start_identity() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) {
            tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-failure-start".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("early-failure").unwrap();
    session.negotiate_scan_end_version("[1,2]").unwrap();
    let pending = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "scan.start",
                &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"early-start"})
                    .to_string(),
            )
            .await
        }
    });
    let generation = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "early failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    radio.answer(generation, ubm_mobile::RadioCompletion::Unit);
    let started = ok(&pending.await.unwrap());
    let records = collect(&session, "scan-end", 1).await;
    let terminal = of_type(&records, "scan-end")[0];
    assert_eq!(terminal["operationId"], started["operationId"]);
    assert_eq!(terminal["startOperationId"], "early-start");
    assert_eq!(terminal["generation"], generation.to_string());
    assert_eq!(terminal["error"]["detail"], "early failure");
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

async fn held_start_failure_scenario(native_fails: bool) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) && tx.is_some() {
            tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "held-start-failure-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("scan").unwrap();
    let independent = host.open_session("independent").unwrap();
    session.negotiate_scan_end_version("[1,2]").unwrap();
    let start = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "scan.start",
                &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"start"})
                    .to_string(),
            )
            .await
        }
    });
    let generation = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: generation,
            advertisement: Advertisement {
                peer_id: "captured".into(),
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "held failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    assert_eq!(
        ok(&call(&session, "session.quiesce", "{}").await)["state"],
        "pending"
    );
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "unrelated".into(),
    });
    let independent_progress = tokio::time::timeout(
        Duration::from_millis(500),
        collect(&independent, "ingress-drop", 1),
    )
    .await;
    radio.answer(
        generation,
        if native_fails {
            ubm_mobile::RadioCompletion::Failed(ubm_mobile::PlatformFailure::new(
                ubm_mobile::FailureKind::Platform,
                "native start refused",
            ))
        } else {
            ubm_mobile::RadioCompletion::Unit
        },
    );
    let result = start.await.unwrap();
    let started = if native_fails {
        let _ = failure(&result);
        None
    } else {
        Some(ok(&result))
    };
    assert!(
        independent_progress.is_ok(),
        "held native start must not stall unrelated ingress"
    );
    let records = collect(&session, "scan-end", 1).await;
    let terminal = records.iter().position(|r| r["t"] == "scan-end").unwrap();
    assert!(
        records[..terminal]
            .iter()
            .any(|r| r["t"] == "adv" && r["peerId"] == "captured")
    );
    if let Some(started) = started {
        assert_eq!(records[terminal]["operationId"], started["operationId"]);
    }
    assert_eq!(records[terminal]["startOperationId"], "start");
    assert_eq!(records[terminal]["error"]["detail"], "held failure");
    assert_eq!(
        ok(&call(&session, "session.quiesce", "{}").await)["state"],
        "sealed"
    );
    ok(&call(&session, "session.dispose", "{}").await);
    ok(&call(&independent, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_start_failure_does_not_block_unrelated_ingress() {
    held_start_failure_scenario(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_native_start_preserves_captured_failure_and_prefix() {
    held_start_failure_scenario(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_failure_and_held_replacement_failure_keep_fifo_without_stalling() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let mut released = self.release.0.lock().unwrap();
                while !*released {
                    released = self.release.1.wait(released).unwrap();
                }
            }
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let mut starts = 0;
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) {
            starts += 1;
            if starts == 2 {
                tx.take().unwrap().send(request.id()).unwrap();
                return Reply::Hold;
            }
        }
        polar_responder(request)
    }));
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "held-replacement-failures".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let first = host.open_session("first").unwrap();
    let second = host.open_session("second").unwrap();
    let independent = host.open_session("independent").unwrap();
    first.negotiate_scan_end_version("[1,2]").unwrap();
    second.negotiate_scan_end_version("[1,2]").unwrap();
    let first_start = ok(&call(
        &first,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"first"}).to_string(),
    )
    .await);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "gate".into(),
    });
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailed {
            detail: "old failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    ok(&call(
        &first,
        "scan.stop",
        &json!({"operationId":first_start["operationId"]}).to_string(),
    )
    .await);
    let second_start = tokio::spawn({
        let second = second.clone();
        async move {
            call(
                &second,
                "scan.start",
                &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"second"})
                    .to_string(),
            )
            .await
        }
    });
    let generation = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: generation,
            advertisement: Advertisement {
                peer_id: "second-prefix".into(),
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "new failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    assert_eq!(
        ok(&call(&first, "session.quiesce", "{}").await)["state"],
        "pending"
    );
    assert_eq!(
        ok(&call(&second, "session.quiesce", "{}").await)["state"],
        "pending"
    );
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    host.ingest(RadioIngress::AdapterState(adapter_on()));
    let progress =
        tokio::time::timeout(Duration::from_secs(2), collect(&independent, "adapter", 1)).await;
    radio.answer(generation, ubm_mobile::RadioCompletion::Unit);
    let _ = second_start.await.unwrap();
    assert!(
        progress.is_ok(),
        "queued failures must not block unrelated ingress"
    );
    let old = collect(&first, "scan-end", 1).await;
    let new = collect(&second, "scan-end", 1).await;
    assert_eq!(
        of_type(&old, "scan-end")[0]["error"]["detail"],
        "old failure"
    );
    let terminal = new
        .iter()
        .position(|record| record["t"] == "scan-end")
        .unwrap();
    assert!(
        new[..terminal]
            .iter()
            .any(|record| record["t"] == "adv" && record["peerId"] == "second-prefix")
    );
    assert_eq!(new[terminal]["error"]["detail"], "new failure");
    ok(&call(&first, "session.dispose", "{}").await);
    ok(&call(&second, "session.dispose", "{}").await);
    ok(&call(&independent, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn production_scan_version_ceiling_preserves_offer_and_session_selection() {
    let (host, _) = fixture().await;
    let first = host.open_session("gated").unwrap();
    let second = host.open_session("independent").unwrap();
    assert!(first.negotiate_scan_end_version_with_max("[2]", 1).is_err());
    assert!(
        first
            .negotiate_scan_end_version_with_max("[1,\"2\"]", 1)
            .is_err()
    );
    assert_eq!(
        parse(
            &first
                .negotiate_scan_end_version_with_max("[1,2]", 1)
                .unwrap()
        )["version"],
        1
    );
    assert!(first.negotiate_scan_end_version_with_max("[2]", 2).is_err());
    assert_eq!(
        parse(
            &second
                .negotiate_scan_end_version_with_max("[1,2]", 2)
                .unwrap()
        )["version"],
        2
    );
    assert!(
        second
            .negotiate_scan_end_version_with_max("[1,2]", 1)
            .is_err()
    );
    ok(&call(&first, "session.dispose", "{}").await);
    ok(&call(&second, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn negotiation_is_per_session_and_frozen_before_first_scan() {
    let (host, _) = fixture().await;
    let old = host.open_session("v1").unwrap();
    let new = host.open_session("v2").unwrap();
    assert!(new.negotiate_scan_end_version("[]").is_err());
    assert!(new.negotiate_scan_end_version("[3]").is_err());
    assert_eq!(
        parse(&new.negotiate_scan_end_version("[1,2]").unwrap())["version"],
        2
    );
    assert!(new.negotiate_scan_end_version("[1]").is_err());
    let args = |id| json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":id}).to_string();
    ok(&call(&old, "scan.start", &args("old")).await);
    assert!(old.negotiate_scan_end_version("[2]").is_err());
    ok(&call(&new, "scan.start", &args("new")).await);
    host.ingest(RadioIngress::ScanFailed {
        detail: "shared failure".into(),
    });
    let old_records = collect(&old, "scan-end", 1).await;
    let new_records = collect(&new, "scan-end", 1).await;
    assert!(
        of_type(&old_records, "scan-end")[0]
            .get("version")
            .is_none()
    );
    let terminal = of_type(&new_records, "scan-end")[0];
    assert_eq!(terminal["version"], 2);
    assert_eq!(terminal["startOperationId"], "new");
    assert_eq!(terminal["error"]["detail"], "shared failure");
    ok(&call(&old, "session.dispose", "{}").await);
    ok(&call(&new, "session.dispose", "{}").await);
    assert!(new.negotiate_scan_end_version("[1,2]").is_err());
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn failure_while_widening_retires_old_members_without_starting_replacement() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StopScan { .. }) && tx.is_some() {
            tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-widen-failure".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let first = host.open_session("first").unwrap();
    let second = host.open_session("second").unwrap();
    first.negotiate_scan_end_version("[1,2]").unwrap();
    let start = ok(&call(
        &first,
        "scan.start",
        &json!({"serviceUuids":["180d"],"duplicatePolicy":"all","operationId":"first"}).to_string(),
    )
    .await);
    let generation = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    let widening = tokio::spawn({
        let second = second.clone();
        async move {
            call(
                &second,
                "scan.start",
                &json!({"serviceUuids":["180f"],"duplicatePolicy":"all","operationId":"second"})
                    .to_string(),
            )
            .await
        }
    });
    let stop = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "failed during stop".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    radio.answer(stop, ubm_mobile::RadioCompletion::Unit);
    assert_eq!(parse(&widening.await.unwrap())["ok"], false);
    let records = collect(&first, "scan-end", 1).await;
    let terminal = of_type(&records, "scan-end")[0];
    assert_eq!(terminal["operationId"], start["operationId"]);
    assert_eq!(terminal["generation"], generation.to_string());
    assert_eq!(terminal["error"]["detail"], "failed during stop");
    assert_eq!(
        radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, ubm_mobile::RadioRequest::StartScan { .. }))
            .count(),
        1
    );
    ok(&call(&first, "session.dispose", "{}").await);
    ok(&call(&second, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_widening_start_delivers_accepted_prefix_before_terminal() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let mut released = self.release.0.lock().unwrap();
                while !*released {
                    released = self.release.1.wait(released).unwrap();
                }
            }
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let mut starts = 0;
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) {
            starts += 1;
            if starts == 2 {
                tx.take().unwrap().send(request.id()).unwrap();
                return Reply::Hold;
            }
        }
        polar_responder(request)
    }));
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "widen-prefix".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let first = host.open_session("first").unwrap();
    let second = host.open_session("second").unwrap();
    first.negotiate_scan_end_version("[1,2]").unwrap();
    ok(&call(
        &first,
        "scan.start",
        &json!({"serviceUuids":["180d"],"duplicatePolicy":"all","operationId":"first"}).to_string(),
    )
    .await);
    let _ = first.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "hold pump".into(),
    });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    let widening = tokio::spawn({
        let second = second.clone();
        async move {
            call(
                &second,
                "scan.start",
                &json!({"serviceUuids":["180f"],"duplicatePolicy":"all","operationId":"second"})
                    .to_string(),
            )
            .await
        }
    });
    let generation = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: generation,
            advertisement: Advertisement {
                peer_id: "prefix".into(),
                service_uuids: vec![HR_SERVICE.into()],
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    radio.answer(
        generation,
        ubm_mobile::RadioCompletion::Failed(ubm_mobile::PlatformFailure::new(
            ubm_mobile::FailureKind::Platform,
            "widen failed",
        )),
    );
    assert_eq!(parse(&widening.await.unwrap())["ok"], false);
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    let records = collect(&first, "scan-end", 1).await;
    let terminal = records.iter().position(|r| r["t"] == "scan-end").unwrap();
    assert!(
        records[..terminal]
            .iter()
            .any(|r| r["t"] == "adv" && r["peerId"] == "prefix"),
        "{records:?}"
    );
    ok(&call(&first, "session.dispose", "{}").await);
    ok(&call(&second, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_widening_start_delivers_accepted_prefix_before_terminal() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let mut released = self.release.0.lock().unwrap();
                while !*released {
                    released = self.release.1.wait(released).unwrap();
                }
            }
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut tx = Some(tx);
    let mut starts = 0;
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StartScan { .. }) {
            starts += 1;
            if starts == 2 {
                tx.take().unwrap().send(request.id()).unwrap();
                return Reply::Hold;
            }
        }
        polar_responder(request)
    }));
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "widen-prefix".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let first = host.open_session("first").unwrap();
    let second = host.open_session("second").unwrap();
    first.negotiate_scan_end_version("[1,2]").unwrap();
    ok(&call(
        &first,
        "scan.start",
        &json!({"serviceUuids":["180d"],"duplicatePolicy":"all","operationId":"first"}).to_string(),
    )
    .await);
    let _ = first.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "hold pump".into(),
    });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    let widening = tokio::spawn({
        let second = second.clone();
        async move {
            call(
                &second,
                "scan.start",
                &json!({"serviceUuids":["180f"],"duplicatePolicy":"all","operationId":"second"})
                    .to_string(),
            )
            .await
        }
    });
    let generation = rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: generation,
            advertisement: Advertisement {
                peer_id: "prefix".into(),
                service_uuids: vec![HR_SERVICE.into()],
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    assert_eq!(
        ok(&call(
            &second,
            "op.cancel",
            &json!({"operationId":"second"}).to_string()
        )
        .await)["state"],
        "cancellation-requested"
    );
    radio.answer(generation, ubm_mobile::RadioCompletion::Unit);
    assert_eq!(parse(&widening.await.unwrap())["ok"], false);
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    let records = collect(&first, "scan-end", 1).await;
    let terminal = records.iter().position(|r| r["t"] == "scan-end").unwrap();
    assert!(
        records[..terminal]
            .iter()
            .any(|r| r["t"] == "adv" && r["peerId"] == "prefix"),
        "{records:?}"
    );
    ok(&call(&first, "session.dispose", "{}").await);
    ok(&call(&second, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test]
async fn native_scan_failure_preserves_android_code_and_detail() {
    let (host, radio) = fixture().await;
    let session = host.open_session("native-code").unwrap();
    session.negotiate_scan_end_version("[1,2]").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"coded"}).to_string(),
    )
    .await);
    let generation = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedAndroid {
            scan_request_id: generation,
            detail: "native failure".into(),
            native_code: 6
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    let records = collect(&session, "scan-end", 1).await;
    let terminal = of_type(&records, "scan-end")[0];
    assert_eq!(terminal["error"]["detail"], "native failure");
    assert_eq!(terminal["error"]["platform"]["domain"], "android");
    assert_eq!(terminal["error"]["platform"]["code"], "6");
    assert_eq!(
        terminal["error"]["platform"]["metadata"]["androidScanErrorCode"],
        6
    );
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

async fn captured_prefix_scenario(replace: bool, seal: bool, dispose: bool, resets: bool) {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let (lock, ready) = &self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            }
        }
    }
    let radio = Scripted::polar();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-prefix-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("prefix").unwrap();
    session.negotiate_scan_end_version("[1,2]").unwrap();
    let started = ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"start"}).to_string(),
    )
    .await);
    let generation = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    let _ = session.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    assert_eq!(
        host.ingest(RadioIngress::AdvertisementScoped {
            scan_request_id: generation,
            advertisement: Advertisement {
                peer_id: "gate".into(),
                ..Advertisement::default()
            }
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    for n in 0..40 {
        assert_eq!(
            host.ingest(RadioIngress::AdvertisementScoped {
                scan_request_id: generation,
                advertisement: Advertisement {
                    peer_id: format!("prefix-{n}"),
                    ..Advertisement::default()
                }
            }),
            ubm_mobile::IngressStatus::Accepted
        );
    }
    assert_eq!(
        host.ingest(RadioIngress::ScanFailedScoped {
            scan_request_id: generation,
            detail: "failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    ok(&call(
        &session,
        "scan.stop",
        &json!({"operationId":started["operationId"]}).to_string(),
    )
    .await);
    let replacement = if replace {
        let replacement = host.open_session("replacement").unwrap();
        ok(&call(
            &replacement,
            "scan.start",
            &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"replacement"})
                .to_string(),
        )
        .await);
        let replacement_generation = radio
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|r| match r {
                ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_ne!(replacement_generation, generation);
        assert_eq!(
            host.ingest(RadioIngress::AdvertisementScoped {
                scan_request_id: replacement_generation,
                advertisement: Advertisement {
                    peer_id: "replacement-peer".into(),
                    ..Advertisement::default()
                }
            }),
            ubm_mobile::IngressStatus::Accepted
        );
        Some(replacement)
    } else {
        None
    };
    if seal {
        let pending = ok(&call(&session, "session.quiesce", "{}").await);
        assert_eq!(
            pending["state"], "pending",
            "captured ingress still owns the cutoff"
        );
        assert_eq!(
            ok(&call(&session, "session.quiesce", "{}").await)["state"],
            "pending"
        );
        let independent = host.open_session("independent").unwrap();
        assert_eq!(
            ok(&call(&independent, "session.quiesce", "{}").await)["state"],
            "sealed"
        );
        ok(&call(&independent, "session.dispose", "{}").await);
    }
    if dispose {
        ok(&call(&session, "session.dispose", "{}").await);
    }
    if resets {
        for power in [
            ubm_mobile::AdapterPower::Off,
            ubm_mobile::AdapterPower::On,
            ubm_mobile::AdapterPower::Off,
        ] {
            let mut state = adapter_on();
            state.power = power;
            assert_eq!(
                host.ingest(RadioIngress::AdapterState(state)),
                ubm_mobile::IngressStatus::Accepted
            );
        }
    }
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    let records = collect(&session, "scan-end", 1).await;
    let terminal_index = records.iter().position(|r| r["t"] == "scan-end").unwrap();
    let prefix: Vec<_> = records[..terminal_index]
        .iter()
        .filter(|r| r["t"] == "adv")
        .collect();
    assert_eq!(
        prefix.len(),
        41,
        "all captured observations must precede the terminal: {records:?}"
    );
    for (n, record) in prefix.iter().skip(1).enumerate() {
        assert_eq!(record["peerId"], format!("prefix-{n}"));
    }
    if seal {
        let sealed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let answer = ok(&call(&session, "session.quiesce", "{}").await);
                if answer["state"] == "sealed" {
                    break answer;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(sealed["state"], "sealed");
    }
    if let Some(replacement) = replacement {
        let records = collect(&replacement, "adv", 1).await;
        assert!(
            records
                .iter()
                .any(|record| record["peerId"] == "replacement-peer")
        );
        ok(&call(&replacement, "session.dispose", "{}").await);
    }
    if !dispose {
        ok(&call(&session, "session.dispose", "{}").await);
    }
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn captured_prefix_larger_than_pump_batch_precedes_failure_after_stop() {
    captured_prefix_scenario(false, false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_membership_keeps_its_advertisement_after_old_failure_cut() {
    captured_prefix_scenario(true, false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn captured_prefix_and_failure_survive_stop_then_seal() {
    captured_prefix_scenario(false, true, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn captured_prefix_and_failure_survive_disposal_before_delivery() {
    captured_prefix_scenario(false, true, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn captured_prefix_survives_quiesce_dispose_and_repeated_adapter_reset() {
    captured_prefix_scenario(false, true, true, true).await;
}

#[tokio::test]
async fn captured_failure_is_visible_while_native_stop_is_pending() {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let mut stop_tx = Some(stop_tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StopScan { .. }) && stop_tx.is_some() {
            stop_tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-terminal-stop-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("scan").unwrap();
    let independent = host.open_session("independent-pump").unwrap();
    let _ = independent.drain(256, 65536);
    session.negotiate_scan_end_version("[1,2]").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}).to_string(),
    )
    .await);
    assert_eq!(
        host.ingest(RadioIngress::ScanFailed {
            detail: "captured failure".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    let stop = stop_rx.await.unwrap();
    let records = collect(&session, "scan-end", 1).await;
    assert_eq!(
        of_type(&records, "scan-end")[0]["error"]["detail"],
        "captured failure"
    );
    let sealed = ok(&call(&session, "session.quiesce", "{}").await);
    assert_eq!(
        sealed["state"], "sealed",
        "native cleanup remains separate from the published failure"
    );
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "independent event".into(),
    });
    let independent_records = collect(&independent, "ingress-drop", 1).await;
    assert_eq!(
        of_type(&independent_records, "ingress-drop")[0]["class"],
        "control"
    );
    radio.answer(stop, ubm_mobile::RadioCompletion::Unit);
    ok(&call(&session, "session.dispose", "{}").await);
    ok(&call(&independent, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_arriving_during_native_stop_does_not_block_pump() {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let mut stop_tx = Some(stop_tx);
    let radio = Scripted::new(Box::new(move |request| {
        if matches!(request, ubm_mobile::RadioRequest::StopScan { .. }) && stop_tx.is_some() {
            stop_tx.take().unwrap().send(request.id()).unwrap();
            Reply::Hold
        } else {
            polar_responder(request)
        }
    }));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::new(Wake),
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-stop-first-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("scan").unwrap();
    let independent = host.open_session("independent").unwrap();
    session.negotiate_scan_end_version("[1,2]").unwrap();
    let started = ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan"}).to_string(),
    )
    .await);
    let stopping = tokio::spawn({
        let session = session.clone();
        async move {
            call(
                &session,
                "scan.stop",
                &json!({"operationId":started["operationId"]}).to_string(),
            )
            .await
        }
    });
    let stop = stop_rx.await.unwrap();
    assert_eq!(
        host.ingest(RadioIngress::ScanFailed {
            detail: "during stop".into()
        }),
        ubm_mobile::IngressStatus::Accepted
    );
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "independent".into(),
    });
    let result = tokio::time::timeout(Duration::from_millis(300), async {
        let records = collect(&session, "scan-end", 1).await;
        assert_eq!(
            of_type(&records, "scan-end")[0]["error"]["detail"],
            "during stop"
        );
        let records = collect(&independent, "ingress-drop", 1).await;
        assert_eq!(of_type(&records, "ingress-drop")[0]["class"], "control");
        assert_eq!(
            ok(&call(&session, "session.quiesce", "{}").await)["state"],
            "sealed"
        );
    })
    .await;
    radio.answer(stop, ubm_mobile::RadioCompletion::Unit);
    ok(&stopping.await.unwrap());
    assert!(
        result.is_ok(),
        "failure and unrelated events must publish while native stop is held"
    );
    ok(&call(&session, "session.dispose", "{}").await);
    ok(&call(&independent, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

async fn nonradio_terminal_follows_prefix(adapter_loss: bool) {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let mut released = self.release.0.lock().unwrap();
                while !*released {
                    released = self.release.1.wait(released).unwrap();
                }
            }
        }
    }
    let radio = Scripted::polar();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-deadline-prefix-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("deadline").unwrap();
    host.ingest(RadioIngress::AdapterState(adapter_on()));
    let mut args = json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"start"});
    if !adapter_loss {
        args["lifetimeMs"] = json!(300);
    }
    ok(&call(&session, "scan.start", &args.to_string()).await);
    let generation = radio
        .requests
        .lock()
        .unwrap()
        .iter()
        .find_map(|r| match r {
            ubm_mobile::RadioRequest::StartScan { id, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    let _ = session.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    host.ingest(RadioIngress::Dropped {
        class: ubm_mobile::IngressClass::Control,
        detail: "hold pump".into(),
    });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    for n in 0..40 {
        assert_eq!(
            host.ingest(RadioIngress::AdvertisementScoped {
                scan_request_id: generation,
                advertisement: Advertisement {
                    peer_id: format!("prefix-{n}"),
                    ..Advertisement::default()
                }
            }),
            ubm_mobile::IngressStatus::Accepted
        );
    }
    if adapter_loss {
        let mut off = adapter_on();
        off.power = ubm_mobile::AdapterPower::Off;
        host.ingest(RadioIngress::AdapterState(off));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    let records = collect(&session, "scan-end", 1).await;
    let terminal = records.iter().position(|r| r["t"] == "scan-end").unwrap();
    assert_eq!(
        records[..terminal]
            .iter()
            .filter(|r| r["t"] == "adv")
            .count(),
        40,
        "all captured advertisements precede expiry: {records:?}"
    );
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_follows_entire_captured_advertisement_prefix() {
    nonradio_terminal_follows_prefix(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_loss_follows_entire_captured_advertisement_prefix() {
    nonradio_terminal_follows_prefix(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_terminal_keeps_seal_pending_until_publication_finishes() {
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Gate {
        armed: AtomicBool,
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.0.lock().unwrap() = true;
            self.0.release.1.notify_all();
        }
    }
    impl WakeSink for Gate {
        fn wake(&self, _: u64) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                let mut released = self.release.0.lock().unwrap();
                while !*released {
                    released = self.release.1.wait(released).unwrap();
                }
            }
        }
    }
    let radio = Scripted::polar();
    let gate = Arc::new(Gate {
        armed: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: (Mutex::new(false), Condvar::new()),
    });
    let _release = Release(Arc::clone(&gate));
    let host = Arc::new(
        MobileHost::open(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&gate) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "scan-deadline-seal-522".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap(),
    );
    radio.bind_host(&host);
    let session = host.open_session("deadline").unwrap();
    ok(&call(
        &session,
        "scan.start",
        &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"start","lifetimeMs":200})
            .to_string(),
    )
    .await);
    let _ = session.drain(256, 65536);
    gate.armed.store(true, Ordering::SeqCst);
    let entered = gate.entered.notified();
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap();
    assert_eq!(
        ok(&call(&session, "session.quiesce", "{}").await)["state"],
        "pending"
    );
    *gate.release.0.lock().unwrap() = true;
    gate.release.1.notify_all();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if ok(&call(&session, "session.quiesce", "{}").await)["state"] == "sealed" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let records = parse(&session.drain(256, 65536));
    assert_eq!(
        of_type(records["records"].as_array().unwrap(), "scan-end").len(),
        1
    );
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
}
