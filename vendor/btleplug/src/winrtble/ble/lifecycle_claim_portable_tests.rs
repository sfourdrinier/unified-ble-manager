use super::RecoveryLease;
use crate::api::{AddressType, BDAddr};
use std::{sync::mpsc, time::Duration};

#[test]
fn blocked_peer_claim_fences_replacement_without_blocking_another_address() {
    let address = BDAddr::from([91, 2, 3, 4, 5, 6]);
    let other = BDAddr::from([92, 2, 3, 4, 5, 6]);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _claim = RecoveryLease::acquire(address, Some(AddressType::Public)).unwrap();
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let same_blocked = RecoveryLease::acquire(address, Some(AddressType::Random)).is_err();
    let unrelated = RecoveryLease::acquire(other, Some(AddressType::Public));
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    assert!(same_blocked);
    assert!(unrelated.is_ok());
    assert!(RecoveryLease::acquire(address, None).is_ok());
}
