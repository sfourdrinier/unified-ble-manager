//! Finding 95: no product quota below the legacy backends. The legacy
//! backends had no attribute, subscription, connection or remembered-peer
//! limit; the core's bounds are protocol limits (the ATT handle space per
//! database, the LE connection-handle space) or a memory bound far above any
//! radio environment. A database past its bound fails discovery whole —
//! never a partial snapshot (`docs/UNIFIED_SEMANTICS.md`, "no partial
//! snapshot becomes current").

use std::time::Duration;

use ubm_desktop::{
    CharacteristicSnapshot, DesktopCentral, FakeRadio, OpControl, PeerSnapshot, PropertyFlags,
    RadioEvent, ServiceSnapshot,
};

const NOTIFY: PropertyFlags = PropertyFlags {
    read: true,
    write: false,
    write_without_response: false,
    notify: true,
    indicate: false,
};

fn advertisement(peer_id: &str) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: Vec::new(),
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

fn uuid(index: usize) -> String {
    format!("{index:08x}-0000-1000-8000-00805f9b34fb")
}

/// `services` services of `characteristics` notifiable characteristics
/// each, every UUID distinct.
fn database(services: usize, characteristics: usize) -> Vec<ServiceSnapshot> {
    (0..services)
        .map(|service| ServiceSnapshot {
            uuid: uuid(0x1000 + service),
            occurrence: 0,
            characteristics: (0..characteristics)
                .map(|characteristic| CharacteristicSnapshot {
                    uuid: uuid(0x10_0000 + service * 1000 + characteristic),
                    occurrence: 0,
                    properties: NOTIFY,
                    descriptors: Vec::new(),
                })
                .collect(),
        })
        .collect()
}

async fn open() -> DesktopCentral<FakeRadio> {
    DesktopCentral::open(FakeRadio::new(), "database-bounds-host")
        .await
        .expect("open")
}

async fn connected(central: &DesktopCentral<FakeRadio>, peer_id: &str, lease: &str) {
    central.boundary().push_event(advertisement(peer_id));
    for _ in 0..2000 {
        if central.peer_key_for(peer_id).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    central
        .connect(peer_id, lease, OpControl::budget_ms(5000))
        .await
        .expect("connect");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_database_many_subscriptions_and_links_register_whole() {
    let central = open().await;
    // 20 services x 7 characteristics = 160 paths (the old bound was 128).
    connected(&central, "peer-big", "lease-a").await;
    central.boundary().set_services("peer-big", database(20, 7));
    let report = central
        .discover("peer-big", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("discover");
    assert_eq!(report.paths_registered, 160);
    assert_eq!(
        central
            .discovered_paths("peer-big")
            .await
            .expect("paths")
            .len(),
        160,
        "the whole database is current"
    );
    // 45 subscriptions on one link (the old bound was 32).
    for service in 0..20 {
        for characteristic in 0..7 {
            if service * 7 + characteristic >= 45 {
                break;
            }
            let selector = DesktopCentral::<FakeRadio>::selector(
                &uuid(0x1000 + service),
                Some(0),
                Some(&uuid(0x10_0000 + service * 1000 + characteristic)),
                Some(0),
                None,
                None,
            )
            .expect("selector");
            central
                .subscribe(
                    "peer-big",
                    &selector,
                    "lease-a",
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("subscription {service}/{characteristic}: {error:?}")
                });
        }
    }
    // 24 simultaneous links and remembered peers (the old bounds were 16).
    for index in 0..24 {
        connected(&central, &format!("peer-{index}"), "lease-a").await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_larger_than_the_att_handle_space_fails_discovery_whole() {
    let central = open().await;
    connected(&central, "peer-huge", "lease-a").await;
    // 1 service + 65 535 characteristics: one attribute past the handle
    // space.
    central
        .boundary()
        .set_services("peer-huge", database(1, 65_535));
    let refused = central
        .discover("peer-huge", "lease-a", OpControl::budget_ms(30_000))
        .await
        .expect_err("no GATT database holds this many attributes");
    assert_eq!(refused.code_str(), "capability.limited");
    assert_eq!(
        central
            .discovered_paths("peer-huge")
            .await
            .expect_err("nothing became current")
            .code_str(),
        "gatt.discovery-required"
    );
    // A database that fits discovers normally afterwards.
    central.boundary().set_services("peer-huge", database(2, 3));
    let report = central
        .discover("peer-huge", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("rediscover");
    assert_eq!(report.paths_registered, 8);
}

/// A UUID the OS reports that is not a UUID fails the discovery whole with
/// `protocol.malformed`, as the legacy backends did; nothing becomes
/// current and no entry is silently dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_os_uuid_fails_discovery_whole() {
    let central = open().await;
    connected(&central, "peer-bad", "lease-a").await;
    let mut services = database(2, 2);
    services[1].characteristics[1].uuid = "not-a-uuid".to_owned();
    central.boundary().set_services("peer-bad", services);
    let refused = central
        .discover("peer-bad", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect_err("a malformed UUID");
    assert_eq!(refused.code_str(), "protocol.malformed");
    assert!(
        refused
            .detail()
            .is_some_and(|detail| detail.contains("not-a-uuid"))
    );
    assert_eq!(
        central
            .discovered_paths("peer-bad")
            .await
            .expect_err("nothing became current")
            .code_str(),
        "gatt.discovery-required"
    );
}

/// A database exactly at the bound registers whole, in linear time: path
/// registration never scans every stored path (a full-size database took
/// minutes when it did).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_at_the_att_handle_space_registers_whole() {
    let central = open().await;
    connected(&central, "peer-full", "lease-a").await;
    // 1 service + 65 534 characteristics = 65 535 attributes.
    central
        .boundary()
        .set_services("peer-full", database(1, 65_534));
    let started = std::time::Instant::now();
    let report = central
        .discover("peer-full", "lease-a", OpControl::budget_ms(60_000))
        .await
        .expect("a full-size database");
    assert_eq!(report.paths_registered, 65_535);
    assert_eq!(
        central
            .discovered_paths("peer-full")
            .await
            .expect("paths")
            .len(),
        65_535
    );
    // Rediscovery revives every slot instead of growing history.
    central
        .discover("peer-full", "lease-a", OpControl::budget_ms(60_000))
        .await
        .expect("rediscover");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "registration is linear: {:?}",
        started.elapsed()
    );
}

/// Findings 106, 107: no admission quota below legacy. The legacy core
/// queued 8 operations per connection behind the one in flight, with no
/// per-owner or global cap, and shared a subscription among any number of
/// consumers. One lease runs 9 concurrent reads on each of 3 links, and 60
/// consumers share one subscription.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_lease_runs_legacy_concurrency_on_many_links_and_consumers() {
    let central = std::sync::Arc::new(open().await);
    let peers = ["peer-x", "peer-y", "peer-z"];
    for peer in peers {
        connected(&central, peer, "lease-a").await;
        central.boundary().set_services(peer, database(1, 1));
        central
            .discover(peer, "lease-a", OpControl::budget_ms(5000))
            .await
            .expect("discover");
    }
    let selector = DesktopCentral::<FakeRadio>::selector(
        &uuid(0x1000),
        Some(0),
        Some(&uuid(0x10_0000)),
        Some(0),
        None,
        None,
    )
    .expect("selector");
    central.boundary().block_op(ubm_desktop::FaultOp::Read);
    let mut reads = Vec::new();
    for peer in peers {
        for _ in 0..9 {
            let central = central.clone();
            let selector = selector.clone();
            reads.push(tokio::spawn(async move {
                central
                    .read(peer, &selector, OpControl::budget_ms(10_000))
                    .await
            }));
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while central
        .boundary()
        .calls()
        .iter()
        .filter(|call| call.as_str() == "read_characteristic")
        .count()
        < 27
    {
        assert!(
            !reads.iter().any(tokio::task::JoinHandle::is_finished),
            "no read is refused before the radio"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "all 27 reads are concurrently at the radio"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    central.boundary().unblock_all(ubm_desktop::FaultOp::Read);
    for read in reads {
        read.await
            .expect("join")
            .expect("every read is admitted and answered");
    }
    for consumer in 0..60 {
        central
            .subscribe(
                "peer-x",
                &selector,
                &format!("consumer-{consumer}"),
                None,
                OpControl::budget_ms(5000),
            )
            .await
            .unwrap_or_else(|error| panic!("consumer {consumer}: {error:?}"));
    }
}
