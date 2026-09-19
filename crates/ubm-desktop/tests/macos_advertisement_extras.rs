//! Vendored btleplug patch #2 (`vendor/btleplug/UBM_PATCHES.md`): the
//! CoreBluetooth advertisement fields upstream drops (solicited and
//! overflow service UUIDs, connectable) parse from a real `NSDictionary`
//! shaped like CoreBluetooth's advertisement data, with absent, empty and
//! populated kept distinct. macOS only; no radio is touched.
#![cfg(all(target_os = "macos", feature = "btleplug"))]

use btleplug::platform::advertisement_extras;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_bluetooth::{
    CBAdvertisementDataIsConnectable, CBAdvertisementDataOverflowServiceUUIDsKey,
    CBAdvertisementDataSolicitedServiceUUIDsKey, CBUUID,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

fn object<T: objc2::Message>(value: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an `AnyObject`.
    unsafe { Retained::cast(value) }
}

#[test]
fn absent_keys_stay_absent() {
    let empty: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_vec::<NSString>(&[], Vec::new());
    let extras = advertisement_extras(&empty);
    assert_eq!(extras.solicited_service_uuids, None);
    assert_eq!(extras.overflow_service_uuids, None);
    assert_eq!(extras.connectable, None);
}

#[test]
fn present_keys_parse_and_empty_stays_distinct_from_absent() {
    let heart_rate = unsafe { CBUUID::UUIDWithString(&NSString::from_str("180D")) };
    let solicited = NSArray::from_vec(vec![heart_rate]);
    let overflow: Retained<NSArray<CBUUID>> = NSArray::from_vec(Vec::new());
    let keys: [&NSString; 3] = unsafe {
        [
            CBAdvertisementDataSolicitedServiceUUIDsKey,
            CBAdvertisementDataOverflowServiceUUIDsKey,
            CBAdvertisementDataIsConnectable,
        ]
    };
    let values = vec![
        object(solicited),
        object(overflow),
        object(NSNumber::new_bool(false)),
    ];
    let data: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_vec(&keys, values);
    let extras = advertisement_extras(&data);
    assert_eq!(
        extras.solicited_service_uuids,
        Some(vec![
            uuid::Uuid::parse_str("0000180d-0000-1000-8000-00805f9b34fb").expect("uuid")
        ])
    );
    assert_eq!(
        extras.overflow_service_uuids,
        Some(Vec::new()),
        "present but empty"
    );
    assert_eq!(extras.connectable, Some(false));
}
