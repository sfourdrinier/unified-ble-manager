use super::super::ble::device::{ServiceOwners, recover_matching};
use super::*;
use std::cell::{Cell, RefCell};

#[test]
fn failed_retirement_keeps_displaced_owner_without_adopting_callbacks() {
    // The source is already terminated. The candidate has no registration;
    // a failed old removal retains the exact displaced owner for retry.
    let mut owners = ServiceOwners::default();
    owners.retain_candidate(1);
    owners.publish_candidate();
    owners.retain_candidate(2);
    let installed = Cell::new(false);
    let registrations = RefCell::new(vec![(true, false), (true, false)]);
    let old_services = RefCell::new(vec![1]);
    let handoff = install_and_retire_services(
        &mut owners,
        || {
            installed.set(true);
            Ok(())
        },
        |owner| owner.publish_candidate(),
        || {
            let mut registrations = registrations.borrow_mut();
            registrations[0] = (false, false); // First old handler removed.
            // The second old handler remains when its removal fails.
            Err(Error::Other("old handler removal failed".into()))
        },
        |_| panic!("service closed before registration retirement"),
    );
    assert!(handoff.is_err());
    assert!(installed.get());
    assert_eq!(*registrations.borrow(), [(false, false), (true, false)]);
    let recover = || {
        recover_matching(
            &mut old_services.borrow_mut(),
            |_| true,
            |_| {
                let registrations = registrations.borrow_mut();
                assert_eq!(registrations[1], (true, false));
                Err::<(), _>(Error::Other("cleanup failed".into()))
            },
        )
    };
    assert!(recover().is_err());
    assert_eq!(*old_services.borrow(), [1]);
    // The discovery boundary must not proceed to native Close when recovery fails.
    assert!(
        owners
            .prepare_candidate(recover, |_| panic!("live handler owner closed"))
            .is_err()
    );
    recover_matching(
        &mut old_services.borrow_mut(),
        |_| true,
        |_| {
            registrations.borrow_mut()[1] = (false, false);
            Ok::<_, Error>(())
        },
    )
    .unwrap();
    let mut closed = Vec::new();
    owners
        .prepare_candidate(
            || Ok::<_, Error>(()),
            |service| {
                closed.push(*service);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(closed, [1]);
    assert!(old_services.borrow().is_empty());
    assert_eq!(registrations.borrow()[1], (false, false));
    owners
        .close_all(|service| {
            closed.push(*service);
            Ok::<_, Error>(())
        })
        .unwrap();
    assert_eq!(closed, [1, 2]);
}
