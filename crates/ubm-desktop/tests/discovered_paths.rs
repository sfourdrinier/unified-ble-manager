//! F01: the dispatch surface reports the full registered discovery tree.
//!
//! `discover` returns counts only; packed consumers build selectors and
//! render databases from [`DesktopCentral::discovered_paths`], which must
//! report every registered path level (service, characteristic, descriptor)
//! with exact UUIDs, occurrences, and property bits — never a subset, never
//! reordered (registration order). Unknown peers fail with `peer.not-found`
//! and undiscovered peers with `gatt.discovery-required`, never an empty
//! list that a consumer could mistake for an empty database.

use ubm_desktop::OpControl;
use ubm_desktop::{
    CharacteristicSnapshot, DescriptorSnapshot, DesktopCentral, FakeRadio, PeerSnapshot,
    PropertyFlags, RadioEvent, ServiceSnapshot,
};

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const HRM_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
const HRM_BODY_LOCATION: &str = "00002a38-0000-1000-8000-00805f9b34fb";
const CHAR_USER_DESCRIPTION: &str = "00002901-0000-1000-8000-00805f9b34fb";
const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";

fn db_services() -> Vec<ServiceSnapshot> {
    vec![
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: vec![
                CharacteristicSnapshot {
                    uuid: HRM_MEASUREMENT.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: true,
                        write: false,
                        write_without_response: false,
                        notify: true,
                        indicate: false,
                    },
                    descriptors: vec![DescriptorSnapshot {
                        uuid: CHAR_USER_DESCRIPTION.to_owned(),
                        occurrence: 0,
                    }],
                },
                CharacteristicSnapshot {
                    uuid: HRM_BODY_LOCATION.to_owned(),
                    occurrence: 0,
                    properties: PropertyFlags {
                        read: true,
                        write: false,
                        write_without_response: false,
                        notify: false,
                        indicate: false,
                    },
                    descriptors: Vec::new(),
                },
            ],
        },
        // Duplicate service UUID: occurrences keep the instances distinct.
        ServiceSnapshot {
            uuid: HRM_SERVICE.to_owned(),
            occurrence: 1,
            characteristics: Vec::new(),
        },
        ServiceSnapshot {
            uuid: BATTERY_SERVICE.to_owned(),
            occurrence: 0,
            characteristics: Vec::new(),
        },
    ]
}

async fn discovered_central() -> DesktopCentral<FakeRadio> {
    let central = DesktopCentral::open(FakeRadio::new(), "test-host")
        .await
        .expect("open");
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: "peer-1".to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: ubm_desktop::AdvertisementExtras::default(),
        }));
    central
        .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    central.boundary().set_services("peer-1", db_services());
    central
        .discover("peer-1", "lease-a", OpControl::unbounded())
        .await
        .expect("discover");
    central
}

#[tokio::test]
async fn discovered_paths_reports_every_registered_level() {
    let central = discovered_central().await;
    let paths = central
        .discovered_paths("peer-1")
        .await
        .expect("discovered paths");
    // Registration order: service, then each characteristic with its
    // descriptors, per service in snapshot order.
    let shape: Vec<(String, u64, Option<String>, Option<String>)> = paths
        .iter()
        .map(|path| {
            (
                path.service_uuid.clone(),
                path.service_occurrence,
                path.characteristic_uuid.clone(),
                path.descriptor_uuid.clone(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (HRM_SERVICE.to_owned(), 0, None, None),
            (
                HRM_SERVICE.to_owned(),
                0,
                Some(HRM_MEASUREMENT.to_owned()),
                None
            ),
            (
                HRM_SERVICE.to_owned(),
                0,
                Some(HRM_MEASUREMENT.to_owned()),
                Some(CHAR_USER_DESCRIPTION.to_owned())
            ),
            (
                HRM_SERVICE.to_owned(),
                0,
                Some(HRM_BODY_LOCATION.to_owned()),
                None
            ),
            (HRM_SERVICE.to_owned(), 1, None, None),
            (BATTERY_SERVICE.to_owned(), 0, None, None),
        ]
    );
    // Property bits survive verbatim (measurement is read+notify).
    let measurement = paths
        .iter()
        .find(|path| {
            path.characteristic_uuid.as_deref() == Some(HRM_MEASUREMENT)
                && path.descriptor_uuid.is_none()
        })
        .expect("measurement path");
    assert_eq!(measurement.characteristic_occurrence, Some(0));
    assert_ne!(measurement.properties & 0x01, 0, "read bit survives");
    assert_ne!(measurement.properties & 0x08, 0, "notify bit survives");
}

#[tokio::test]
async fn discovered_paths_rejects_unknown_peer() {
    let central = discovered_central().await;
    let error = central
        .discovered_paths("peer-unknown")
        .await
        .expect_err("unknown peer must fail");
    assert_eq!(error.code_str(), "peer.not-found");
}

#[tokio::test]
async fn discovered_paths_requires_discovery() {
    let central = DesktopCentral::open(FakeRadio::new(), "test-host")
        .await
        .expect("open");
    central
        .boundary()
        .push_event(RadioEvent::Advertisement(PeerSnapshot {
            id: "peer-1".to_owned(),
            address: None,
            service_uuids: vec![HRM_SERVICE.to_owned()],
            rssi: Some(-60),
            local_name: None,
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: ubm_desktop::AdvertisementExtras::default(),
        }));
    central
        .connect("peer-1", "lease-a", OpControl::budget_ms(5000))
        .await
        .expect("connect");
    let error = central
        .discovered_paths("peer-1")
        .await
        .expect_err("undiscovered peer must fail");
    assert_eq!(error.code_str(), "gatt.discovery-required");
}
