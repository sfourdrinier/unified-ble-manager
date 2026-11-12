//! A normal manager may install the process host before a later manager opts
//! into durable terminal evidence. That later option must attach to the same
//! host; it must not depend on construction order.

mod common;

use common::{Scripted, Wakes};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::runtime::Handle;
use ubm_desktop::continuation_journal::JournalRegistry;
use ubm_mobile::{HostOptions, MobileHost, MobilePlatform, PlatformRadio, WakeSink};

fn unique_directory() -> std::path::PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ubm-mobile-terminal-evidence-late-attach-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed),
    ))
}

#[tokio::test]
async fn terminal_evidence_attaches_to_an_already_installed_host() {
    let directory = unique_directory();
    std::fs::create_dir_all(&directory).expect("create durable registry directory");
    let registry = Arc::new(JournalRegistry::default());
    registry
        .configure_directory(&directory)
        .expect("configure durable registry");
    let host = Arc::new(
        MobileHost::open_with_recording_registry(
            Scripted::polar() as Arc<dyn PlatformRadio>,
            Arc::new(Wakes::default()) as Arc<dyn WakeSink>,
            HostOptions {
                platform: MobilePlatform::Android,
                owner: "late-terminal-evidence".to_owned(),
                adapter_label: "scripted".to_owned(),
            },
            Handle::current(),
            registry,
        )
        .await
        .expect("host opens before terminal evidence is configured"),
    );

    assert!(host.terminal_evidence_recovery_selected().is_err());
    let first = Arc::clone(&host);
    let second = Arc::clone(&host);
    let (first, second) = tokio::join!(
        tokio::task::spawn_blocking(move || first.configure_terminal_evidence_maximum_bytes(65_536)),
        tokio::task::spawn_blocking(
            move || second.configure_terminal_evidence_maximum_bytes(65_536)
        ),
    );
    first
        .expect("first configuration task runs")
        .expect("first late configuration attaches");
    second
        .expect("second configuration task runs")
        .expect("same-bound late configuration is idempotent");
    host.terminal_evidence_recovery_selected()
        .expect("the attached ledger is recoverable through the same host");
    assert!(
        host.configure_terminal_evidence_maximum_bytes(65_537)
            .is_err()
    );
    host.shutdown().await;
    assert!(
        host.configure_terminal_evidence_maximum_bytes(65_536)
            .is_err()
    );
}
