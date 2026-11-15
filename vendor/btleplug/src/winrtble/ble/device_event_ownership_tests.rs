#[path = "device_event_ownership.rs"]
mod ownership;

use ownership::{Event, EventTokens, close_once, retain_failed_drop};

#[test]
fn session_and_device_closes_preserve_progress_across_failure() {
    let mut session_closed = false;
    let mut device_closed = false;
    close_once(&mut session_closed, || Ok::<_, &str>(())).unwrap();
    assert_eq!(
        close_once(&mut device_closed, || Err("close failed")),
        Err("close failed")
    );
    close_once(&mut session_closed, || -> Result<(), &str> {
        panic!("session close repeated")
    })
    .unwrap();
    close_once(&mut device_closed, || Ok::<_, &str>(())).unwrap();
    close_once(&mut device_closed, || -> Result<(), &str> {
        panic!("device close repeated")
    })
    .unwrap();
}

#[test]
fn every_partial_registration_rolls_back_and_failed_removal_is_retryable() {
    let events = [
        Event::Connection,
        Event::Pdu,
        Event::Parameters,
        Event::Services,
    ];
    for failed_registration in 0..=events.len() {
        let mut tokens = EventTokens::default();
        for (index, event) in events.iter().copied().enumerate().take(failed_registration) {
            tokens.record(event, index as i64 + 1);
        }
        if failed_registration > 0 {
            let mut calls = Vec::new();
            assert_eq!(
                tokens.remove_all(|event, token| {
                    calls.push((event, token));
                    Err::<(), _>("removal failed")
                }),
                Err("removal failed")
            );
            let failed = calls[0];
            let mut retried = Vec::new();
            tokens
                .remove_all(|event, token| {
                    retried.push((event, token));
                    Ok::<_, &str>(())
                })
                .unwrap();
            assert_eq!(retried[0], failed);
            assert_eq!(retried.len(), failed_registration);
        }
        tokens
            .remove_all(|_, _| -> Result<(), &str> { panic!("already removed") })
            .unwrap();
    }
}

#[test]
fn retry_does_not_repeat_removals_that_succeeded_before_a_failure() {
    let mut tokens = EventTokens::default();
    tokens.record(Event::Connection, 1);
    tokens.record(Event::Pdu, 2);
    tokens.record(Event::Parameters, 3);
    tokens.record(Event::Services, 4);
    let mut first = Vec::new();
    assert_eq!(
        tokens.remove_all(|event, _| {
            first.push(event);
            if event == Event::Parameters {
                Err("failed")
            } else {
                Ok(())
            }
        }),
        Err("failed")
    );
    let mut retry = Vec::new();
    tokens
        .remove_all(|event, _| {
            retry.push(event);
            Ok::<_, &str>(())
        })
        .unwrap();
    assert_eq!(first, [Event::Pdu, Event::Parameters]);
    assert_eq!(
        retry,
        [Event::Parameters, Event::Services, Event::Connection]
    );
}

#[test]
fn each_removal_failure_retains_that_token_and_every_later_token() {
    let order = [
        Event::Pdu,
        Event::Parameters,
        Event::Services,
        Event::Connection,
    ];
    for failure_index in 0..order.len() {
        let mut tokens = EventTokens::default();
        for (index, event) in order.iter().copied().enumerate() {
            tokens.record(event, index as i64);
        }
        let mut first = Vec::new();
        assert_eq!(
            tokens.remove_all(|event, _| {
                first.push(event);
                if event == order[failure_index] {
                    Err("failure")
                } else {
                    Ok(())
                }
            }),
            Err("failure")
        );
        let mut retry = Vec::new();
        tokens
            .remove_all(|event, token| {
                retry.push((event, token));
                Ok::<_, &str>(())
            })
            .unwrap();
        assert_eq!(first, order[..=failure_index]);
        assert_eq!(
            retry,
            order[failure_index..]
                .iter()
                .copied()
                .enumerate()
                .map(|(i, event)| (event, (i + failure_index) as i64))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn drop_transfers_the_failed_owner_and_retry_removes_the_same_token() {
    use std::{cell::RefCell, rc::Rc};
    struct Owner {
        tokens: EventTokens,
        retained: Rc<RefCell<Vec<EventTokens>>>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            retain_failed_drop(
                self,
                |owner| owner.tokens.remove_all(|_, _| Err::<(), _>("failed")),
                |owner, error| {
                    assert_eq!(error, "failed");
                    owner
                        .retained
                        .borrow_mut()
                        .push(std::mem::take(&mut owner.tokens));
                },
            );
        }
    }
    let retained = Rc::new(RefCell::new(Vec::new()));
    let mut owner = Owner {
        tokens: EventTokens::default(),
        retained: retained.clone(),
    };
    owner.tokens.record(Event::Pdu, 42);
    drop(owner);
    let mut recovered = retained.borrow_mut().pop().expect("failed owner retained");
    recovered
        .remove_all(|event, token| {
            assert_eq!((event, token), (Event::Pdu, 42));
            Ok::<_, &str>(())
        })
        .unwrap();
    recovered
        .remove_all(|_, _| -> Result<(), &str> { panic!("already removed") })
        .unwrap();
}
