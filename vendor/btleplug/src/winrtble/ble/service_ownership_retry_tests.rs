use super::ServiceOwners;

#[test]
fn repeated_failed_later_queries_retire_each_quiescent_candidate() {
    let mut owners = ServiceOwners::default();
    let mut closed = Vec::new();
    for service in 0..100 {
        owners
            .prepare_candidate(
                || Ok::<_, &str>(()),
                |value| {
                    closed.push(*value);
                    Ok(())
                },
            )
            .unwrap();
        owners.retain_candidate(service);
        // A later query fails; its exact native operation still has to settle.
        assert!(owners
            .prepare_candidate(
                || Err::<(), _>("query pending"),
                |_| panic!("premature close")
            )
            .is_err());
    }
    assert_eq!(closed, (0..99).collect::<Vec<_>>());
    owners
        .prepare_candidate(
            || Ok::<_, &str>(()),
            |value| {
                closed.push(*value);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(closed, (0..100).collect::<Vec<_>>());
}

#[test]
fn installed_candidate_survives_failed_handoff_and_close_retry() {
    let mut owners = ServiceOwners::default();
    owners.retain_candidate(1);
    owners.publish_candidate();
    owners.retain_candidate(2);
    // The replacement table is installed even though registration handoff failed.
    owners.publish_candidate();
    let mut closed = Vec::new();
    assert!(owners
        .prepare_candidate(
            || Err::<(), _>("registration retained"),
            |_| panic!("registration still live")
        )
        .is_err());
    assert!(owners
        .prepare_candidate(
            || Ok::<_, &str>(()),
            |value| {
                closed.push(*value);
                Err("close failed")
            }
        )
        .is_err());
    assert_eq!(closed, [1]);
    owners
        .prepare_candidate(
            || Ok::<_, &str>(()),
            |value| {
                closed.push(*value);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(closed, [1, 1]);
    owners
        .close_all(|value| {
            closed.push(*value);
            Ok::<_, &str>(())
        })
        .unwrap();
    assert_eq!(closed, [1, 1, 2]);
}
