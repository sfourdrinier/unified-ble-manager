//! Mobile ordered records already include failure observations. Their format
//! revision is separate from the desktop transport's record revision.
use serde_json::Value;
use ubm_core::ordered_notifications::{EndReason, Failure, Poll, Record, Terminal};

pub(crate) fn legacy_failure_encodable(failure: &Failure) -> bool {
    let terminal = Terminal {
        reason: EndReason::SourceFailed(failure.clone()),
        tail_uncertain: false,
        rejected_values: None,
        rejected_loss: None,
        source_rejection: None,
        source_failure_member: None,
        secondary_failure: None,
        additional_failure_count: 0,
        failure_count_overflowed: false,
        failure_observations_omitted: false,
    };
    ubm_desktop::ordered_wire::encode_versioned(Poll::Terminal(terminal), 1).is_some()
}

pub(crate) fn encode(poll: Poll) -> Option<Value> {
    let valid = match &poll {
        Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            ..
        }) => legacy_failure_encodable(failure),
        Poll::Terminal(terminal) => {
            let reason = match &terminal.reason {
                EndReason::SetupFailed(failure)
                | EndReason::SourceFailed(failure)
                | EndReason::HostReset(failure) => legacy_failure_encodable(failure),
                _ => true,
            };
            reason
                && terminal
                    .secondary_failure
                    .as_ref()
                    .is_none_or(legacy_failure_encodable)
        }
        _ => true,
    };
    valid
        .then(|| ubm_desktop::ordered_wire::encode_versioned(poll, 2))
        .flatten()
}

pub(crate) fn discard(receipt: &ubm_desktop::central::OrderedDiscardReceipt) -> Option<Value> {
    use ubm_desktop::central::OrderedDiscardEvidence;
    match &receipt.evidence {
        OrderedDiscardEvidence::Loss(record) | OrderedDiscardEvidence::Observation(record) => {
            encode(Poll::Record(record.clone()))?;
        }
        OrderedDiscardEvidence::Terminal(terminal) => {
            encode(Poll::Terminal(terminal.clone()))?;
        }
        _ => {}
    }
    ubm_desktop::ordered_wire::encode_discard_receipt_versioned(receipt, 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ubm_core::ordered_notifications::{FailureMetadataEntry, FailureValue};
    use ubm_desktop::{DesktopError, PlatformDetail, PlatformValue};

    fn poll(failure: Failure) -> Poll {
        Poll::Record(Record::FailureObservation {
            failure: Some(failure),
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: true,
        })
    }

    #[test]
    fn legacy_observations_replay_but_exact_sequences_require_a_new_revision() {
        let error = DesktopError::read_failed("native").with_platform(
            PlatformDetail::new("native", "failed")
                .with_metadata("status", PlatformValue::Int(i64::MIN)),
        );
        let mut failure = ubm_desktop::ordered_notification_groups::failure_from_desktop(&error);
        let expected = encode(poll(failure.clone())).unwrap();
        assert_eq!(expected, encode(poll(failure.clone())).unwrap());
        assert_eq!(
            expected["failure"]["platformMetadata"]["status"]["value"],
            i64::MIN.to_string()
        );
        failure.platform_metadata_entries = Some(vec![
            FailureMetadataEntry {
                key: "__proto__".into(),
                value: FailureValue::Int(i64::MIN),
            },
            FailureMetadataEntry {
                key: "__proto__".into(),
                value: FailureValue::Int(i64::MAX),
            },
        ]);
        assert!(encode(poll(failure)).is_none());
    }
}
