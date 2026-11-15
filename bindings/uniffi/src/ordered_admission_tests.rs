#![cfg(test)]

use super::*;

fn identity() -> ubm_mobile::OrderedOpenIdentity {
    ubm_mobile::OrderedOpenIdentity {
        version: 2,
        owner_incarnation: ubm_mobile::OrderedAdmissionOwner::negotiate_slot(
            2,
            "12345678-1234-4abc-8def-123456789abc",
        )
        .unwrap(),
        slot: 63,
        slot_generation: u64::MAX,
        original_request_id: u64::MAX,
        native_group_id: 1 << 63,
        peer_id: "Peer".into(),
        spec: ubm_mobile::OrderedOpenSpec {
            connection_generation: "connection".into(),
            database_generation: "database".into(),
            snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "owner".into(),
                attachment: u64::MAX,
                revision: 1 << 63,
            },
            members: vec![ubm_mobile::OrderedMember {
                member_id: u64::MAX,
                instance: Instance {
                    peer_id: "Peer".into(),
                    service_uuid: "service".into(),
                    service_occurrence: u64::MAX,
                    characteristic_uuid: "characteristic".into(),
                    characteristic_occurrence: 1 << 63,
                },
            }],
            item_capacity: u32::MAX,
            byte_capacity: 42,
        },
    }
}

#[test]
fn slot_identity_and_spec_use_checked_lossless_maps() {
    let original = identity();
    let wire = MobileOrderedOpenIdentity::from(&original);
    assert_eq!(
        ubm_mobile::OrderedOpenIdentity::try_from(wire.clone()).unwrap(),
        original
    );
    let mut invalid = wire.clone();
    invalid.spec.members[0].instance.peer_id = "peer".into();
    assert!(ubm_mobile::OrderedOpenIdentity::try_from(invalid).is_err());
    let mut invalid = wire.clone();
    invalid.owner_incarnation = "not-a-uuid".into();
    assert!(ubm_mobile::OrderedOpenIdentity::try_from(invalid).is_err());
    let mut invalid = wire;
    invalid.slot = 64;
    assert!(ubm_mobile::OrderedOpenIdentity::try_from(invalid).is_err());
}

#[test]
fn result_and_retirement_preserve_slot_and_original_failure() {
    let identity = identity();
    let failure = PlatformFailure {
        kind: FailureKind::Busy,
        gatt_status: Some(7),
        native_domain: Some("domain".into()),
        native_code: Some(-5),
        native_name: Some("name".into()),
        detail: "original".into(),
        dispatched: false,
    };
    let completion = ubm_mobile::OrderedOpenCompletionV2::Result(ubm_mobile::OrderedOpenResultV2 {
        slot_identity: identity.slot_identity(),
        outcome: ubm_mobile::OrderedOpenOutcomeV2::Failed {
            original_failure: failure,
            ownership: ubm_mobile::OrderedOpenOwnershipV2::Created {
                source_generation: OrderedSourceGeneration::Counter(u64::MAX),
                members: vec![OrderedRegistration {
                    member_id: u64::MAX,
                    source_member: 9,
                    epoch: 4,
                }],
            },
        },
    });
    let wire = MobileOrderedOpenCompletion::from(&completion);
    assert_eq!(
        ubm_mobile::OrderedOpenCompletionV2::try_from(wire).unwrap(),
        completion
    );
    let retired = ubm_mobile::OrderedOpenCompletionV2::Retired(ubm_mobile::OrderedOpenRetired {
        slot_identity: identity.slot_identity(),
    });
    assert_eq!(
        ubm_mobile::OrderedOpenCompletionV2::try_from(MobileOrderedOpenCompletion::from(&retired))
            .unwrap(),
        retired
    );
}

#[test]
fn all_v2_requests_roundtrip_and_check_attempt_identity() {
    let identity = identity();
    let requests = [
        ubm_mobile::OrderedOpenRequestV2::Open {
            id: identity.original_request_id,
            identity: identity.clone(),
        },
        ubm_mobile::OrderedOpenRequestV2::Recover {
            id: 7,
            identity: identity.clone(),
        },
        ubm_mobile::OrderedOpenRequestV2::Acknowledge {
            id: 8,
            identity: identity.clone(),
        },
    ];
    for request in requests {
        let wire = MobileRadioRequest::from(&request);
        let mut bytes = Vec::new();
        <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            wire.clone(),
            &mut bytes,
        );
        let mut input = bytes.as_slice();
        let decoded =
            <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(&mut input)
                .unwrap();
        assert!(input.is_empty());
        assert_eq!(decoded, wire);
        assert_eq!(
            MobileRadioRequest::from(&RadioRequest::OrderedOpenV2(request.clone())),
            wire
        );
        assert_eq!(
            ubm_mobile::OrderedOpenRequestV2::try_from(decoded).unwrap(),
            request
        );
    }
    let invalid = MobileRadioRequest::OrderedOpenV2 {
        id: 1,
        identity: (&identity).into(),
    };
    assert!(ubm_mobile::OrderedOpenRequestV2::try_from(invalid).is_err());
}

#[test]
fn outcomes_and_retirement_cross_the_generated_codec() {
    let slot = identity().slot_identity();
    let values = [
        ubm_mobile::OrderedOpenCompletionV2::Result(ubm_mobile::OrderedOpenResultV2 {
            slot_identity: slot,
            outcome: ubm_mobile::OrderedOpenOutcomeV2::Pending,
        }),
        ubm_mobile::OrderedOpenCompletionV2::Result(ubm_mobile::OrderedOpenResultV2 {
            slot_identity: slot,
            outcome: ubm_mobile::OrderedOpenOutcomeV2::Opened {
                source_generation: OrderedSourceGeneration::Opaque("source".into()),
                members: vec![],
            },
        }),
        ubm_mobile::OrderedOpenCompletionV2::Result(ubm_mobile::OrderedOpenResultV2 {
            slot_identity: slot,
            outcome: ubm_mobile::OrderedOpenOutcomeV2::Indeterminate {
                original_failure_or_reason: ubm_mobile::OrderedOpenIndeterminateReason::Reason(
                    "uncertain".into(),
                ),
            },
        }),
        ubm_mobile::OrderedOpenCompletionV2::Retired(ubm_mobile::OrderedOpenRetired {
            slot_identity: slot,
        }),
    ];
    for value in values {
        let wire = MobileOrderedOpenCompletion::from(&value);
        let mut bytes = Vec::new();
        <MobileOrderedOpenCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            wire, &mut bytes,
        );
        let mut input = bytes.as_slice();
        let decoded =
            <MobileOrderedOpenCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
                &mut input,
            )
            .unwrap();
        assert!(input.is_empty());
        assert_eq!(
            ubm_mobile::OrderedOpenCompletionV2::try_from(decoded).unwrap(),
            value
        );
        assert_eq!(
            ubm_mobile::OrderedOpenCompletionV2::try_from(MobileRadioCompletion::from(&value))
                .unwrap(),
            value
        );
    }
    assert!(
        ubm_mobile::OrderedOpenCompletionV2::try_from(MobileOrderedOpenCompletion::Retired {
            slot_identity: MobileOrderedSlotIdentity {
                owner_incarnation: "invalid".into(),
                slot: 0,
                slot_generation: 1
            }
        })
        .is_err()
    );
}

#[test]
fn v2_radio_envelopes_reach_core_and_legacy_completion_is_not_retirement() {
    let identity = identity();
    for request in [
        ubm_mobile::OrderedOpenRequestV2::Open {
            id: identity.original_request_id,
            identity: identity.clone(),
        },
        ubm_mobile::OrderedOpenRequestV2::Recover {
            id: 7,
            identity: identity.clone(),
        },
        ubm_mobile::OrderedOpenRequestV2::Acknowledge {
            id: 8,
            identity: identity.clone(),
        },
    ] {
        let wire = MobileRadioRequest::from(&request);
        let mut bytes = Vec::new();
        <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::write(
            wire.clone(),
            &mut bytes,
        );
        let mut input = bytes.as_slice();
        let decoded =
            <MobileRadioRequest as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(&mut input)
                .unwrap();
        assert!(input.is_empty());
        assert_eq!(decoded, wire);
        assert_eq!(
            MobileRadioRequest::from(&RadioRequest::OrderedOpenV2(request.clone())),
            wire
        );
        assert_eq!(
            ubm_mobile::OrderedOpenRequestV2::try_from(decoded).unwrap(),
            request
        );
    }
    let wire = MobileRadioCompletion::OrderedOpenRetiredV2 {
        slot_identity: identity.slot_identity().into(),
    };
    let mut bytes = Vec::new();
    <MobileRadioCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::write(
        wire.clone(),
        &mut bytes,
    );
    let decoded = <MobileRadioCompletion as uniffi::FfiConverter<crate::UniFfiTag>>::try_read(
        &mut bytes.as_slice(),
    )
    .unwrap();
    assert_eq!(decoded, wire);
    assert_eq!(
        ubm_mobile::OrderedOpenCompletionV2::try_from(decoded.clone()).unwrap(),
        ubm_mobile::OrderedOpenCompletionV2::Retired(ubm_mobile::OrderedOpenRetired {
            slot_identity: identity.slot_identity()
        })
    );
    let typed = completion(decoded).unwrap();
    let ack = RadioRequest::OrderedOpenV2(ubm_mobile::OrderedOpenRequestV2::Acknowledge {
        id: 8,
        identity: identity.clone(),
    });
    let recover =
        RadioRequest::OrderedOpenV2(ubm_mobile::OrderedOpenRequestV2::Recover { id: 7, identity });
    assert!(typed.answers_request(&ack));
    assert!(!typed.answers_request(&recover));
    assert!(!RadioCompletion::Unit.answers_request(&ack));
    assert!(ubm_mobile::OrderedOpenCompletionV2::try_from(MobileRadioCompletion::Unit).is_err());
}

#[test]
fn completion_must_answer_exact_attempt_and_slot() {
    let identity = identity();
    let request = ubm_mobile::OrderedOpenRequestV2::Acknowledge {
        id: 8,
        identity: identity.clone(),
    };
    let retired = MobileRadioCompletion::OrderedOpenRetiredV2 {
        slot_identity: identity.slot_identity().into(),
    };
    assert!(checked_ordered_open_completion(retired.clone(), &request).is_ok());
    let wrong = MobileRadioCompletion::OrderedOpenRetiredV2 {
        slot_identity: MobileOrderedSlotIdentity {
            slot: 0,
            ..identity.slot_identity().into()
        },
    };
    assert!(checked_ordered_open_completion(wrong, &request).is_err());
    let recover = ubm_mobile::OrderedOpenRequestV2::Recover { id: 7, identity };
    assert!(checked_ordered_open_completion(retired, &recover).is_err());
}

struct VersionedOwner {
    supported: u32,
    identity: &'static str,
    seen: Mutex<Vec<u32>>,
}
impl MobilePlatformRadio for VersionedOwner {
    fn ordered_admission_owner(&self, version: u32) -> String {
        self.seen.lock().unwrap().push(version);
        if version == self.supported {
            self.identity.into()
        } else {
            String::new()
        }
    }
    fn submit(&self, _: MobileRadioRequest) {
        panic!("negotiation must not dispatch")
    }
    fn cancel(&self, _: u64) {
        panic!("negotiation must not cancel")
    }
}
#[test]
fn v2_negotiation_rejects_v1_and_malformed_owner_without_dispatch() {
    let uuid = "12345678-1234-4abc-8def-123456789abc";
    let old = VersionedOwner {
        supported: 1,
        identity: uuid,
        seen: Mutex::new(vec![]),
    };
    assert!(
        matches!(negotiate_ordered_admission_v2(&old), Err(MobileCoreError::Failed { code, .. }) if code == "protocol.incompatible")
    );
    assert_eq!(*old.seen.lock().unwrap(), [2]);
    let malformed = VersionedOwner {
        supported: 2,
        identity: "invalid",
        seen: Mutex::new(vec![]),
    };
    assert!(
        matches!(negotiate_ordered_admission_v2(&malformed), Err(MobileCoreError::Failed { code, .. }) if code == "protocol.malformed")
    );
    let valid = VersionedOwner {
        supported: 2,
        identity: uuid,
        seen: Mutex::new(vec![]),
    };
    assert_eq!(
        negotiate_ordered_admission_v2(&valid).unwrap().to_string(),
        uuid
    );
}
