//! Durable subscription-terminal evidence is committed before JS publication.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use serde_json::{Value, json};
use ubm_desktop::continuation_journal::JournalRegistry;
use ubm_mobile::{
    HostOptions, Instance, MobileHost, MobilePlatform, PlatformFailure, PlatformRadio,
    RadioIngress, RequestKind, WakeSink,
};

fn unique_directory() -> std::path::PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ubm-mobile-subscription-terminal-evidence-{serial}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos(),
    ))
}

async fn records(session: &ubm_mobile::MobileSession) -> Vec<Value> {
    parse(&session.drain(256, 65_536))["records"]
        .as_array()
        .expect("wire records array")
        .clone()
}

async fn connect(session: &ubm_mobile::MobileSession, operation_id: &str) {
    ok(&call(
        session,
        "connection.connect",
        &json!({
            "peerId": POLAR, "lease": "lease-1", "operationId": operation_id,
        })
        .to_string(),
    )
    .await);
}

fn enable_epoch(radio: &Scripted) -> u64 {
    radio
        .requests
        .lock()
        .expect("scripted requests lock")
        .iter()
        .rev()
        .find_map(|request| match request {
            ubm_mobile::RadioRequest::EnableNotifications { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .expect("subscription enabled notifications")
}

async fn open_durable(directory: &std::path::Path) -> (Arc<MobileHost>, Arc<Scripted>) {
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(directory)
        .expect("configure private registry directory");
    open_durable_registry(registry).await
}

async fn open_durable_registry(registry: Arc<JournalRegistry>) -> (Arc<MobileHost>, Arc<Scripted>) {
    let radio = Scripted::polar();
    let wake = Arc::new(Wakes::default());
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Arc::clone(&radio) as Arc<dyn PlatformRadio>,
            Arc::clone(&wake) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "subscription-terminal-evidence".into(),
                adapter_label: "scripted".into(),
            },
            tokio::runtime::Handle::current(),
            registry,
        )
        .await
        .expect("open host with durable ledger"),
    );
    radio.bind_host(&host);
    (host, radio)
}

async fn subscribe_with_scope(session: &ubm_mobile::MobileSession, scope: Value) {
    connect(session, "subscription-connect").await;
    ok(&call(
        session,
        "gatt.discover",
        &json!({"peerId":POLAR,"lease":"lease-1","operationId":"subscription-discover"})
            .to_string(),
    )
    .await);
    ok(&call(
        session,
        "gatt.subscribe",
        &json!({
            "peerId":POLAR, "selector":selector(), "consumer":"hr",
            "deliveryMode":"require-notification", "operationId":"subscription",
            "terminalEvidenceScope":scope,
        })
        .to_string(),
    )
    .await);
}

fn source_failure(epoch: u64) -> RadioIngress {
    RadioIngress::NotificationSourceFailed {
        instance: Instance {
            peer_id: POLAR.to_owned(),
            service_uuid: HR_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: HR_MEASUREMENT.to_owned(),
            characteristic_occurrence: 0,
        },
        epoch,
        failure: PlatformFailure::new(
            ubm_mobile::FailureKind::GattStatus,
            "synthetic subscription terminal",
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_scope_is_checked_before_cccd_activation_and_failure_debt_reconciles_before_drain()
 {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let (host, radio) = open_durable(&directory).await;
    let session = host.open_session("manager").expect("open session");

    connect(&session, "forged-connect").await;
    ok(&call(
        &session,
        "gatt.discover",
        &json!({"peerId":POLAR,"lease":"lease-1","operationId":"forged-discover"}).to_string(),
    )
    .await);
    let (error, _) = failure(&call(&session, "gatt.subscribe", &json!({
        "peerId":POLAR,"selector":selector(),"consumer":"forged","deliveryMode":"require-notification","operationId":"forged-subscribe",
        "terminalEvidenceScope":{"managerId":"manager","operationId":"forged","incarnation":"1"}
    }).to_string()).await);
    assert_eq!(error["code"], "lifecycle.invalid-state");
    assert_eq!(
        radio.count(RequestKind::EnableNotifications),
        0,
        "forged scope must not activate CCCD"
    );

    let scope =
        json!({"managerId":"manager","operationId":"subscription-terminal","incarnation":"1"});
    ok(&call(
        &session,
        "terminal-evidence.reserve",
        &json!({
            "producer":"subscription","maximumBytes":65_536,"formatVersion":1,"scope":scope
        })
        .to_string(),
    )
    .await);
    subscribe_with_scope(&session, scope).await;
    let epoch = enable_epoch(&radio);
    let _ = records(&session).await;

    host.fail_next_subscription_terminal_evidence_commit_for_test();
    host.ingest(source_failure(epoch));
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.pending_subscription_terminal_evidence_debts_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed durable commit retained as retry debt");
    assert!(
        records(&session)
            .await
            .iter()
            .all(|record| record["t"] != "stream-end"),
        "terminal must not enter outbox before its evidence commit"
    );

    ok(&call(&session, "session.reconcile", "{}").await);
    assert_eq!(
        host.pending_subscription_terminal_evidence_debts_for_test(),
        0
    );
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(record) = records(&session)
                .await
                .into_iter()
                .find(|record| record["t"] == "stream-end")
            {
                return record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconcile commits and publishes exact subscription terminal");
    assert_eq!(terminal["evidence"]["producer"], "subscription");
    assert_eq!(terminal["evidence"]["durability"], "committed");

    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private registry directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_subscription_terminal_survives_restart_for_explicit_native_recovery() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure private registry directory");
    let (host, radio) = open_durable_registry(Arc::clone(&registry)).await;
    let session = host.open_session("manager").expect("open session");
    let scope =
        json!({"managerId":"manager","operationId":"subscription-restart","incarnation":"1"});
    ok(&call(
        &session,
        "terminal-evidence.reserve",
        &json!({"producer":"subscription","maximumBytes":65_536,"formatVersion":1,"scope":scope})
            .to_string(),
    )
    .await);
    subscribe_with_scope(&session, scope).await;
    host.fail_next_subscription_terminal_evidence_commit_for_test();
    host.ingest(source_failure(enable_epoch(&radio)));
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.pending_subscription_terminal_evidence_debts_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("staged subscription terminal retained before restart");
    drop(session);
    drop(host);

    let (reopened, _radio) = open_durable_registry(registry).await;
    let recovery = reopened
        .terminal_evidence_recovery_selected()
        .expect("native recovery controller");
    let page = recovery
        .list(8, 65_536)
        .expect("list staged subscription terminal");
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].producer, "subscription");
    assert_eq!(
        page.entries[0].state,
        ubm_mobile::TerminalEvidenceRecoveryState::Staged
    );
    assert!(
        !recovery
            .read(&page.entries[0], 0, 65_536)
            .expect("read exact staged terminal")
            .bytes
            .is_empty()
    );
    recovery
        .acknowledge(&page.entries[0])
        .expect("explicitly acknowledge recovered terminal");
    assert_eq!(parse(&reopened.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private registry directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_commit_debt_does_not_block_independent_scan_terminal_evidence() {
    let _ = ubm_mobile::continuation::configure_terminal_evidence_maximum_bytes(65_536);
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create private registry directory");
    let (host, radio) = open_durable(&directory).await;
    let session = host.open_session("manager").expect("open session");
    let subscription_scope =
        json!({"managerId":"manager","operationId":"subscription-isolation","incarnation":"1"});
    let scan_scope =
        json!({"managerId":"manager","operationId":"scan-isolation","incarnation":"1"});
    for (producer, scope) in [("subscription", &subscription_scope), ("scan", &scan_scope)] {
        ok(&call(
            &session,
            "terminal-evidence.reserve",
            &json!({"producer":producer,"maximumBytes":32_768,"formatVersion":1,"scope":scope})
                .to_string(),
        )
        .await);
    }
    subscribe_with_scope(&session, subscription_scope).await;
    ok(&call(&session, "scan.start", &json!({"serviceUuids":[],"duplicatePolicy":"all","operationId":"scan-isolation","terminalEvidenceScope":scan_scope}).to_string()).await);
    let _ = records(&session).await;
    host.fail_next_subscription_terminal_evidence_commit_for_test();
    host.ingest(source_failure(enable_epoch(&radio)));
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.pending_subscription_terminal_evidence_debts_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("subscription debt retained");
    host.ingest(RadioIngress::ScanFailed {
        detail: "independent scan terminal".into(),
    });
    let scan_terminal = tokio::time::timeout(Duration::from_secs(1), async {
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
    .expect("scan terminal commits despite subscription debt");
    assert_eq!(scan_terminal["evidence"]["producer"], "scan");
    assert_eq!(
        host.pending_subscription_terminal_evidence_debts_for_test(),
        1
    );
    ok(&call(&session, "session.reconcile", "{}").await);
    assert_eq!(
        host.pending_subscription_terminal_evidence_debts_for_test(),
        0
    );
    ok(&call(&session, "session.dispose", "{}").await);
    assert_eq!(parse(&host.shutdown().await)["state"], "released");
    std::fs::remove_dir_all(&directory).expect("remove private registry directory");
}
