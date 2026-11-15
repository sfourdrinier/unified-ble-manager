use super::recover_matching;

#[test]
fn failed_owner_and_unattempted_owners_survive_until_retry() {
    let mut owners = vec![(1, 0), (1, 1), (2, 0)];
    let mut calls = Vec::new();
    assert_eq!(
        recover_matching(
            &mut owners,
            |x| x.0 == 1,
            |x| {
                calls.push(*x);
                if x.1 == 1 {
                    Err("remove failed")
                } else {
                    Ok(())
                }
            }
        ),
        Err("remove failed")
    );
    assert_eq!(calls, [(1, 0), (1, 1)]);
    assert_eq!(owners, [(1, 1), (2, 0)]);
    recover_matching(&mut owners, |x| x.0 == 2, |_| Ok::<_, &str>(())).unwrap();
    assert_eq!(owners, [(1, 1)]);
    recover_matching(&mut owners, |_| true, |_| Ok::<_, &str>(())).unwrap();
    assert!(owners.is_empty());
}

#[test]
fn failed_handoff_retains_displaced_owner_after_new_registration() {
    let mut displaced = vec![true];
    let mut new_registered = false;
    let failed = recover_matching(
        &mut displaced,
        |_| true,
        |old_registered| {
            assert!(*old_registered);
            new_registered = true;
            Err::<(), _>("old registration removal failed")
        },
    );
    assert_eq!(failed, Err("old registration removal failed"));
    assert!(new_registered);
    assert_eq!(displaced, [true]);
    recover_matching(
        &mut displaced,
        |_| true,
        |old_registered| {
            *old_registered = false;
            Ok::<_, &str>(())
        },
    )
    .unwrap();
    assert!(displaced.is_empty());
}
