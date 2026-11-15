use super::super::ble::{characteristic::BLECharacteristic, descriptor::BLEDescriptor};
use crate::Result;
use std::{future::Future, pin::Pin};
use windows::Devices::Bluetooth::GenericAttributeProfile::{GattCharacteristic, GattDescriptor};

type Read = Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>;
type Write = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

#[test]
fn gatt_read_and_descriptor_operations_have_synchronous_starts() {
    let _: fn(&GattCharacteristic) -> Result<Read> = BLECharacteristic::start_read_value;
    let _: fn(&GattDescriptor) -> Result<Read> = BLEDescriptor::start_read_value;
    let _: fn(&GattDescriptor, &[u8]) -> Result<Write> = BLEDescriptor::start_write_value;
}

#[test]
fn recovery_failure_keeps_exact_owner_for_retry_and_other_peers() {
    use super::super::ble::device::recover_matching;
    let mut owners = vec![(1, false), (2, false)];
    assert_eq!(
        recover_matching(
            &mut owners,
            |owner| owner.0 == 1,
            |_| Err::<(), _>("failed")
        ),
        Err("failed")
    );
    assert_eq!(owners, vec![(1, false), (2, false)]);
    recover_matching(
        &mut owners,
        |owner| owner.0 == 1,
        |owner| {
            owner.1 = true;
            Ok::<_, &str>(())
        },
    )
    .unwrap();
    assert_eq!(owners, vec![(2, false)]);
}

#[test]
fn recovery_claim_serializes_same_address_and_allows_other_peers() {
    use super::super::ble::device::RecoveryLease;
    use crate::api::{AddressType, BDAddr};
    let address = BDAddr::from([61, 2, 3, 4, 5, 6]);
    let other = BDAddr::from([62, 2, 3, 4, 5, 6]);
    let claim = RecoveryLease::acquire(address, Some(AddressType::Public)).unwrap();
    assert!(RecoveryLease::acquire(address, Some(AddressType::Random)).is_err());
    let other_claim = RecoveryLease::acquire(other, None).unwrap();
    drop(claim);
    assert!(RecoveryLease::acquire(address, None).is_ok());
    drop(other_claim);
}

#[test]
fn stale_read_and_descriptor_start_are_refused_after_graph_replacement() {
    use std::sync::{Arc, Mutex, Weak};
    let peer = super::Peripheral::new_with_id(
        Weak::new(),
        super::PeripheralId::from(crate::api::BDAddr::from([1, 2, 3, 4, 5, 6])),
        Arc::new(Mutex::new(Default::default())),
    );
    let mut graph = peer.shared.graph.lock().unwrap();
    graph.ready = true;
    let old = graph.identity().unwrap().unwrap();
    graph.invalidate();
    graph.ready = true;
    for _ in 0..3 {
        let mut initiated = false;
        assert!(super::start_on_current_graph(&graph, &old, || initiated = true).is_err());
        assert!(!initiated);
    }
}
