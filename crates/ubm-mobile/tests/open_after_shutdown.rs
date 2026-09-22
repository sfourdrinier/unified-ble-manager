//! A mobile open refused by the shared central carries the mobile owner's
//! namespace, never the desktop host's. Its own binary: shutting the shared
//! executor down is process-wide.

mod common;

use std::sync::Arc;

use common::*;
use ubm_mobile::{HostOptions, MobileHost, MobilePlatform, PlatformRadio, WakeSink};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_after_executor_shutdown_fails_in_the_mobile_namespace() {
    ubm_desktop::executor::shutdown_desktop_runtime();
    let radio = Scripted::polar();
    let wakes = Arc::new(Wakes::default());
    let error = MobileHost::open(
        radio as Arc<dyn PlatformRadio>,
        wakes as Arc<dyn WakeSink>,
        HostOptions {
            platform: MobilePlatform::Android,
            owner: "ubm-mobile-test".to_owned(),
            adapter_label: "scripted-adapter".to_owned(),
        },
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("a shut-down executor admits no central");
    assert_eq!(error.code_str(), "adapter.unavailable");
    assert_eq!(error.operation(), "ubm-mobile.host.open");
}
