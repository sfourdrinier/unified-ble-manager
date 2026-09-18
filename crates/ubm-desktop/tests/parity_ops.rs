//! Desktop parity operations over the scriptable radio (PR210 decision 7):
//! link security (state, pair, cancel, unpair, events, generation hold),
//! adapter authorization, address targeting, address type, characteristic
//! facts and the per-mode maximum write length. The OS adapters behind
//! these calls are exercised on their hosts; here the central's own rules
//! are proven: admission, arbitration, one answer per ceremony, and no
//! substituted values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ubm_desktop::{
    AdapterAuthorization, AddressType, BondState, CancelPairingOutcome, CharacteristicAccess,
    CharacteristicSnapshot, DesktopCentral, FakeRadio, FaultOp, OpControl, PairOutcome,
    PairRequest, PairingGeneration, PairingGenerationController, PeerSnapshot, PropertyFlags,
    RadioEvent, SecureConnections, SecurityState, ServiceSnapshot, UnpairOutcome, WriteLimits,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_CONTROL: &str = "00002a39-0000-1000-8000-00805f9b34fb";

fn advertisement(peer_id: &str) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: vec![HRM_SERVICE.to_owned()],
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

fn control_service() -> ServiceSnapshot {
    ServiceSnapshot {
        uuid: HRM_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![CharacteristicSnapshot {
            uuid: HRM_CONTROL.to_owned(),
            occurrence: 0,
            properties: PropertyFlags {
                read: true,
                write: true,
                write_without_response: true,
                notify: false,
                indicate: false,
            },
            descriptors: Vec::new(),
        }],
    }
}

fn selector() -> ubm_desktop::PathSelector {
    DesktopCentral::<FakeRadio>::selector(
        HRM_SERVICE,
        Some(0),
        Some(HRM_CONTROL),
        Some(0),
        None,
        None,
    )
    .expect("selector")
}

async fn open() -> DesktopCentral<FakeRadio> {
    DesktopCentral::open(FakeRadio::new(), "parity-host")
        .await
        .expect("open")
}

async fn known_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    central.boundary().push_event(advertisement(peer_id));
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("peer {peer_id} never resolved");
}

async fn connected_peer(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    known_peer(central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![control_service()]);
    central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
}

const BONDED: SecurityState = SecurityState {
    bond: BondState::Bonded,
    pairing_possible: Some(true),
};
const NOT_BONDED: SecurityState = SecurityState {
    bond: BondState::NotBonded,
    pairing_possible: Some(true),
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn security_state_reads_the_os_answer_and_unsupported_stays_unsupported() {
    let central = open().await;
    known_peer(&central, "peer-s").await;
    let error = central
        .security_state("peer-s", OpControl::budget_ms(1000))
        .await
        .expect_err("a radio without security says so");
    assert_eq!(error.code_str(), "capability.unsupported");
    central.boundary().set_security("peer-s", NOT_BONDED);
    assert_eq!(
        central
            .security_state("peer-s", OpControl::budget_ms(1000))
            .await
            .expect("state"),
        NOT_BONDED
    );
    let unknown = central
        .security_state("peer-nobody", OpControl::budget_ms(1000))
        .await
        .expect_err("unknown peer");
    assert_eq!(unknown.code_str(), "peer.not-found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_publishes_the_ceremonys_state_and_unpair_publishes_the_release() {
    let central = open().await;
    known_peer(&central, "peer-p").await;
    central.boundary().set_security("peer-p", NOT_BONDED);
    let mut events = central.security_events();
    let outcome = central
        .pair("peer-p", PairRequest::default(), OpControl::budget_ms(5000))
        .await
        .expect("pair");
    assert_eq!(outcome, PairOutcome::Paired(BONDED));
    let event = events.try_recv().expect("pair publishes the new state");
    assert_eq!(event.peer_id, "peer-p");
    assert_eq!(event.state, BONDED);
    assert_eq!(
        central
            .unpair("peer-p", OpControl::budget_ms(5000))
            .await
            .expect("unpair"),
        UnpairOutcome::Unpaired
    );
    let event = events
        .try_recv()
        .expect("unpair publishes the released bond");
    assert_eq!(event.state.bond, BondState::NotBonded);
    assert!(event.sequence > 1, "sequences increase");
    assert_eq!(
        central
            .unpair("peer-p", OpControl::budget_ms(5000))
            .await
            .expect("idempotent unpair"),
        UnpairOutcome::AlreadyUnpaired
    );
    assert_eq!(central.resource_counters().await.pairings_in_flight, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_ceremony_per_peer_and_cancel_reads_the_ceremonys_own_answer() {
    let central = open().await;
    known_peer(&central, "peer-c").await;
    central.boundary().block_op(FaultOp::Pair);
    let pairing = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .pair(
                    "peer-c",
                    PairRequest::default(),
                    OpControl::budget_ms(10_000),
                )
                .await
        }
    });
    for _ in 0..2000 {
        if central.resource_counters().await.pairings_in_flight == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let second = central
        .pair("peer-c", PairRequest::default(), OpControl::budget_ms(1000))
        .await
        .expect_err("a second ceremony is refused");
    assert_eq!(second.code_str(), "ownership.denied");
    let unpair = central
        .unpair("peer-c", OpControl::budget_ms(1000))
        .await
        .expect_err("unpair waits for the ceremony");
    assert_eq!(unpair.code_str(), "ownership.denied");
    let cancelled = central
        .cancel_pairing("peer-c", OpControl::budget_ms(5000))
        .await
        .expect("cancel");
    assert_eq!(cancelled, CancelPairingOutcome::Cancelled);
    assert_eq!(
        pairing.await.expect("join").expect("pair answers"),
        PairOutcome::Cancelled,
        "pair and cancel agree about one ceremony"
    );
    assert_eq!(
        central
            .cancel_pairing("peer-c", OpControl::budget_ms(1000))
            .await
            .expect("nothing in flight"),
        CancelPairingOutcome::NotPairing
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_pair_asks_the_os_to_cancel_and_reports_a_won_bond() {
    let central = open().await;
    known_peer(&central, "peer-w").await;
    // The OS finishes the bond even though the caller gives up: the
    // pairing's own answer is `Paired`, never a claimed cancellation.
    central
        .boundary()
        .fail_next(FaultOp::CancelPairing, "too late");
    central.boundary().block_op(FaultOp::Pair);
    let control = OpControl::budget_ms(10_000);
    let ticket = control.ticket.clone();
    let pairing = tokio::spawn({
        let central = central.clone();
        async move {
            central
                .pair("peer-w", PairRequest::default(), control)
                .await
        }
    });
    for _ in 0..2000 {
        if central.resource_counters().await.pairings_in_flight == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central.cancel(&ticket).await.expect("cancel recorded");
    for _ in 0..2000 {
        if central
            .boundary()
            .calls()
            .iter()
            .any(|call| call == "cancel_pairing")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central.boundary().unblock_op(FaultOp::Pair);
    assert_eq!(
        pairing.await.expect("join").expect("pair answers"),
        PairOutcome::Paired(BONDED)
    );
}

#[derive(Default)]
struct RecordingController {
    generation: Mutex<Option<PairingGeneration>>,
    sets: Mutex<Vec<PairingGeneration>>,
    fail_restore: bool,
    reads: AtomicUsize,
}

impl PairingGenerationController for RecordingController {
    fn read<'a>(
        &'a self,
        _adapter_id: &'a str,
    ) -> ubm_desktop::central::ControllerFuture<'a, PairingGeneration> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let generation = self
            .generation
            .lock()
            .expect("generation")
            .unwrap_or(PairingGeneration::Enabled);
        Box::pin(async move { Ok(generation) })
    }

    fn set<'a>(
        &'a self,
        _adapter_id: &'a str,
        generation: PairingGeneration,
    ) -> ubm_desktop::central::ControllerFuture<'a, ()> {
        let mut sets = self.sets.lock().expect("sets");
        sets.push(generation);
        let restoring = sets.len() > 1;
        drop(sets);
        if restoring && self.fail_restore {
            return Box::pin(async { Err("mgmt refused".to_owned()) });
        }
        *self.generation.lock().expect("generation") = Some(generation);
        Box::pin(async { Ok(()) })
    }
}

/// A profile whose host supplies the privileged generation controller:
/// the desktop rows plus `security:pairing-generation`, which the scripted
/// radio can hold on any build host.
fn register_with_generation(
    core: &mut ubm_core::central::Central,
) -> Result<(), ubm_core::contracts::CoreError> {
    use ubm_core::central::{CapabilityDescriptor, CapabilityState, EvidenceLevel};

    ubm_desktop::register_desktop_capabilities(core)?;
    core.register_capability(CapabilityDescriptor::new(
        "security:pairing-generation",
        CapabilityState::Limited,
        &[("availability", 1)],
        &["host-supplied-controller"],
        "parity-test-pairing-generation",
        EvidenceLevel::Deterministic,
        "test",
        "parity-test",
        &["security.pairing-generation"],
    )?)
}

async fn open_with_generation() -> DesktopCentral<FakeRadio> {
    let mut profile = ubm_desktop::CentralProfile::desktop("parity-host");
    profile.register_capabilities = register_with_generation;
    DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .expect("open")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directed_generation_is_refused_where_the_capability_is_not_registered() {
    let central = open().await;
    known_peer(&central, "peer-n").await;
    let refused = central
        .pair(
            "peer-n",
            PairRequest {
                secure_connections: Some(SecureConnections::Require),
                generation_controller: Some(Arc::new(RecordingController::default())),
            },
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("the registered capability is the runtime truth");
    assert_eq!(refused.code_str(), "capability.unsupported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directed_generation_needs_the_hosts_privileged_controller() {
    let central = open_with_generation().await;
    known_peer(&central, "peer-g").await;
    let refused = central
        .pair(
            "peer-g",
            PairRequest {
                secure_connections: Some(SecureConnections::Require),
                generation_controller: None,
            },
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("privilege is never implicit");
    assert_eq!(refused.code_str(), "capability.unsupported");
    assert!(
        !central.boundary().calls().iter().any(|call| call == "pair"),
        "refused before any radio effect"
    );
    let controller = Arc::new(RecordingController::default());
    let outcome = central
        .pair(
            "peer-g",
            PairRequest {
                secure_connections: Some(SecureConnections::Require),
                generation_controller: Some(controller.clone()),
            },
            OpControl::budget_ms(5000),
        )
        .await
        .expect("held pair");
    assert_eq!(outcome, PairOutcome::Paired(BONDED));
    assert_eq!(
        *controller.sets.lock().expect("sets"),
        vec![PairingGeneration::Required, PairingGeneration::Enabled],
        "held at the required generation, then restored"
    );
    assert_eq!(controller.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_generation_restore_is_counted_never_swallowed() {
    let central = open_with_generation().await;
    known_peer(&central, "peer-r").await;
    let controller = Arc::new(RecordingController {
        fail_restore: true,
        ..RecordingController::default()
    });
    let outcome = central
        .pair(
            "peer-r",
            PairRequest {
                secure_connections: Some(SecureConnections::Disallow),
                generation_controller: Some(controller),
            },
            OpControl::budget_ms(5000),
        )
        .await
        .expect("the bond itself was created");
    assert_eq!(outcome, PairOutcome::Paired(BONDED));
    assert_eq!(
        central
            .resource_counters()
            .await
            .generation_restore_failures,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_authorization_is_the_os_answer_or_unsupported() {
    let central = open().await;
    let error = central
        .adapter_authorization(OpControl::budget_ms(1000))
        .await
        .expect_err("no authorization concept");
    assert_eq!(error.code_str(), "capability.unsupported");
    central
        .boundary()
        .set_adapter_authorization(AdapterAuthorization::Denied);
    assert_eq!(
        central
            .adapter_authorization(OpControl::budget_ms(1000))
            .await
            .expect("authorization"),
        AdapterAuthorization::Denied
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn address_targeting_validates_then_makes_the_peer_known() {
    let central = open().await;
    let invalid = central
        .resolve_address(
            "not-an-address",
            AddressType::Public,
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("malformed address");
    assert_eq!(invalid.code_str(), "argument.invalid");
    assert!(
        central.boundary().calls().is_empty()
            || !central
                .boundary()
                .calls()
                .iter()
                .any(|call| call == "resolve_address"),
        "validation precedes the radio"
    );
    central.boundary().set_address(
        "AA:BB:CC:DD:EE:01",
        AddressType::Random,
        "hci0/dev_AA_BB_CC_DD_EE_01",
    );
    let peer_id = central
        .resolve_address(
            "aa:bb:cc:dd:ee:01",
            AddressType::Random,
            OpControl::budget_ms(1000),
        )
        .await
        .expect("resolved");
    assert_eq!(peer_id, "hci0/dev_AA_BB_CC_DD_EE_01");
    assert!(
        central.peer_key_for(&peer_id).await.is_some(),
        "an address-targeted peer is connectable without an advertisement"
    );
    central
        .boundary()
        .set_address_type(&peer_id, AddressType::Random);
    assert_eq!(
        central
            .address_type(&peer_id, OpControl::budget_ms(1000))
            .await
            .expect("type"),
        Some(AddressType::Random)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn characteristic_facts_ride_the_discovered_paths() {
    let central = open().await;
    known_peer(&central, "peer-f").await;
    let access = CharacteristicAccess {
        reliable_write: Some(true),
        encrypt_write: Some(true),
        ..CharacteristicAccess::default()
    };
    central.boundary().set_characteristic_access(
        "peer-f",
        HashMap::from([(
            (
                "peer-f".to_owned(),
                HRM_SERVICE.to_owned(),
                0,
                HRM_CONTROL.to_owned(),
                0,
            ),
            access,
        )]),
    );
    central
        .boundary()
        .set_services("peer-f", vec![control_service()]);
    central
        .connect("peer-f", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    let report = central
        .discover("peer-f", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    assert!(report.access_error.is_none());
    let paths = central.discovered_paths("peer-f").await.expect("paths");
    let characteristic = paths
        .iter()
        .find(|path| path.characteristic_uuid.as_deref() == Some(HRM_CONTROL))
        .expect("characteristic path");
    assert_eq!(characteristic.access, Some(access));
    let service = paths
        .iter()
        .find(|path| path.characteristic_uuid.is_none())
        .expect("service path");
    assert_eq!(service.access, None, "facts are characteristic-level only");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maximum_write_length_is_per_mode_and_matches_what_write_enforces() {
    let central = open().await;
    connected_peer(&central, "peer-m").await;
    central.boundary().set_write_limits(
        "peer-m",
        WriteLimits {
            with_response: 509,
            without_response: 182,
        },
    );
    let with_response = central
        .maximum_write_length(
            "peer-m",
            "lease-a",
            &selector(),
            true,
            OpControl::budget_ms(1000),
        )
        .await
        .expect("with response");
    let without_response = central
        .maximum_write_length(
            "peer-m",
            "lease-a",
            &selector(),
            false,
            OpControl::budget_ms(1000),
        )
        .await
        .expect("without response");
    assert_eq!((with_response, without_response), (509, 182));
    central
        .write(
            "peer-m",
            &selector(),
            vec![0; 300],
            "with-response",
            OpControl::budget_ms(1000),
        )
        .await
        .expect("a long with-response write inside its limit");
    let refused = central
        .write(
            "peer-m",
            &selector(),
            vec![0; 300],
            "without-response",
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("the command limit is smaller");
    assert_eq!(refused.code_str(), "bytes.too-large");
    let foreign = central
        .maximum_write_length(
            "peer-m",
            "lease-z",
            &selector(),
            true,
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("foreign lease");
    assert_eq!(foreign.code_str(), "ownership.denied");
}

/// Finding 41: a write that reached the radio and failed with any code may
/// still have reached the peer. Its commit is `unknown` and it is never
/// caller-retryable — not only when it was aborted or timed out.
/// Finding 81 (N2): where the OS performs the long write itself (WinRT
/// `WriteValueAsync` with response, BlueZ `WriteValue`, the Android stack),
/// a with-response characteristic or descriptor write carries a whole
/// attribute value (512 bytes), exactly as the legacy WinRT addon and
/// Tauri 4.x did; a command stays bounded by one ATT payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_os_long_write_link_carries_a_whole_attribute_value_with_response() {
    const USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";
    let central = open().await;
    known_peer(&central, "peer-l").await;
    let mut service = control_service();
    service.characteristics[0]
        .descriptors
        .push(ubm_desktop::DescriptorSnapshot {
            uuid: USER_DESCRIPTION.to_owned(),
            occurrence: 0,
        });
    central.boundary().set_services("peer-l", vec![service]);
    central
        .connect("peer-l", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover("peer-l", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    let limits = WriteLimits::os_long_write(ubm_desktop::ATT_DEFAULT_LE_MTU).expect("limits");
    assert_eq!(
        (limits.with_response, limits.without_response),
        (ubm_desktop::ATT_MAX_ATTRIBUTE_VALUE, 20)
    );
    central.boundary().set_write_limits("peer-l", limits);
    let write = |len: usize, mode: &'static str| {
        let central = &central;
        async move {
            central
                .write(
                    "peer-l",
                    &selector(),
                    vec![0; len],
                    mode,
                    OpControl::budget_ms(1000),
                )
                .await
        }
    };
    write(512, "with-response")
        .await
        .expect("a whole attribute value at the default MTU");
    assert_eq!(
        write(513, "with-response")
            .await
            .expect_err("past the attribute ceiling")
            .code_str(),
        "bytes.too-large"
    );
    write(20, "without-response")
        .await
        .expect("one ATT payload");
    assert_eq!(
        write(21, "without-response")
            .await
            .expect_err("a command is one payload")
            .code_str(),
        "bytes.too-large"
    );
    let descriptor = DesktopCentral::<FakeRadio>::selector(
        HRM_SERVICE,
        Some(0),
        Some(HRM_CONTROL),
        Some(0),
        Some(USER_DESCRIPTION),
        Some(0),
    )
    .expect("descriptor selector");
    central
        .write_descriptor(
            "peer-l",
            &descriptor,
            vec![0; 512],
            OpControl::budget_ms(1000),
        )
        .await
        .expect("a descriptor write is a long write too");
    assert_eq!(
        central
            .maximum_write_length(
                "peer-l",
                "lease-a",
                &selector(),
                true,
                OpControl::budget_ms(1000)
            )
            .await
            .expect("reported maximum"),
        512,
        "the reported maximum is what write enforces"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f41_a_dispatched_write_failure_of_any_code_is_commit_unknown_never_retryable() {
    let central = open().await;
    connected_peer(&central, "peer-x").await;
    central.boundary().set_mtu("peer-x", 185);
    central
        .boundary()
        .fail_next(FaultOp::Write, "link dropped mid-write");
    let error = central
        .write(
            "peer-x",
            &selector(),
            vec![1, 2, 3],
            "with-response",
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("the radio refused");
    assert_eq!(error.code_str(), "gatt.write-failed");
    assert_eq!(
        error.commit(),
        Some(ubm_core::contracts::CommitState::Unknown),
        "a dispatched write's commit is unknown"
    );
    assert_eq!(error.retryability(), ubm_desktop::Retryability::Never);
    // A refusal before dispatch stays not-dispatched and caller-decides.
    let early = central
        .write(
            "peer-x",
            &selector(),
            vec![0; 400],
            "without-response",
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("too large before dispatch");
    assert_eq!(early.code_str(), "bytes.too-large");
    assert_ne!(
        early.commit(),
        Some(ubm_core::contracts::CommitState::Unknown)
    );
}

const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";

fn notify_service() -> ServiceSnapshot {
    ServiceSnapshot {
        uuid: HRM_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![CharacteristicSnapshot {
            uuid: HRM_MEASUREMENT.to_owned(),
            occurrence: 0,
            properties: PropertyFlags {
                read: false,
                write: false,
                write_without_response: false,
                notify: true,
                indicate: false,
            },
            descriptors: Vec::new(),
        }],
    }
}

fn measurement() -> ubm_desktop::PathSelector {
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

async fn subscribed_then_services_changed(central: &DesktopCentral<FakeRadio>, peer_id: &str) {
    known_peer(central, peer_id).await;
    central
        .boundary()
        .set_services(peer_id, vec![notify_service()]);
    central
        .connect(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central
        .discover(peer_id, "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    central
        .subscribe(
            peer_id,
            &measurement(),
            "consumer",
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("subscribe");
    assert_eq!(central.boundary().live_subscription_count(), 1);
    let mut events = central.lifecycle_events();
    central
        .boundary()
        .push_event(RadioEvent::ServicesChanged(peer_id.to_owned()));
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("event in time")
        .expect("event");
    assert_eq!(event.kind, ubm_desktop::LifecycleKind::ServicesChanged);
}

/// Finding 40: a service change invalidates the path, but the OS-side CCCD
/// may still be live. Unsubscribe must still disable it through the
/// instance the enable addressed, never answer `gatt.not-found` and leave
/// the OS subscription running until shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f40_unsubscribe_after_a_service_change_still_disables_the_cccd() {
    let central = open().await;
    subscribed_then_services_changed(&central, "peer-40").await;
    let disabled = central
        .unsubscribe(
            "peer-40",
            &measurement(),
            "consumer",
            OpControl::budget_ms(5000),
        )
        .await
        .expect("the retained enablement is released");
    assert!(disabled, "the physical CCCD was disabled");
    assert_eq!(
        central.boundary().live_subscription_count(),
        0,
        "no OS subscription outlives the unsubscribe"
    );
    let again = central
        .unsubscribe(
            "peer-40",
            &measurement(),
            "consumer",
            OpControl::budget_ms(5000),
        )
        .await;
    assert!(
        again.is_err(),
        "a second unsubscribe has nothing left to release"
    );
}

/// Finding 40 counterpart: a failed physical release after a service change
/// is reported, kept, and a retry reaches the radio again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f40_a_failed_release_after_a_service_change_is_reported_and_retried() {
    let central = open().await;
    subscribed_then_services_changed(&central, "peer-41").await;
    central
        .boundary()
        .fail_next(FaultOp::Unsubscribe, "os refused");
    let error = central
        .unsubscribe(
            "peer-41",
            &measurement(),
            "consumer",
            OpControl::budget_ms(5000),
        )
        .await
        .expect_err("the release failed");
    assert_eq!(error.code_str(), "gatt.subscribe-failed");
    assert_eq!(central.boundary().live_subscription_count(), 1);
    assert!(
        central
            .unsubscribe(
                "peer-41",
                &measurement(),
                "consumer",
                OpControl::budget_ms(5000),
            )
            .await
            .expect("the retry releases"),
    );
    assert_eq!(central.boundary().live_subscription_count(), 0);
}

/// Advertisement extras (legacy CoreBluetooth solicited / overflow service
/// UUIDs and connectable) reach the host verbatim, absent vs empty kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advertisement_extras_reach_the_host_verbatim() {
    let central = open().await;
    central
        .start_scan("scanner", &[], OpControl::budget_ms(5000))
        .await
        .expect("scan");
    let extras = ubm_desktop::AdvertisementExtras {
        solicited_service_uuids: Some(vec![HRM_SERVICE.to_owned()]),
        overflow_service_uuids: Some(Vec::new()),
        connectable: None,
        appearance: None,
        raw_record: None,
        source: ubm_desktop::ObservationSource::DeviceState,
    };
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: "peer-adv".to_owned(),
            address: None,
            service_uuids: Vec::new(),
            rssi: Some(-50),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: extras.clone(),
        }));
    let mut taken = None;
    for _ in 0..2000 {
        taken = central.take_advertisement().await;
        if taken.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(taken.expect("observation").extras, extras);
}

/// Legacy BlueZ `busKind`: the profile carries the bus to the radio, the
/// default is the system bus, and a bus this build cannot honour fails the
/// open before any radio work — never a silent fall back to the system bus.
#[test]
fn bluez_bus_choice_is_carried_and_never_silently_replaced() {
    assert_eq!(
        ubm_desktop::CentralProfile::desktop("bus-host").bluez_bus,
        ubm_desktop::BluezBus::System
    );
    assert!(ubm_desktop::bluez_bus_supported(ubm_desktop::BluezBus::System).is_ok());
    let session = ubm_desktop::bluez_bus_supported(ubm_desktop::BluezBus::Session);
    if cfg!(target_os = "linux") {
        assert!(session.is_ok(), "Linux with the vendored patch honours it");
    } else {
        assert_eq!(
            session
                .expect_err("BlueZ bus choice is Linux-only")
                .code_str(),
            "capability.unsupported"
        );
    }
}

#[cfg(all(feature = "btleplug", not(target_os = "linux")))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_bus_request_off_linux_fails_before_any_radio_opens() {
    let mut profile = ubm_desktop::CentralProfile::desktop("bus-host");
    profile.bluez_bus = ubm_desktop::BluezBus::Session;
    let error = ubm_desktop::executor::desktop_runtime()
        .spawn(DesktopCentral::open_btleplug(profile))
        .await
        .expect("join")
        .err()
        .expect("refused");
    assert_eq!(error.code_str(), "capability.unsupported");
}

/// Legacy CoreBluetooth write-without-response readiness watch: a probe
/// for the lease holder plus readiness reports carrying the connection
/// generation they arrived under.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_readiness_is_probed_and_reported_per_connection() {
    let central = open().await;
    connected_peer(&central, "peer-wr").await;
    let unsupported = central
        .write_readiness("peer-wr", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("a radio without readiness says so");
    assert_eq!(unsupported.code_str(), "capability.unsupported");
    central.boundary().set_write_readiness("peer-wr", false);
    assert!(
        !central
            .write_readiness("peer-wr", "lease-a", OpControl::budget_ms(1000))
            .await
            .expect("probe")
    );
    let foreign = central
        .write_readiness("peer-wr", "lease-z", OpControl::budget_ms(1000))
        .await
        .expect_err("foreign lease");
    assert_eq!(foreign.code_str(), "ownership.denied");
    let generation = central
        .peer_records()
        .await
        .into_iter()
        .find(|record| record.peer_id == "peer-wr")
        .and_then(|record| record.connection_generation);
    let mut reports = central.write_readiness_events();
    central.boundary().push_event(RadioEvent::WriteReadiness {
        peer_id: "peer-wr".to_owned(),
        ready: true,
    });
    let report = tokio::time::timeout(Duration::from_secs(2), reports.recv())
        .await
        .expect("in time")
        .expect("report");
    assert!(report.ready);
    assert_eq!(report.peer_id, "peer-wr");
    assert_eq!(report.connection_generation, generation);
}

/// Legacy WinRT `OnScanTerminal`: a scan the OS ends on its own releases
/// the scan owner and is reported with the OS's reason; a report with no
/// active scan (e.g. the OS confirming a requested stop late) ends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_os_ended_scan_is_released_and_reported() {
    let central = open().await;
    let mut terminals = central.scan_terminal_events();
    let session = central
        .start_scan("scanner", &[], OpControl::budget_ms(1000))
        .await
        .expect("scan");
    central.boundary().push_event(RadioEvent::ScanTerminated {
        aborted: true,
        detail: "the WinRT advertisement watcher stopped (RadioNotAvailable)".to_owned(),
    });
    let terminal = tokio::time::timeout(Duration::from_secs(2), terminals.recv())
        .await
        .expect("in time")
        .expect("terminal");
    assert!(terminal.aborted);
    assert_eq!(&terminal.operation_id, session.operation_id());
    assert!(terminal.detail.contains("RadioNotAvailable"));
    assert!(!central.has_active_scan().await, "the owner is released");
    // A report with no active scan ends nothing and publishes nothing.
    central.boundary().push_event(RadioEvent::ScanTerminated {
        aborted: false,
        detail: "late stop confirmation".to_owned(),
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), terminals.recv())
            .await
            .is_err(),
        "no scan was active"
    );
    central
        .start_scan("scanner", &[], OpControl::budget_ms(1000))
        .await
        .expect("a new scan can start");
}

/// Finding 98: on a radio whose OS answers an unflagged subscribe (BlueZ
/// `StartNotify`, called unchecked by the legacy BlueZ backend) the
/// subscribe reaches the radio and its answer is the result; elsewhere the
/// missing property is refused before any effect, as the CoreBluetooth and
/// WinRT legacy backends did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unflagged_subscribe_reaches_only_an_os_that_answers_it() {
    for os_answers in [true, false] {
        let radio = FakeRadio::new();
        radio.set_os_answers_unflagged_subscribe(os_answers);
        let central = DesktopCentral::open(radio, "parity-host")
            .await
            .expect("open");
        connected_peer(&central, "peer-u").await;
        let answer = central
            .subscribe(
                "peer-u",
                &selector(),
                "lease-a",
                None,
                OpControl::budget_ms(1000),
            )
            .await;
        let enables = central
            .boundary()
            .calls()
            .iter()
            .filter(|call| call.as_str() == "set_notifications")
            .count();
        if os_answers {
            answer.expect("the OS answers");
            assert_eq!(enables, 1);
        } else {
            assert_eq!(
                answer.expect_err("refused").code_str(),
                "gatt.property-not-supported"
            );
            assert_eq!(enables, 0, "no effect");
        }
    }
}

/// Finding 113: the platform's structured answer behind a radio failure
/// reaches the caller through the central's classification unchanged, on
/// reads, writes and connects alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_platform_answer_survives_the_central() {
    use ubm_desktop::{PlatformDetail, PlatformValue};
    let central = open().await;
    connected_peer(&central, "peer-p").await;
    let winrt = PlatformDetail::new("winrt", "gatt-status")
        .with_message("read failed")
        .with_metadata("gattStatus", PlatformValue::Text("protocol-error".into()));
    central
        .boundary()
        .fail_next_with_platform(FaultOp::Read, "os refused", winrt.clone());
    let read = central
        .read("peer-p", &selector(), OpControl::budget_ms(1000))
        .await
        .expect_err("read refused");
    assert_eq!(read.code_str(), "gatt.read-failed");
    assert_eq!(read.platform(), Some(&winrt));

    let corebluetooth = PlatformDetail::new("corebluetooth", "3");
    central.boundary().set_mtu("peer-p", 185);
    central
        .boundary()
        .fail_next_with_platform(FaultOp::Write, "os refused", corebluetooth.clone());
    let write = central
        .write(
            "peer-p",
            &selector(),
            vec![1],
            "with-response",
            OpControl::budget_ms(1000),
        )
        .await
        .expect_err("write refused");
    assert_eq!(write.code_str(), "gatt.write-failed");
    assert_eq!(
        write.platform(),
        Some(&corebluetooth),
        "kept on a dispatched write"
    );

    known_peer(&central, "peer-q").await;
    let bluez = PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed");
    central
        .boundary()
        .fail_next_with_platform(FaultOp::Connect, "os refused", bluez.clone());
    let connect = central
        .connect("peer-q", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("connect refused");
    assert_eq!(connect.platform(), Some(&bluez));
}

/// Owner decision (5.0): a connect the platform could not establish
/// transiently answers `caller-decides` through the central with the
/// platform's answer kept; a connect refused for any other reason stays
/// `never`. The central never retries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_connect_failure_is_caller_decides_through_the_central() {
    use ubm_desktop::{PlatformDetail, PlatformValue, Retryability};
    let central = open().await;
    let transient = [
        PlatformDetail::new("corebluetooth", "10").with_metadata(
            "nsErrorDomain",
            PlatformValue::Text("CBErrorDomain".to_owned()),
        ),
        PlatformDetail::new("winrt", "gatt-status")
            .with_metadata("gattStatus", PlatformValue::Text("unreachable".to_owned())),
        PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
            .with_message("le-connection-abort-by-local"),
    ];
    for (index, platform) in transient.iter().enumerate() {
        let peer = format!("peer-t{index}");
        known_peer(&central, &peer).await;
        central.boundary().fail_next_with_platform(
            FaultOp::Connect,
            "link not established",
            platform.clone(),
        );
        let connects_before = connect_calls(&central);
        let error = central
            .connect(&peer, "lease-a", OpControl::budget_ms(1000))
            .await
            .expect_err("connect failed");
        assert_eq!(
            error.retryability(),
            Retryability::CallerDecides,
            "{platform:?}"
        );
        assert_eq!(error.platform(), Some(platform));
        assert_eq!(
            connect_calls(&central),
            connects_before + 1,
            "no internal retry"
        );
    }
    known_peer(&central, "peer-n").await;
    central.boundary().fail_next_with_platform(
        FaultOp::Connect,
        "not ready",
        PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotReady"),
    );
    let refused = central
        .connect("peer-n", "lease-a", OpControl::budget_ms(1000))
        .await
        .expect_err("connect refused");
    assert_eq!(refused.retryability(), Retryability::Never);
}

fn connect_calls(central: &DesktopCentral<FakeRadio>) -> usize {
    central
        .boundary()
        .calls()
        .iter()
        .filter(|call| call.as_str() == "connect")
        .count()
}

/// Owner decision (5.0): an operation the platform fails because the link
/// is gone reports `connection.lost` through the central on every desktop
/// OS, as Android does, with the platform's answer kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_loss_answer_is_connection_lost_through_the_central() {
    use ubm_desktop::{PlatformDetail, PlatformValue};
    let answers = [
        PlatformDetail::new("corebluetooth", "7").with_metadata(
            "nsErrorDomain",
            PlatformValue::Text("CBErrorDomain".to_owned()),
        ),
        PlatformDetail::new("winrt", "gatt-status")
            .with_metadata("gattStatus", PlatformValue::Text("unreachable".to_owned())),
        PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed").with_message("Not connected"),
        PlatformDetail::new("btleplug", "not-connected").with_message("Not connected"),
    ];
    for (index, platform) in answers.iter().enumerate() {
        let central = open().await;
        let peer = format!("peer-l{index}");
        connected_peer(&central, &peer).await;
        central
            .boundary()
            .fail_next_with_platform(FaultOp::Read, "gone", platform.clone());
        let read = central
            .read(&peer, &selector(), OpControl::budget_ms(1000))
            .await
            .expect_err("read on a lost link");
        assert_eq!(read.code_str(), "connection.lost", "{platform:?}");
        assert_eq!(read.platform(), Some(platform));
        central
            .boundary()
            .fail_next_with_platform(FaultOp::Discover, "gone", platform.clone());
        let discover = central
            .discover(&peer, "lease-a", OpControl::budget_ms(1000))
            .await
            .expect_err("discovery on a lost link");
        assert_eq!(discover.code_str(), "connection.lost", "{platform:?}");
        assert_eq!(discover.platform(), Some(platform));
    }
}
