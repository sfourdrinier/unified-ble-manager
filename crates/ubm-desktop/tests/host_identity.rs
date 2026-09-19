//! Host identity: the central names no host. The attachment tuple and kernel
//! generation of every scope — at open and after each adapter reset — and
//! the operation namespace of the central's own open failures are the
//! owning host's ([`HostIdentity`]); the desktop hosts keep the names they
//! always had through [`DesktopIdentity`].

use std::sync::Arc;
use std::time::Duration;

use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, CoreError, Generation,
};
use ubm_desktop::{
    AdapterPowerState, AdmissionPolicy, AttachmentEpoch, CentralProfile, DesktopCentral,
    DesktopIdentity, FakeRadio, HostIdentity, RadioEvent,
};

fn tuple(epoch: AttachmentEpoch<'_>) -> AttachmentTuple {
    DesktopIdentity::new("btleplug", "node")
        .attachment(epoch)
        .expect("desktop attachment")
}

#[test]
fn the_desktop_identity_names_every_scope_as_the_desktop_hosts_always_did() {
    let opened = tuple(AttachmentEpoch {
        ordinal: 7,
        resets: 0,
        adapter: "hci0",
    });
    assert_eq!(opened.attachment_id().as_str(), "desktop-attachment-7");
    assert_eq!(
        opened.backend_instance_id().as_str(),
        "ubm-desktop-btleplug-node"
    );
    assert_eq!(
        opened.backend_generation().as_str(),
        "desktop-backend-gen-7"
    );
    assert_eq!(opened.adapter_id().as_str(), "hci0");
    assert_eq!(
        opened.adapter_generation().as_str(),
        "desktop-adapter-gen-7"
    );

    let reset = tuple(AttachmentEpoch {
        ordinal: 7,
        resets: 2,
        adapter: "hci0",
    });
    assert_eq!(reset.attachment_id().as_str(), "desktop-attachment-7-r2");
    assert_eq!(
        reset.backend_instance_id().as_str(),
        "ubm-desktop-btleplug-node"
    );
    assert_eq!(
        reset.backend_generation().as_str(),
        "desktop-backend-gen-7-r2"
    );
    assert_eq!(
        reset.adapter_generation().as_str(),
        "desktop-adapter-gen-7-r2"
    );

    let identity = DesktopIdentity::new("btleplug", "node");
    let kernel = |resets| {
        identity
            .kernel_generation(AttachmentEpoch {
                ordinal: 7,
                resets,
                adapter: "hci0",
            })
            .expect("kernel generation")
    };
    assert_eq!(kernel(0).as_str(), "desktop-kernel-gen-7");
    assert_eq!(kernel(2).as_str(), "desktop-kernel-gen-7-r2");
    assert_eq!(identity.namespace(), "desktop");
    assert_eq!(identity.log_tag(), "ubm-desktop");
}

#[test]
fn the_desktop_profile_carries_the_desktop_identity() {
    let profile = CentralProfile::desktop("node");
    let named = profile
        .identity
        .attachment(AttachmentEpoch {
            ordinal: 3,
            resets: 0,
            adapter: "hci0",
        })
        .expect("attachment");
    assert_eq!(
        named.backend_instance_id().as_str(),
        "ubm-desktop-btleplug-node"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_desktop_owner_or_backend_label_fails_the_open_as_before() {
    let mut profile = CentralProfile::desktop("");
    let error = DesktopCentral::open_with(FakeRadio::new(), profile.clone())
        .await
        .err()
        .expect("an empty owner fails the open");
    assert_eq!(error.code_str(), "argument.invalid");
    assert_eq!(error.operation(), "desktop.owner");

    profile.identity = Arc::new(DesktopIdentity::new("", "node"));
    let error = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .err()
        .expect("an empty backend label fails the open");
    assert_eq!(error.code_str(), "argument.invalid");
    assert_eq!(error.operation(), "desktop.backend-label");
}

/// A host that is not the desktop: every name it mints carries its own
/// prefix and none mentions the desktop.
#[derive(Debug)]
struct ProbeHost;

impl HostIdentity for ProbeHost {
    fn namespace(&self) -> &str {
        "probe-host"
    }

    fn log_tag(&self) -> &str {
        "probe-host"
    }

    fn attachment(&self, epoch: AttachmentEpoch<'_>) -> Result<AttachmentTuple, CoreError> {
        let generation = epoch.resets + 1;
        Ok(AttachmentTuple::new(
            AttachmentId::new(format!("probe-{}:{generation}", epoch.ordinal))?,
            BackendInstanceId::new(format!("probe-{}", epoch.ordinal))?,
            BackendGeneration::new(generation.to_string())?,
            AdapterId::new("probe-adapter")?,
            AdapterGeneration::new(generation.to_string())?,
        ))
    }

    fn kernel_generation(&self, epoch: AttachmentEpoch<'_>) -> Result<Generation, CoreError> {
        Generation::new(format!("probe-kernel-{}", epoch.resets + 1))
    }
}

fn probe_profile() -> CentralProfile {
    let mut profile = CentralProfile::desktop("unused");
    profile.identity = Arc::new(ProbeHost);
    profile
}

fn assert_no_desktop_name(attachment: &AttachmentTuple) {
    for name in [
        attachment.attachment_id().as_str(),
        attachment.backend_instance_id().as_str(),
        attachment.backend_generation().as_str(),
        attachment.adapter_id().as_str(),
        attachment.adapter_generation().as_str(),
    ] {
        assert!(!name.contains("desktop"), "{name:?} names the desktop host");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_central_takes_its_attachment_from_the_owning_host_at_open_and_after_a_reset() {
    let radio = FakeRadio::new();
    radio.set_os_policy(AdmissionPolicy::LifecycleOnly, true);
    let central = DesktopCentral::open_with(radio, probe_profile())
        .await
        .expect("open");
    let opened = central.attachment();
    assert_no_desktop_name(&opened);
    assert_eq!(opened.backend_generation().as_str(), "1");
    assert_eq!(opened.adapter_generation().as_str(), "1");
    assert_eq!(opened.adapter_id().as_str(), "probe-adapter");
    let ordinal = opened
        .backend_instance_id()
        .as_str()
        .strip_prefix("probe-")
        .expect("the host's instance name")
        .to_owned();
    assert_eq!(
        opened.attachment_id().as_str(),
        format!("probe-{ordinal}:1")
    );

    let mut resets = central.adapter_reset_events();
    central
        .boundary()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
    let reset = tokio::time::timeout(Duration::from_secs(5), resets.recv())
        .await
        .expect("a reset is published")
        .expect("reset event");
    assert_eq!(reset.previous, opened);
    let current = central.attachment();
    assert_eq!(reset.current, current);
    assert_no_desktop_name(&current);
    assert_eq!(current.backend_generation().as_str(), "2");
    assert_eq!(current.adapter_generation().as_str(), "2");
    assert_eq!(
        current.attachment_id().as_str(),
        format!("probe-{ordinal}:2")
    );
    assert_eq!(current.backend_instance_id(), opened.backend_instance_id());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_identity_that_cannot_name_the_scope_fails_the_open() {
    #[derive(Debug)]
    struct Nameless;
    impl HostIdentity for Nameless {
        fn namespace(&self) -> &str {
            "nameless"
        }
        fn log_tag(&self) -> &str {
            "nameless"
        }
        fn attachment(&self, _: AttachmentEpoch<'_>) -> Result<AttachmentTuple, CoreError> {
            Ok(AttachmentTuple::new(
                AttachmentId::new("")?,
                BackendInstanceId::new("nameless")?,
                BackendGeneration::new("1")?,
                AdapterId::new("nameless")?,
                AdapterGeneration::new("1")?,
            ))
        }
        fn kernel_generation(&self, _: AttachmentEpoch<'_>) -> Result<Generation, CoreError> {
            Generation::new("1")
        }
    }
    let mut profile = CentralProfile::desktop("unused");
    profile.identity = Arc::new(Nameless);
    let error = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .err()
        .expect("an empty host name is refused, never replaced");
    assert_eq!(error.code_str(), "argument.invalid");
}
