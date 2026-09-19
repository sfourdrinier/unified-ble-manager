//! Finding 108: every mobile owner bound is at or above legacy React Native's.
//! Legacy queued 512 records and 1 MiB per binding on both platforms
//! (`UnifiedBleProtocolJsiBinding.cpp` `kMaximumQueuedRecords`,
//! `UnifiedBleProtocolAppleExecutionState.hpp` `kMaximum*Records`) and held
//! 1024 pending operations per attachment (`NativeOperationRegistry`).

use std::sync::Arc;

use ubm_desktop::{AdvertisementExtras, PeerSnapshot, RadioEvent};
use ubm_mobile::foreign::{
    ADVERTISEMENT_INGRESS_CAP, CONTROL_INGRESS_CAP, NOTIFICATION_INGRESS_BYTES,
    NOTIFICATION_INGRESS_CAP,
};
use ubm_mobile::{ForeignRadio, IngressClass, MobilePlatform, PlatformRadio, RadioRequest};

const LEGACY_RECORDS: usize = 512;
const LEGACY_BYTES: usize = 1 << 20;

struct Silent;

impl PlatformRadio for Silent {
    fn submit(&self, _request: RadioRequest) {}
    fn cancel(&self, _request_id: u64) {}
}

fn radio() -> ForeignRadio {
    ForeignRadio::new(Arc::new(Silent), MobilePlatform::Android, "caps".to_owned())
}

fn advertisement(index: usize) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: format!("peer-{index}"),
        address: None,
        service_uuids: Vec::new(),
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: AdvertisementExtras::default(),
    })
}

// A bound below legacy fails the build of this test target.
const _: () = {
    assert!(ADVERTISEMENT_INGRESS_CAP >= LEGACY_RECORDS);
    assert!(CONTROL_INGRESS_CAP >= LEGACY_RECORDS);
    assert!(NOTIFICATION_INGRESS_CAP >= LEGACY_RECORDS);
    assert!(NOTIFICATION_INGRESS_BYTES >= LEGACY_BYTES);
};

#[test]
fn a_legacy_burst_of_advertisements_is_queued_whole_and_the_next_one_counted() {
    let radio = radio();
    for index in 0..LEGACY_RECORDS.max(ADVERTISEMENT_INGRESS_CAP) {
        assert_eq!(
            radio.push_event(advertisement(index)),
            Ok(()),
            "advertisement {index}"
        );
    }
    assert_eq!(
        radio.push_event(advertisement(usize::MAX)),
        Err(IngressClass::Advertisement)
    );
}

#[test]
fn a_legacy_burst_of_control_facts_is_queued_whole_and_the_next_one_counted() {
    let radio = radio();
    for index in 0..LEGACY_RECORDS.max(CONTROL_INGRESS_CAP) {
        assert_eq!(
            radio.push_event(RadioEvent::ServicesChanged(format!("peer-{index}"))),
            Ok(()),
            "control fact {index}"
        );
    }
    assert_eq!(
        radio.push_event(RadioEvent::ServicesChanged("one-more".to_owned())),
        Err(IngressClass::Control)
    );
}
