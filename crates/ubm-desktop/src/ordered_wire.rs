// crates/ubm-desktop/src/ordered_wire.rs
//! Lossless native ordered-group record encoding for the N-API boundary.

use serde_json::{Value, json};
use ubm_core::ordered_notifications::{
    EndReason, Failure, FailureValue, LossScope, LossUnit, Membership, Poll, Record, RejectedLoss,
    SourceRejection,
};

fn membership(value: Membership) -> Value {
    json!({"member": value.member.to_string(), "epoch": value.epoch.to_string()})
}
fn scope(value: LossScope) -> Value {
    match value {
        LossScope::Member(id) => json!({"kind":"member", "memberId":id.to_string()}),
        LossScope::Group => json!({"kind":"group"}),
        LossScope::Unknown => json!({"kind":"unknown"}),
    }
}
fn unit(value: LossUnit) -> &'static str {
    match value {
        LossUnit::Values => "values",
        LossUnit::SourceRecords => "source-records",
    }
}
fn loss(
    scope_value: LossScope,
    unit_value: LossUnit,
    count: Option<u64>,
    bytes: Option<u64>,
) -> Value {
    json!({"scope":scope(scope_value), "unit":unit(unit_value), "count":count.map(|x|x.to_string()), "bytes":bytes.map(|x|x.to_string())})
}
pub fn failure(value: &Failure) -> Value {
    let metadata: serde_json::Map<String, Value> = value
        .platform_metadata
        .iter()
        .map(|(key, value)| {
            let value = match value {
                FailureValue::Int(v) => json!({"type":"int", "value":v.to_string()}),
                FailureValue::Text(v) => json!({"type":"text", "value":v}),
                FailureValue::Bool(v) => json!({"type":"bool", "value":v}),
            };
            (key.clone(), value)
        })
        .collect();
    json!({"code":value.code, "domain":value.domain, "operation":value.operation, "detail":value.detail,
        "platformDomain":value.platform_domain, "platformCode":value.platform_code,
        "platformMessage":value.platform_message, "platformMetadata":metadata})
}
/// Lossless marker admission and native-write phase; unresolved initiation is distinct.
pub fn encode_marker_write(status: &crate::boundary::OrderedMarkerWriteStatus) -> Value {
    use crate::boundary::{OrderedMarkerWritePhase as Phase, OrderedMarkerWriteStatus as Status};
    let error = |value: &crate::DesktopError| {
        failure(&crate::ordered_notification_groups::failure_from_desktop(
            value,
        ))
    };
    match status {
        Status::Pending => json!({"state":"pending"}),
        Status::NotAdmitted(cause) => {
            json!({"state":"not-admitted","error":cause.as_ref().map(error)})
        }
        Status::Rejected(cause) => json!({"state":"rejected","error":error(cause)}),
        Status::Admitted { ordinal, write } => {
            let write = match write {
                Phase::Initiating => json!({"state":"initiating"}),
                Phase::NotStarted(cause) => {
                    json!({"state":"not-started","error":cause.as_ref().map(error)})
                }
                Phase::Started(cause) => {
                    json!({"state":"started","error":cause.as_ref().map(error)})
                }
                Phase::Settled(Ok(())) => json!({"state":"settled","error":null}),
                Phase::Settled(Err(cause)) => json!({"state":"settled","error":error(cause)}),
            };
            json!({"state":"admitted","ordinal":ordinal.to_string(),"write":write})
        }
    }
}

/// Encode a marker result under a previously negotiated metadata revision.
pub fn encode_marker_write_versioned(
    status: &crate::boundary::OrderedMarkerWriteStatus,
    version: u8,
) -> Option<Value> {
    use crate::boundary::{OrderedMarkerWritePhase as Phase, OrderedMarkerWriteStatus as Status};
    if !(1..=2).contains(&version) {
        return None;
    }
    let mut encoded = encode_marker_write(status);
    let (cause, nested) = match status {
        Status::NotAdmitted(cause) => (cause.as_ref(), false),
        Status::Rejected(cause) => (Some(cause), false),
        Status::Admitted { write, .. } => (
            match write {
                Phase::NotStarted(cause) | Phase::Started(cause) => cause.as_ref(),
                Phase::Settled(Err(cause)) => Some(cause),
                Phase::Initiating | Phase::Settled(Ok(())) => None,
            },
            true,
        ),
        Status::Pending => (None, false),
    };
    if let Some(cause) = cause {
        let value = crate::ordered_notification_groups::failure_from_desktop(cause);
        let value = failure_versioned(&value, version)?;
        if nested {
            encoded["write"]["error"] = value;
        } else {
            encoded["error"] = value;
        }
    }
    (serde_json::to_vec(&encoded).ok()?.len() <= 1_048_576).then_some(encoded)
}

fn reason(value: &EndReason) -> Value {
    match value {
        EndReason::Closed => json!({"kind":"closed"}),
        EndReason::SetupFailed(error) => json!({"kind":"setup-failed", "error":failure(error)}),
        EndReason::SourceFailed(error) => json!({"kind":"source-failed", "error":failure(error)}),
        EndReason::Invalidated => json!({"kind":"invalidated"}),
        EndReason::ServiceChanged => json!({"kind":"service-changed"}),
        EndReason::ConnectionLost => json!({"kind":"connection-lost"}),
        EndReason::OwnerReleased => json!({"kind":"owner-released"}),
        EndReason::HostReset(error) => json!({"kind":"host-reset", "error":failure(error)}),
        EndReason::CapacityExceeded => json!({"kind":"capacity-exceeded"}),
        EndReason::OrdinalInvalid => json!({"kind":"ordinal-invalid"}),
        EndReason::SourceOverflow => json!({"kind":"source-overflow"}),
        EndReason::SourceOrdinalExhausted => json!({"kind":"ordinal-exhausted"}),
    }
}
fn rejection(value: SourceRejection) -> Value {
    match value {
        SourceRejection::Value { bytes } => json!({"kind":"value", "bytes":bytes.to_string()}),
        SourceRejection::Loss { skipped_records } => {
            json!({"kind":"loss", "skippedRecords":skipped_records.to_string()})
        }
        SourceRejection::Fence { membership: m } => {
            json!({"kind":"fence", "membership":membership(m)})
        }
        SourceRejection::Barrier { id } => json!({"kind":"barrier", "id":id.to_string()}),
        SourceRejection::Marker { marker_id } => {
            json!({"kind":"marker", "markerId":marker_id.to_string()})
        }
        SourceRejection::OrdinalExhausted { bytes } => {
            json!({"kind":"ordinal-exhausted", "bytes":bytes.to_string()})
        }
    }
}
fn rejected_loss(value: RejectedLoss) -> Value {
    loss(value.scope, value.unit, value.count, value.bytes)
}

pub fn encode(poll: Poll) -> Value {
    match poll {
        Poll::Empty => json!({"kind":"empty"}),
        Poll::Closed => json!({"kind":"closed"}),
        Poll::Record(Record::Value {
            ordinal,
            member,
            membership: source_membership,
            value,
            delivery,
            provenance,
        }) => {
            let delivery = match delivery {
                ubm_core::ordered_notifications::ObservedDelivery::Notification => "notification",
                ubm_core::ordered_notifications::ObservedDelivery::Indication => "indication",
                ubm_core::ordered_notifications::ObservedDelivery::Unknown => "unknown",
            };
            json!({"kind":"value", "ordinal":ordinal.to_string(), "memberId":member.to_string(), "sourceMembership":membership(source_membership), "bytes":value, "delivery":delivery, "provenance":provenance.as_str()})
        }
        Poll::Record(Record::Loss {
            scope: s,
            unit: u,
            count,
            bytes,
        }) => json!({"kind":"loss", "loss":loss(s,u,count,bytes)}),
        Poll::Record(Record::Marker { marker_id, ordinal }) => {
            json!({"kind":"marker", "markerId":marker_id.to_string(), "ordinal":ordinal.to_string()})
        }
        Poll::Record(Record::FailureObservation {
            failure: observed_failure,
            source_rejection,
            source_failure_member,
            rejected_loss: rejected,
            tail_uncertain,
        }) => {
            json!({"kind":"failure-observation", "failure":observed_failure.as_ref().map(failure), "sourceRejection":source_rejection.map(rejection), "sourceFailureMember":source_failure_member.map(membership), "rejectedLoss":rejected.map(rejected_loss), "tailUncertain":tail_uncertain})
        }
        Poll::Record(Record::FailureObservationsOmitted { count }) => {
            json!({"kind":"failure-observations-omitted", "count":count.map(|value| value.to_string())})
        }
        Poll::Terminal(terminal) => {
            json!({"kind":"terminal", "reason":reason(&terminal.reason), "uncertainTail":terminal.tail_uncertain,
            "rejectedValues":terminal.rejected_values.map(|x|x.to_string()), "rejectedLoss":terminal.rejected_loss.map(rejected_loss),
            "sourceRejection":terminal.source_rejection.map(rejection), "sourceFailureMember":terminal.source_failure_member.map(membership), "secondaryFailure":terminal.secondary_failure.as_ref().map(failure), "additionalFailureCount":terminal.additional_failure_count.to_string(), "failureSummaryVersion":1, "failureCountOverflowed":terminal.failure_count_overflowed, "failureObservationsOmitted":terminal.failure_observations_omitted})
        }
    }
}

/// Encode only for an explicitly selected metadata revision. Callers must
/// negotiate the revision before opening a source and retain a delivery when
/// this returns `None`; the legacy transport has not yet adopted this API.
pub fn encode_versioned(poll: Poll, version: u8) -> Option<Value> {
    if !(1..=2).contains(&version) {
        return None;
    }
    if version == 1
        && matches!(
            &poll,
            Poll::Record(
                Record::FailureObservation { .. } | Record::FailureObservationsOmitted { .. }
            )
        )
    {
        return None;
    }
    let mut encoded = encode(poll.clone());
    match &poll {
        Poll::Record(Record::FailureObservation {
            failure: Some(cause),
            ..
        }) => {
            encoded["failure"] = failure_versioned(cause, version)?;
        }
        Poll::Terminal(terminal) => {
            match &terminal.reason {
                EndReason::SetupFailed(cause)
                | EndReason::SourceFailed(cause)
                | EndReason::HostReset(cause) => {
                    encoded["reason"]["error"] = failure_versioned(cause, version)?;
                }
                _ => {}
            }
            if let Some(cause) = &terminal.secondary_failure {
                encoded["secondaryFailure"] = failure_versioned(cause, version)?;
            }
        }
        _ => {}
    }
    (serde_json::to_vec(&encoded).ok()?.len() <= 1_048_576).then_some(encoded)
}

pub fn failure_versioned(value: &Failure, version: u8) -> Option<Value> {
    if !crate::ordered_recording::failure_is_readable(value, version) {
        return None;
    }
    let mut encoded = failure(value);
    if version == 2 {
        let extended = crate::ordered_recording::failure_v2(value);
        for key in [
            "platformMetadataEntries",
            "platformCleanup",
            "platformCleanupFailures",
        ] {
            if let Some(field) = extended.get(key) {
                encoded[key] = field.clone();
            }
        }
    }
    Some(encoded)
}

/// Encode a sealed per-delivery discard receipt without exposing value bytes.
pub fn encode_discard_receipt(receipt: &crate::central::OrderedDiscardReceipt) -> Value {
    use crate::central::OrderedDiscardEvidence;
    let evidence = match &receipt.evidence {
        OrderedDiscardEvidence::Value {
            member,
            ordinal,
            membership: source_membership,
            bytes,
        } => {
            json!({"kind":"value", "memberId":member.to_string(), "ordinal":ordinal.to_string(), "sourceMembership":membership(*source_membership), "bytes":bytes.to_string()})
        }
        OrderedDiscardEvidence::Loss(record) | OrderedDiscardEvidence::Observation(record) => {
            encode(Poll::Record(record.clone()))
        }
        OrderedDiscardEvidence::Marker { marker_id, ordinal } => {
            json!({"kind":"marker", "markerId":marker_id.to_string(), "ordinal":ordinal.to_string()})
        }
        OrderedDiscardEvidence::Terminal(terminal) => encode(Poll::Terminal(terminal.clone())),
    };
    json!({"receiptId":receipt.receipt_id.to_string(), "discardedDeliveryId":receipt.discarded_delivery_id.to_string(), "evidence":evidence})
}

/// Retain a receipt when its evidence cannot be represented by the selected revision.
pub fn encode_discard_receipt_versioned(
    receipt: &crate::central::OrderedDiscardReceipt,
    version: u8,
) -> Option<Value> {
    use crate::central::OrderedDiscardEvidence;
    let evidence = match &receipt.evidence {
        OrderedDiscardEvidence::Value { .. } | OrderedDiscardEvidence::Marker { .. } => {
            if !(1..=2).contains(&version) {
                return None;
            }
            encode_discard_receipt(receipt)["evidence"].clone()
        }
        OrderedDiscardEvidence::Loss(record) | OrderedDiscardEvidence::Observation(record) => {
            encode_versioned(Poll::Record(record.clone()), version)?
        }
        OrderedDiscardEvidence::Terminal(terminal) => {
            encode_versioned(Poll::Terminal(terminal.clone()), version)?
        }
    };
    let encoded = json!({"receiptId":receipt.receipt_id.to_string(), "discardedDeliveryId":receipt.discarded_delivery_id.to_string(), "evidence":evidence});
    (serde_json::to_vec(&encoded).ok()?.len() <= 1_048_576).then_some(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_wire_rejects_new_observation_records() {
        let observation = Poll::Record(Record::FailureObservation {
            failure: None,
            source_rejection: Some(SourceRejection::Barrier { id: 7 }),
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: true,
        });
        assert!(encode_versioned(observation.clone(), 1).is_none());
        assert_eq!(
            encode_versioned(observation, 2).unwrap()["sourceRejection"]["id"],
            "7"
        );
        assert!(
            encode_versioned(
                Poll::Record(Record::FailureObservationsOmitted { count: None }),
                1
            )
            .is_none()
        );
    }
    #[test]
    fn discard_receipt_preserves_observation_and_requires_v2() {
        let observation = Record::FailureObservation {
            failure: None,
            source_rejection: Some(SourceRejection::Barrier { id: 7 }),
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: true,
        };
        let receipt = crate::central::OrderedDiscardReceipt {
            receipt_id: 8,
            discarded_delivery_id: 9,
            evidence: crate::central::OrderedDiscardEvidence::Observation(observation),
        };
        assert!(encode_discard_receipt_versioned(&receipt, 1).is_none());
        let encoded = encode_discard_receipt_versioned(&receipt, 2).unwrap();
        assert_eq!(encoded["evidence"]["sourceRejection"]["id"], "7");
        assert_eq!(encoded["evidence"]["tailUncertain"], true);
    }
    #[test]
    fn versioned_wire_refuses_legacy_loss_and_retains_independent_causes() {
        use crate::errors::{DesktopError, PlatformDetail, PlatformMetadataEntry, PlatformValue};
        let mut primary = PlatformDetail::new("source", "failed");
        primary.metadata_entries = Some(vec![
            PlatformMetadataEntry {
                key: "constructor".into(),
                value: PlatformValue::Int(i64::MIN),
            },
            PlatformMetadataEntry {
                key: "constructor".into(),
                value: PlatformValue::Int(i64::MAX),
            },
        ]);
        let mut cleanup = PlatformDetail::new("cleanup", "refused");
        cleanup.metadata_entries = Some(Vec::new());
        primary.cleanup.push(cleanup);
        let error = DesktopError::read_failed("read").with_platform(primary);
        let failure = crate::ordered_notification_groups::failure_from_desktop(&error);
        let poll = Poll::Record(Record::FailureObservation {
            failure: Some(failure.clone()),
            source_rejection: None,
            source_failure_member: None,
            rejected_loss: None,
            tail_uncertain: false,
        });
        assert!(encode_versioned(poll.clone(), 1).is_none());
        let value = encode_versioned(poll, 2).unwrap();
        assert_eq!(
            value["failure"]["platformMetadataEntries"][0]["value"]["value"],
            i64::MIN.to_string()
        );
        assert_eq!(
            value["failure"]["platformMetadataEntries"][1]["value"]["value"],
            i64::MAX.to_string()
        );
        assert_eq!(value["failure"]["platformCleanup"][0]["domain"], "cleanup");
        assert_eq!(
            value["failure"]["platformCleanup"][0]["metadataEntries"],
            json!([])
        );
        let terminal = ubm_core::ordered_notifications::Terminal {
            reason: EndReason::SetupFailed(failure.clone()),
            tail_uncertain: false,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: None,
            source_failure_member: None,
            secondary_failure: Some(failure),
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
        };
        assert!(encode_versioned(Poll::Terminal(terminal.clone()), 1).is_none());
        let terminal = encode_versioned(Poll::Terminal(terminal), 2).unwrap();
        assert_eq!(
            terminal["reason"]["error"]["platformCleanup"][0]["code"],
            "refused"
        );
        assert_eq!(
            terminal["secondaryFailure"]["platformMetadataEntries"][1]["value"]["value"],
            i64::MAX.to_string()
        );
    }
    #[test]
    fn versioned_marker_refuses_legacy_loss_and_keeps_exact_entries() {
        use crate::errors::{DesktopError, PlatformDetail, PlatformMetadataEntry, PlatformValue};
        let mut platform = PlatformDetail::new("native", "failed");
        platform.metadata_entries = Some(vec![PlatformMetadataEntry {
            key: "prototype".into(),
            value: PlatformValue::Int(i64::MIN),
        }]);
        let status = crate::boundary::OrderedMarkerWriteStatus::Rejected(
            DesktopError::read_failed("write").with_platform(platform),
        );
        assert!(encode_marker_write_versioned(&status, 1).is_none());
        let value = encode_marker_write_versioned(&status, 2).unwrap();
        assert_eq!(
            value["error"]["platformMetadataEntries"][0]["key"],
            "prototype"
        );
    }
    #[test]
    fn failure_observation_wire_keeps_each_field_and_unknown_omission_count() {
        let observation = Record::FailureObservation {
            failure: Some(Failure {
                code: "platform.failure".into(),
                domain: "gatt".into(),
                operation: "seal".into(),
                detail: None,
                platform_domain: None,
                platform_code: None,
                platform_message: None,
                platform_metadata: [("native".into(), FailureValue::Int(i64::MAX))].into(),
                platform_metadata_entries: None,
                platform_cleanup: Vec::new(),
                platform_cleanup_failures: Vec::new(),
            }),
            source_rejection: Some(SourceRejection::Fence {
                membership: Membership {
                    member: 7,
                    epoch: u64::MAX,
                },
            }),
            source_failure_member: Some(Membership {
                member: 9,
                epoch: 4,
            }),
            rejected_loss: Some(RejectedLoss {
                scope: LossScope::Member(3),
                unit: LossUnit::Values,
                count: Some(u64::MAX),
                bytes: None,
            }),
            tail_uncertain: true,
        };
        assert_eq!(
            encode(Poll::Record(observation)),
            json!({
                "kind":"failure-observation", "failure":{"code":"platform.failure","domain":"gatt","operation":"seal","detail":null,"platformDomain":null,"platformCode":null,"platformMessage":null,"platformMetadata":{"native":{"type":"int","value":i64::MAX.to_string()}}},
                "sourceRejection":{"kind":"fence","membership":{"member":"7","epoch":u64::MAX.to_string()}},
                "sourceFailureMember":{"member":"9","epoch":"4"},
                "rejectedLoss":{"scope":{"kind":"member","memberId":"3"},"unit":"values","count":u64::MAX.to_string(),"bytes":null},
                "tailUncertain":true
            })
        );
        assert_eq!(
            encode(Poll::Record(Record::FailureObservationsOmitted {
                count: None
            })),
            json!({"kind":"failure-observations-omitted","count":null})
        );
    }
    #[test]
    fn marker_wire_and_discard_preserve_full_width_values() {
        let marker = encode(Poll::Record(Record::Marker {
            marker_id: u64::MAX,
            ordinal: u64::MAX,
        }));
        assert_eq!(marker["markerId"], "18446744073709551615");
        assert_eq!(marker["ordinal"], "18446744073709551615");
        let rejected = rejection(SourceRejection::Marker {
            marker_id: u64::MAX,
        });
        assert_eq!(rejected["markerId"], "18446744073709551615");
        let receipt = crate::central::OrderedDiscardReceipt {
            receipt_id: 1,
            discarded_delivery_id: 2,
            evidence: crate::central::OrderedDiscardEvidence::Marker {
                marker_id: u64::MAX,
                ordinal: u64::MAX,
            },
        };
        assert_eq!(encode_discard_receipt(&receipt)["evidence"], marker);
    }
    #[test]
    fn marker_ordinal_exhaustion_preserves_marker_rejection_in_terminal_and_discard() {
        let terminal = ubm_core::ordered_notifications::Terminal {
            reason: EndReason::SourceOrdinalExhausted,
            tail_uncertain: false,
            rejected_values: None,
            rejected_loss: None,
            source_rejection: Some(SourceRejection::Marker {
                marker_id: u64::MAX,
            }),
            additional_failure_count: 0,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
            secondary_failure: None,
            source_failure_member: None,
        };
        let encoded = encode(Poll::Terminal(terminal.clone()));
        assert_eq!(encoded["reason"]["kind"], "ordinal-exhausted");
        assert_eq!(
            encoded["sourceRejection"]["markerId"],
            "18446744073709551615"
        );
        let receipt = crate::central::OrderedDiscardReceipt {
            receipt_id: 1,
            discarded_delivery_id: 2,
            evidence: crate::central::OrderedDiscardEvidence::Terminal(terminal),
        };
        assert_eq!(encode_discard_receipt(&receipt)["evidence"], encoded);
    }
    #[test]
    fn marker_initiating_is_distinct_from_started_and_not_started() {
        use crate::boundary::{OrderedMarkerWritePhase, OrderedMarkerWriteStatus};
        let encoded = encode_marker_write(&OrderedMarkerWriteStatus::Admitted {
            ordinal: u64::MAX,
            write: OrderedMarkerWritePhase::Initiating,
        });
        assert_eq!(encoded["ordinal"], "18446744073709551615");
        assert_eq!(encoded["write"]["state"], "initiating");
    }
    #[test]
    fn terminal_retains_rejected_control_and_source_membership() {
        let terminal = ubm_core::ordered_notifications::Terminal {
            reason: EndReason::SourceOverflow,
            tail_uncertain: true,
            rejected_values: Some(u64::MAX),
            rejected_loss: Some(RejectedLoss {
                scope: LossScope::Unknown,
                unit: LossUnit::SourceRecords,
                count: None,
                bytes: Some(u64::MAX),
            }),
            source_rejection: Some(SourceRejection::Fence {
                membership: Membership {
                    member: u64::MAX,
                    epoch: 3,
                },
            }),
            additional_failure_count: u64::MAX,
            failure_count_overflowed: false,
            failure_observations_omitted: false,
            secondary_failure: Some(Failure {
                code: "platform.failure".into(),
                domain: "gatt".into(),
                operation: "seal".into(),
                detail: None,
                platform_domain: None,
                platform_code: None,
                platform_message: None,
                platform_metadata: [("native".into(), FailureValue::Int(i64::MAX))].into(),
                platform_metadata_entries: None,
                platform_cleanup: Vec::new(),
                platform_cleanup_failures: Vec::new(),
            }),
            source_failure_member: Some(Membership {
                member: 4,
                epoch: u64::MAX,
            }),
        };
        let reset = super::reason(&EndReason::HostReset(
            terminal.secondary_failure.clone().unwrap(),
        ));
        assert_eq!(reset["kind"], "host-reset");
        assert_eq!(reset["error"]["operation"], "seal");
        let wire = encode(Poll::Terminal(terminal));
        assert_eq!(wire["additionalFailureCount"], u64::MAX.to_string());
        assert_eq!(wire["secondaryFailure"]["operation"], "seal");
        assert_eq!(
            wire["secondaryFailure"]["platformMetadata"]["native"]["value"],
            i64::MAX.to_string()
        );
        assert_eq!(wire["rejectedValues"], "18446744073709551615");
        assert!(wire["rejectedLoss"]["count"].is_null());
        assert_eq!(wire["rejectedLoss"]["bytes"], "18446744073709551615");
        assert_eq!(
            wire["sourceRejection"]["membership"]["member"],
            "18446744073709551615"
        );
        assert_eq!(wire["sourceFailureMember"]["epoch"], "18446744073709551615");
    }

    #[test]
    fn lifecycle_causes_remain_distinct_on_wire() {
        for (reason, expected) in [
            (EndReason::ServiceChanged, "service-changed"),
            (EndReason::ConnectionLost, "connection-lost"),
            (EndReason::OwnerReleased, "owner-released"),
        ] {
            assert_eq!(super::reason(&reason)["kind"], expected);
        }
    }

    #[test]
    fn full_width_ordinal_and_unattributed_loss_remain_exact() {
        let value = encode(Poll::Record(Record::Value {
            ordinal: u64::MAX,
            member: 4,
            membership: Membership {
                member: 4,
                epoch: u64::MAX,
            },
            value: vec![0, 255],
            delivery: ubm_core::ordered_notifications::ObservedDelivery::Unknown,
            provenance: ubm_core::central::NotificationProvenance::Unknown,
        }));
        assert_eq!(value["ordinal"], "18446744073709551615");
        assert_eq!(value["bytes"], json!([0, 255]));
        let loss = encode(Poll::Record(Record::Loss {
            scope: LossScope::Unknown,
            unit: LossUnit::SourceRecords,
            count: Some(u64::MAX),
            bytes: None,
        }));
        assert_eq!(loss["loss"]["count"], "18446744073709551615");
        assert!(loss["loss"]["bytes"].is_null());
    }
}
