//! The mobile owner's identity is the legacy React Native backend's, never
//! the desktop host's. Legacy (origin/main
//! `src/backends/corebluetooth/corebluetooth-attachment-lifecycle.ts:62-90`,
//! used by `react-native-android-provider.ts:287-296` and
//! `react-native-apple-provider.ts:218-227`) numbered the backend and adapter
//! generations from `"1"`, named the backend instance
//! `react-native-{android|apple}-backend-{n}`, the attachment
//! `{backendInstance}:{backendGeneration}:{adapterGeneration}`, and the adapter
//! `android-default-adapter` / `apple-corebluetooth-default-adapter`.

mod common;

use common::*;
use serde_json::Value;
use ubm_desktop::{AttachmentEpoch, HostIdentity};
use ubm_mobile::{MobileIdentity, MobilePlatform};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_reports_the_legacy_generations_on_both_platforms() {
    for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
        let radio = Scripted::polar();
        let (host, _) = open(&radio, platform).await;
        let session = host.open_session("identity").expect("session");
        let state = ok(&call(&session, "adapter.state", "{}").await);
        assert_eq!(state["backendGeneration"], "1", "{platform:?}: {state}");
        assert_eq!(state["adapterGeneration"], "1", "{platform:?}: {state}");
        let reconciled = ok(&call(&session, "session.reconcile", "{}").await);
        assert_eq!(reconciled["adapter"]["backendGeneration"], "1");
        assert_eq!(reconciled["adapter"]["adapterGeneration"], "1");
        assert_no_desktop_name(&state);
        assert_no_desktop_name(&reconciled);
        ok(&call(&session, "session.dispose", "{}").await);
        let record = parse(&host.shutdown().await);
        assert_eq!(record["state"], "released", "{record}");
    }
}

fn assert_no_desktop_name(value: &Value) {
    let text = value.to_string().to_ascii_lowercase();
    assert!(
        !text.contains("desktop"),
        "a desktop name reached the mobile wire: {text}"
    );
}

fn epoch(ordinal: u64, resets: u64) -> AttachmentEpoch<'static> {
    AttachmentEpoch {
        ordinal,
        resets,
        adapter: "A0:9E:1A:FF:FF:FF",
    }
}

#[test]
fn the_mobile_identity_names_every_scope_in_the_legacy_react_native_formats() {
    for (platform, scope, adapter) in [
        (
            MobilePlatform::Android,
            "react-native-android",
            "android-default-adapter",
        ),
        (
            MobilePlatform::Apple,
            "react-native-apple",
            "apple-corebluetooth-default-adapter",
        ),
    ] {
        let identity = MobileIdentity::new(platform);
        let opened = identity.attachment(epoch(3, 0)).expect("open scope");
        let instance = format!("{scope}-backend-3");
        assert_eq!(opened.backend_instance_id().as_str(), instance);
        assert_eq!(opened.backend_generation().as_str(), "1");
        assert_eq!(opened.adapter_generation().as_str(), "1");
        assert_eq!(opened.attachment_id().as_str(), format!("{instance}:1:1"));
        assert_eq!(opened.adapter_id().as_str(), adapter);

        // Legacy `advanceGeneration` moved both generations by one and
        // rebuilt the attachment id; the instance and adapter stay.
        let reset = identity.attachment(epoch(3, 1)).expect("reset scope");
        assert_eq!(reset.backend_instance_id().as_str(), instance);
        assert_eq!(reset.backend_generation().as_str(), "2");
        assert_eq!(reset.adapter_generation().as_str(), "2");
        assert_eq!(reset.attachment_id().as_str(), format!("{instance}:2:2"));
        assert_eq!(reset.adapter_id().as_str(), adapter);

        assert_ne!(
            identity.kernel_generation(epoch(3, 0)).expect("kernel"),
            identity.kernel_generation(epoch(3, 1)).expect("kernel"),
            "every scope of one central has its own kernel generation"
        );
        for name in [
            identity.namespace(),
            identity.log_tag(),
            identity
                .kernel_generation(epoch(3, 0))
                .expect("kernel")
                .as_str(),
        ] {
            assert!(!name.contains("desktop"), "{name:?} names the desktop host");
        }
        assert_eq!(identity.namespace(), "ubm-mobile.host");
        assert_eq!(identity.log_tag(), "ubm-mobile");
    }
}

/// `instance_key` backs per-session client state (test admissions, and any
/// host keeping state per session): it must never repeat for a new session,
/// even when the allocator hands the freed address straight back. On Windows
/// the second platform iteration inherited the first one's admissions and
/// production correctly refused them as `argument.invalid` -- the harness
/// identity, not the admission check, was wrong. Hosts drop per iteration
/// exactly like the adapter-loss test, so a pointer-derived key collides
/// here on any recycling allocator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_instance_keys_are_never_reused_across_hosts() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for round in 0..32u32 {
        for platform in [MobilePlatform::Android, MobilePlatform::Apple] {
            let radio = Scripted::polar();
            let (host, _) = open(&radio, platform).await;
            let session = host.open_session("reuse").expect("session");
            let key = session.instance_key();
            assert_eq!(
                session.clone().instance_key(),
                key,
                "clones share the session identity"
            );
            assert!(
                seen.insert(key),
                "round {round} {platform:?}: instance_key {key} reused after its session died"
            );
        }
    }
}

/// The tightest form of the same contract: dispose, drop, and recreate on
/// one host. A pointer-derived key is at the mercy of the freelist here, so
/// this fails fast on recycling allocators while the cross-host test above
/// mirrors the Windows CI shape (one host per platform per round).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_instance_keys_differ_after_drop_and_recreate() {
    use std::collections::HashSet;
    let radio = Scripted::polar();
    let (host, _) = open(&radio, MobilePlatform::Android).await;
    let mut seen = HashSet::new();
    for round in 0..64u32 {
        let session = host.open_session("cycle").expect("session");
        let key = session.instance_key();
        assert!(
            seen.insert(key),
            "round {round}: instance_key {key} reused after drop and recreate"
        );
        ok(&call(&session, "session.dispose", "{}").await);
        drop(session);
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_host_owner_fails_the_open_in_the_mobile_namespace() {
    use std::sync::Arc;
    use ubm_mobile::{HostOptions, MobileHost, PlatformRadio, WakeSink};
    let error = MobileHost::open(
        Scripted::polar() as Arc<dyn PlatformRadio>,
        Arc::new(Wakes::default()) as Arc<dyn WakeSink>,
        HostOptions {
            platform: MobilePlatform::Apple,
            owner: String::new(),
            adapter_label: "scripted-adapter".to_owned(),
        },
        tokio::runtime::Handle::current(),
    )
    .await
    .err()
    .expect("an empty owner is refused");
    assert_eq!(error.code_str(), "argument.invalid");
    assert_eq!(error.operation(), "ubm-mobile.host.owner");
}
