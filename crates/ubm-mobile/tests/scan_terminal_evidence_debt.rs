//! Durable scan-terminal evidence is never acknowledged before commit.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use serde_json::{Value, json};
use ubm_desktop::continuation_journal::JournalRegistry;
use ubm_mobile::{
    HostOptions, MobileHost, MobilePlatform, PlatformRadio, RadioIngress, RequestKind, WakeSink,
};

fn unique_directory() -> std::path::PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ubm-mobile-terminal-evidence-debt-{}-{serial}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos()
    ))
}

async fn records(session: &ubm_mobile::MobileSession) -> Vec<Value> {
    parse(&session.drain(256, 65_536))["records"]
        .as_array()
        .expect("wire records array")
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_terminal_commit_failure_withholds_outbox_ack_until_reconcile_retries() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure private registry directory");

    let radio = Scripted::polar();
    let wake = Arc::new(Wakes::default());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-debt".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            registry,
        )
        .await
        .expect("open host with durable ledger"),
    );
    radio.bind_host(&host);
    let session = host.open_session("manager").expect("open session");

    ok(&call(
        &session,
        "terminal-evidence.reserve",
        &json!({
            "producer": "scan",
            "maximumBytes": 65_536,
            "formatVersion": 1,
            "scope": {
                "managerId": "manager",
                "operationId": "terminal",
                "incarnation": "1"
            }
        })
        .to_string(),
    )
    .await);
    ok(&call(
        &session,
        "scan.start",
        &json!({
            "serviceUuids": [],
            "duplicatePolicy": "all",
            "operationId": "scan",
            "terminalEvidenceScope": {
                "managerId": "manager",
                "operationId": "terminal",
                "incarnation": "1"
            }
        })
        .to_string(),
    )
    .await);
    let _ = records(&session).await;

    host.fail_next_scan_terminal_evidence_commit_for_test();
    host.ingest(RadioIngress::ScanFailed {
        detail: "test durable terminal".to_owned(),
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.pending_scan_terminal_evidence_debts_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed durable commit retained as retry debt");

    assert!(
        records(&session)
            .await
            .iter()
            .all(|record| record["t"] != "scan-end"),
        "a terminal without a durable evidence receipt must not enter the outbox"
    );

    ok(&call(&session, "session.reconcile", "{}").await);
    assert_eq!(host.pending_scan_terminal_evidence_debts_for_test(), 0);
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let drained = records(&session).await;
            if let Some(record) = drained.into_iter().find(|record| record["t"] == "scan-end") {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconcile retries the durable commit and publishes its terminal");
    assert_eq!(terminal["evidence"]["durability"], "committed");

    ok(&tokio::time::timeout(
        Duration::from_secs(2),
        call(&session, "session.dispose", "{}"),
    )
    .await
    .expect("dispose settles while debt is retained"));
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private test registry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_terminal_debt_survives_dispose_until_a_later_reconcile_commits_it() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure private registry directory");
    let radio = Scripted::polar();
    let wake = Arc::new(Wakes::default());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-dispose".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            registry,
        )
        .await
        .expect("open host with durable ledger"),
    );
    radio.bind_host(&host);
    let session = host.open_session("manager").expect("open session");

    ok(&call(
        &session,
        "terminal-evidence.reserve",
        &json!({
            "producer":"scan", "maximumBytes":65_536, "formatVersion":1,
            "scope":{"managerId":"manager", "operationId":"terminal-dispose", "incarnation":"1"}
        })
        .to_string(),
    )
    .await);
    ok(&call(&session, "scan.start", &json!({
        "serviceUuids":[], "duplicatePolicy":"all", "operationId":"scan",
        "terminalEvidenceScope":{"managerId":"manager", "operationId":"terminal-dispose", "incarnation":"1"}
    }).to_string()).await);
    let _ = records(&session).await;
    host.fail_next_scan_terminal_evidence_commit_for_test();
    host.ingest(RadioIngress::ScanFailed {
        detail: "retain through dispose".to_owned(),
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.pending_scan_terminal_evidence_debts_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed durable commit retained as retry debt");

    ok(&tokio::time::timeout(
        Duration::from_secs(2),
        call(&session, "session.dispose", "{}"),
    )
    .await
    .expect("dispose settles while debt is retained"));
    let replacement = host
        .open_session("replacement")
        .expect("replacement session");
    ok(&call(&replacement, "session.reconcile", "{}").await);
    assert_eq!(
        host.pending_scan_terminal_evidence_debts_for_test(),
        0,
        "later reconcile must commit retained debt"
    );
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(record) = records(&session)
                .await
                .into_iter()
                .find(|record| record["t"] == "scan-end")
            {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disposed owner retains terminal until durable commit");
    assert_eq!(terminal["evidence"]["durability"], "committed");

    ok(&call(&replacement, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private test registry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_start_rejects_an_unreserved_terminal_evidence_scope_before_radio_activation() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure private registry directory");
    let radio = Scripted::polar();
    let wake = Arc::new(Wakes::default());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-forged".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            registry,
        )
        .await
        .expect("open host with durable ledger"),
    );
    radio.bind_host(&host);
    let session = host.open_session("manager").expect("open session");

    let (error, _) = failure(&call(&session, "scan.start", &json!({
        "serviceUuids":[], "duplicatePolicy":"all", "operationId":"scan",
        "terminalEvidenceScope":{"managerId":"manager", "operationId":"forged", "incarnation":"1"}
    }).to_string()).await);
    assert_eq!(error["code"], "lifecycle.invalid-state");
    assert_eq!(
        radio.count(RequestKind::StartScan),
        0,
        "a forged scope must not activate the radio"
    );

    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private test registry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_scan_terminal_reopens_through_explicit_recovery_controller() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure private registry directory");
    let radio = Scripted::polar();
    let wake = Arc::new(Wakes::default());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-restart".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            Arc::clone(&registry),
        )
        .await
        .expect("open first host"),
    );
    radio.bind_host(&host);
    let session = host.open_session("manager").expect("open session");
    ok(&call(
        &session,
        "terminal-evidence.reserve",
        &json!({
            "producer":"scan", "maximumBytes":65_536, "formatVersion":1,
            "scope":{"managerId":"manager", "operationId":"terminal-restart", "incarnation":"1"}
        })
        .to_string(),
    )
    .await);
    ok(&call(&session, "scan.start", &json!({
        "serviceUuids":[], "duplicatePolicy":"all", "operationId":"scan",
        "terminalEvidenceScope":{"managerId":"manager", "operationId":"terminal-restart", "incarnation":"1"}
    }).to_string()).await);
    let _ = records(&session).await;
    host.fail_next_scan_terminal_evidence_commit_for_test();
    host.ingest(RadioIngress::ScanFailed {
        detail: "stage exact terminal".to_owned(),
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.pending_scan_terminal_evidence_debts_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed commit leaves staged terminal");
    drop(session);
    drop(host);

    let reopened_radio = Scripted::polar();
    let reopened_wake = Arc::new(Wakes::default());
    let reopened = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&reopened_radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&reopened_wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-restart".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            registry,
        )
        .await
        .expect("reopen host"),
    );
    reopened_radio.bind_host(&reopened);
    let recovery = reopened
        .terminal_evidence_recovery_selected()
        .expect("selected recovery");
    let page = recovery.list(8, 65_536).expect("list staged terminal");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].state,
        ubm_mobile::TerminalEvidenceRecoveryState::Staged
    );
    assert!(
        !recovery
            .read(&page.entries[0], 0, 65_536)
            .expect("read staged bytes")
            .bytes
            .is_empty()
    );
    recovery
        .acknowledge(&page.entries[0])
        .expect("acknowledging a fully read staged row promotes and retires it");
    assert_eq!(
        recovery
            .list(8, 65_536)
            .expect("re-list staged terminal")
            .entries
            .len(),
        0,
        "the explicit acknowledgement retires the promoted staged evidence"
    );
    assert_eq!(parse(&reopened.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private test registry");
}

// A terminal remains committed after stream release until explicit recovery
// acknowledgement, so a process death after native outbox publication replays it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_unreleased_terminal_remains_recoverable_in_the_ledger() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure private registry directory");
    let radio = Scripted::polar();
    let wake = Arc::new(Wakes::default());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-ack".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            Arc::clone(&registry),
        )
        .await
        .expect("open host"),
    );
    radio.bind_host(&host);
    let session = host.open_session("manager-before-crash").expect("session");
    let reservation = ok(&call(&session, "terminal-evidence.reserve", &json!({"producer":"scan","maximumBytes":65_536,"formatVersion":1,"scope":{"managerId":"old-manager","operationId":"terminal","incarnation":"1"}}).to_string()).await);
    ok(&call(&session, "scan.start", &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan","terminalEvidenceScope":{"managerId":"old-manager","operationId":"terminal","incarnation":"1"}}).to_string()).await);
    let _ = records(&session).await;
    host.ingest(RadioIngress::ScanFailed {
        detail: "published then crash".into(),
    });
    let published = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(record) = records(&session)
                .await
                .into_iter()
                .find(|record| record["t"] == "scan-end")
            {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("published terminal");
    let released = ok(&call(
        &session,
        "terminal-evidence.release",
        &json!({"reservationId": reservation["reservationId"]}).to_string(),
    )
    .await);
    assert_eq!(
        released,
        json!({"released":true,"retainedForRecovery":true}),
        "stream release abandons only its runtime reservation; explicit recovery ACK owns durable retirement"
    );
    drop(session);
    drop(host);
    let reopened_radio = Scripted::polar();
    let reopened_wake = Arc::new(Wakes::default());
    let reopened = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&reopened_radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&reopened_wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "terminal-evidence-ack".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            registry,
        )
        .await
        .expect("reopen host"),
    );
    reopened_radio.bind_host(&reopened);
    let recovery = reopened
        .terminal_evidence_recovery_selected()
        .expect("selected native recovery");
    let page = recovery.list(8, 65_536).expect("list committed terminal");
    assert!(
        !page.more,
        "the only retained terminal fits the requested page"
    );
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].state,
        ubm_mobile::TerminalEvidenceRecoveryState::Committed
    );
    let first = recovery
        .read(&page.entries[0], 0, 8)
        .expect("bounded native read");
    assert!(!first.bytes.is_empty());
    assert_eq!(
        page.entries[0].reference.as_deref(),
        published["evidence"]["ref"].as_str()
    );
    recovery
        .acknowledge(&page.entries[0])
        .expect("acknowledge opaque entry");
    recovery
        .acknowledge(&page.entries[0])
        .expect("lost acknowledgement reply is retryable");
    assert!(recovery.list(8, 65_536).unwrap().entries.is_empty());
    let _ = reopened.shutdown().await;
    std::fs::remove_dir_all(&directory).expect("remove private registry directory");
}
