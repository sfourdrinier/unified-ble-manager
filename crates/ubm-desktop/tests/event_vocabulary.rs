//! One name per physical event (owner decision, 5.0): each desktop OS's own
//! answer for an event, driven through the real central over the scripted
//! radio, reports exactly the error code and retryability the shared table
//! names (`src/backend-contract/event-vocabulary.ts`, copied to
//! `tests/fixtures/event-vocabulary.json` and pinned by
//! `__tests__/event-vocabulary.test.js`).

use std::time::Duration;

use serde_json::Value;
use ubm_desktop::{
    AdapterPowerState, AdmissionPolicy, CharacteristicSnapshot, DesktopCentral, FakeRadio, FaultOp,
    OpControl, PeerSnapshot, PlatformDetail, PlatformValue, PropertyFlags, RadioEvent,
    ServiceSnapshot,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

#[derive(Clone, Copy, Debug)]
enum Os {
    Mac,
    Windows,
    Linux,
}

impl Os {
    const ALL: [Self; 3] = [Self::Mac, Self::Windows, Self::Linux];

    fn backend(self) -> &'static str {
        match self {
            Self::Mac => "desktop-macos",
            Self::Windows => "desktop-windows",
            Self::Linux => "desktop-linux",
        }
    }

    /// The platform's answer when the link is gone under an operation.
    fn link_gone(self) -> PlatformDetail {
        match self {
            Self::Mac => corebluetooth("CBErrorDomain", "7"),
            Self::Windows => winrt("unreachable"),
            Self::Linux => PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_message("Not connected"),
        }
    }

    /// The platform's answer when a connect could not establish the link.
    fn not_established(self) -> PlatformDetail {
        match self {
            Self::Mac => corebluetooth("CBErrorDomain", "10"),
            Self::Windows => winrt("unreachable"),
            Self::Linux => PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_message("le-connection-abort-by-local"),
        }
    }

    /// The platform's answer when the peer refuses for lack of security.
    fn security(self) -> PlatformDetail {
        match self {
            Self::Mac => corebluetooth("CBATTErrorDomain", "5"),
            // WinRT carries no ATT error through the radio.
            Self::Windows => winrt("protocol-error"),
            Self::Linux => PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotAuthorized"),
        }
    }
}

fn corebluetooth(domain: &str, code: &str) -> PlatformDetail {
    PlatformDetail::new("corebluetooth", code)
        .with_metadata("nsErrorDomain", PlatformValue::Text(domain.to_owned()))
}

fn winrt(status: &str) -> PlatformDetail {
    PlatformDetail::new("winrt", "gatt-status")
        .with_metadata("gattStatus", PlatformValue::Text(status.to_owned()))
}

fn vocabulary() -> Value {
    serde_json::from_str(include_str!("fixtures/event-vocabulary.json")).expect("fixture")
}

/// `(error, retryability)` the table names for `event` on `os`.
fn expected(event: &str, os: Os) -> (String, String) {
    let row = &vocabulary()[event][os.backend()];
    (
        row["error"].as_str().expect("error").to_owned(),
        row["retryability"]
            .as_str()
            .expect("retryability")
            .to_owned(),
    )
}

fn observed(error: &ubm_desktop::DesktopError) -> (String, String) {
    (
        error.code_str().to_owned(),
        error.retryability().as_str().to_owned(),
    )
}

fn selector() -> ubm_desktop::PathSelector {
    DesktopCentral::<FakeRadio>::selector(
        HRM_SERVICE,
        Some(0),
        Some(HRM_MEASUREMENT),
        Some(0),
        None,
        None,
    )
    .expect("selector")
}

fn hrm_service() -> ServiceSnapshot {
    ServiceSnapshot {
        uuid: HRM_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![CharacteristicSnapshot {
            uuid: HRM_MEASUREMENT.to_owned(),
            occurrence: 0,
            properties: PropertyFlags {
                read: true,
                write: true,
                write_without_response: false,
                notify: true,
                indicate: false,
            },
            descriptors: Vec::new(),
        }],
    }
}

async fn known(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: peer_id.to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: ubm_desktop::AdvertisementExtras::default(),
        }));
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("peer {peer_id} never resolved");
}

/// A radio scripted as `os`'s radio: its admission gate, teardown on an
/// adapter loss.
fn os_radio(os: Os) -> FakeRadio {
    let radio = FakeRadio::new();
    radio.set_os_policy(
        match os {
            Os::Mac => AdmissionPolicy::CoreBluetooth,
            Os::Windows => AdmissionPolicy::WinRt,
            Os::Linux => AdmissionPolicy::LifecycleOnly,
        },
        true,
    );
    radio
}

async fn connected(peer_id: &str, os: Os) -> DesktopCentral<FakeRadio> {
    let central = DesktopCentral::open(os_radio(os), "vocabulary-host")
        .await
        .expect("open");
    known(&central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![hrm_service()]);
    central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    central
}

/// A read left waiting on the radio, ended by `end`.
async fn pending_read_ended_by(
    central: &DesktopCentral<FakeRadio>,
    peer_id: &str,
    budget_ms: u64,
    end: impl AsyncFnOnce(&DesktopCentral<FakeRadio>),
) -> ubm_desktop::DesktopError {
    central.boundary().block_op(FaultOp::Read);
    let reader = central.clone();
    let peer = peer_id.to_owned();
    let pending = tokio::spawn(async move {
        reader
            .read(&peer, &selector(), OpControl::budget_ms(budget_ms))
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    end(central).await;
    let error = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("the read ends")
        .expect("task")
        .expect_err("the read fails");
    central.boundary().unblock_op(FaultOp::Read);
    error
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_desktop_os_names_each_event_as_the_table_does() {
    for os in Os::ALL {
        // link-lost-during-operation: the OS answers the operation...
        let central = connected("peer-a", os).await;
        central
            .boundary()
            .fail_next_with_platform(FaultOp::Read, "gone", os.link_gone());
        let error = central
            .read("peer-a", &selector(), OpControl::budget_ms(1000))
            .await
            .expect_err("lost");
        assert_eq!(
            observed(&error),
            expected("link-lost-during-operation", os),
            "{os:?} answered"
        );
        // ...or never answers it, and the OS reports the link gone.
        let central = connected("peer-b", os).await;
        let error = pending_read_ended_by(&central, "peer-b", 60_000, async |central| {
            central
                .boundary()
                .push_event(RadioEvent::Lost("peer-b".to_owned()));
        })
        .await;
        assert_eq!(
            observed(&error),
            expected("link-lost-during-operation", os),
            "{os:?} unanswered"
        );

        // requested-disconnect-during-operation
        let central = connected("peer-c", os).await;
        let error = pending_read_ended_by(&central, "peer-c", 60_000, async |central| {
            central
                .disconnect("peer-c", "lease-a", OpControl::budget_ms(5000))
                .await
                .expect("release");
        })
        .await;
        assert_eq!(
            observed(&error),
            expected("requested-disconnect-during-operation", os),
            "{os:?}"
        );

        // adapter-loss-during-operation
        let central = connected("peer-d", os).await;
        let error = pending_read_ended_by(&central, "peer-d", 60_000, async |central| {
            central
                .boundary()
                .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
        })
        .await;
        assert_eq!(
            observed(&error),
            expected("adapter-loss-during-operation", os),
            "{os:?}"
        );

        // connect-not-established
        let central = DesktopCentral::open(os_radio(os), "vocabulary-host")
            .await
            .expect("open");
        known(&central, "peer-e").await;
        central.boundary().fail_next_with_platform(
            FaultOp::Connect,
            "not established",
            os.not_established(),
        );
        let error = central
            .connect("peer-e", "lease-a", OpControl::budget_ms(1000))
            .await
            .expect_err("not established");
        assert_eq!(
            observed(&error),
            expected("connect-not-established", os),
            "{os:?}"
        );

        // peer-not-found: every desktop OS resolves the peer out of the
        // adapter's listing before connecting (the scripted radio connects
        // anything, so the production lookup answers here).
        let error = ubm_desktop::btleplug_backend::find_peer(
            Ok::<Vec<String>, btleplug::Error>(Vec::new()),
            "peer-never-seen",
            String::clone,
        )
        .expect_err("unknown");
        assert_eq!(observed(&error), expected("peer-not-found", os), "{os:?}");

        // security-refused
        let central = connected("peer-f", os).await;
        central
            .boundary()
            .fail_next_with_platform(FaultOp::Read, "refused", os.security());
        let error = central
            .read("peer-f", &selector(), OpControl::budget_ms(1000))
            .await
            .expect_err("refused");
        assert_eq!(observed(&error), expected("security-refused", os), "{os:?}");

        // operation-timed-out
        let central = connected("peer-g", os).await;
        let error = pending_read_ended_by(&central, "peer-g", 50, async |_| {}).await;
        assert_eq!(
            observed(&error),
            expected("operation-timed-out", os),
            "{os:?}"
        );

        // operation-cancelled
        let central = connected("peer-h", os).await;
        central.boundary().block_op(FaultOp::Read);
        let ctl = OpControl::budget_ms(60_000);
        let ticket = ctl.ticket.clone();
        let reader = central.clone();
        let pending = tokio::spawn(async move { reader.read("peer-h", &selector(), ctl).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        central.cancel(&ticket).await.expect("cancel");
        let error = pending.await.expect("task").expect_err("cancelled");
        central.boundary().unblock_op(FaultOp::Read);
        assert_eq!(
            observed(&error),
            expected("operation-cancelled", os),
            "{os:?}"
        );
    }
}
