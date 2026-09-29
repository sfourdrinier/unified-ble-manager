#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_smallest_actual_subscriber_budget() {
        assert_eq!(payload_capacity([]).unwrap(), None);
        assert_eq!(payload_capacity([244, 20, 182]).unwrap(), Some(20));
        assert!(payload_capacity([244, 0]).is_err());
    }

    #[test]
    fn no_subscribers_and_backpressure_never_count_as_accepted() {
        assert_eq!(
            corebluetooth_outcome(false, false),
            NotificationOutcome::NotSubscribed
        );
        assert_eq!(
            corebluetooth_outcome(true, false),
            NotificationOutcome::Backpressured
        );
        assert_eq!(
            corebluetooth_outcome(true, true),
            NotificationOutcome::Accepted
        );
    }

    #[test]
    fn every_windows_recipient_must_accept() {
        assert_eq!(
            recipient_outcome(Vec::<Result<(), String>>::new()).unwrap(),
            NotificationOutcome::NotSubscribed
        );
        assert_eq!(
            recipient_outcome([Ok(())]).unwrap(),
            NotificationOutcome::Accepted
        );
        let error = recipient_outcome([
            Ok(()),
            Err("client b: unreachable".into()),
            Err("client c: protocol 5".into()),
        ])
        .unwrap_err();
        assert!(error.contains("client b: unreachable"));
        assert!(error.contains("client c: protocol 5"));
    }
}
/// Acceptance is an OS result, not proof of receipt by the central.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationOutcome {
    NotSubscribed,
    Accepted,
    Backpressured,
}

pub fn payload_capacity(values: impl IntoIterator<Item = usize>) -> Result<Option<usize>, String> {
    let mut minimum = None;
    for value in values {
        if value == 0 {
            return Err("subscriber reported zero notification capacity".into());
        }
        minimum = Some(minimum.map_or(value, |previous: usize| previous.min(value)));
    }
    Ok(minimum)
}

pub fn corebluetooth_outcome(subscribed: bool, accepted: bool) -> NotificationOutcome {
    if !subscribed {
        NotificationOutcome::NotSubscribed
    } else if accepted {
        NotificationOutcome::Accepted
    } else {
        NotificationOutcome::Backpressured
    }
}

pub fn recipient_outcome(
    results: impl IntoIterator<Item = Result<(), String>>,
) -> Result<NotificationOutcome, String> {
    let mut count = 0;
    let mut failures = Vec::new();
    for result in results {
        count += 1;
        if let Err(error) = result {
            failures.push(error);
        }
    }
    if !failures.is_empty() {
        Err(failures.join("; "))
    } else if count == 0 {
        Ok(NotificationOutcome::NotSubscribed)
    } else {
        Ok(NotificationOutcome::Accepted)
    }
}
