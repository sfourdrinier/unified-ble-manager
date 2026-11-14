use serde_json::json;
use std::sync::Arc;
use ubm_core::oversized_evidence::{
    EvidenceLimits, EvidenceSource, EvidenceToken, RecoveryError, ScanFailureIdentity,
    ScanFailureLimits, ScanFailureMember, ScanFailureOccurrence,
};
use ubm_desktop::continuation_outbox::{
    Outbox, SCAN_FAILURE_OWNER_CAP, SCAN_FAILURE_SOURCE_BYTES, ScanFailureOutboxError, WakeSink,
};

struct Wake;
impl WakeSink for Wake {
    fn wake(&self, _: u64) {}
}
fn outbox() -> Outbox {
    Outbox::new(42, Arc::new(Wake))
}
fn member(n: u8) -> ScanFailureMember {
    ScanFailureMember {
        recipient: [n; 16],
        delivery: [n + 10; 16],
    }
}
fn limits(max: u64) -> ScanFailureLimits {
    ScanFailureLimits {
        evidence: EvidenceLimits {
            max_depth: 8,
            max_tokens: 32,
            max_encoded_bytes: max,
            max_chunks: 2000,
            chunk_bytes: 4096,
        },
        max_recipients: 2,
        max_bookkeeping_bytes: 4096,
    }
}
fn occurrence(size: usize) -> ScanFailureOccurrence {
    occurrence_with_id(size, 5)
}
fn occurrence_with_id(size: usize, id: u8) -> ScanFailureOccurrence {
    ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [id; 16],
        },
        vec![member(1), member(2)],
        Arc::new(EvidenceSource::new(vec![
            EvidenceToken::Array(3),
            EvidenceToken::Bytes(vec![0x9a; size]),
            EvidenceToken::Text("raw provenance".into()),
            EvidenceToken::U64(17),
        ])),
    )
}

#[test]
fn recovery_discovery_keeps_prepared_and_released_owners_visible() {
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(10_000)).unwrap();
    outbox
        .commit_scan_failure(&mut slot, &mut Some(occurrence(10)))
        .unwrap();
    outbox.finish_scan_failure_producer(&mut slot).unwrap();
    let owners = outbox.scan_failure_recovery_owners();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].member_count, 2);
    assert_eq!(
        outbox
            .scan_failure_recovery_member(&owners[0].key, 0)
            .unwrap(),
        member(1)
    );
    assert_eq!(
        outbox
            .scan_failure_recovery_member(&owners[0].key, 1)
            .unwrap(),
        member(2)
    );
    assert!(
        outbox
            .scan_failure_recovery_member(&owners[0].key, 2)
            .is_err()
    );
    assert!(owners[0].released.is_none());
    let first = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    for member in [member(1), member(2)] {
        let head = outbox.scan_failure_head(member).unwrap().unwrap();
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            offset = outbox.read_scan_failure(&head, offset).unwrap().next_offset;
        }
        outbox.acknowledge_scan_failure(&head).unwrap();
        outbox.retire_scan_failure(&head).unwrap();
    }
    outbox.release_scan_failure_receipt(&first).unwrap();
    let owners = outbox.scan_failure_recovery_owners();
    assert_eq!(owners.len(), 1);
    let mut released = first;
    released.acknowledged = true;
    released.contiguous_read = released.evidence.encoded_length;
    assert_eq!(owners[0].released, Some(released));
    let receipt = outbox.confirm_scan_failure_receipt_release(&first).unwrap();
    outbox.finalize_scan_failure_release(&receipt).unwrap();
    assert!(outbox.scan_failure_recovery_owners().is_empty());
}

#[test]
fn source_capacity_is_reserved_before_capture_and_returned_on_cancel() {
    let outbox = outbox();
    let reservation = outbox
        .reserve_scan_failure_with_source_capacity(limits(10_000), SCAN_FAILURE_SOURCE_BYTES)
        .unwrap();
    assert!(matches!(
        outbox.reserve_scan_failure_with_source_capacity(limits(10_000), 1),
        Err(ScanFailureOutboxError::Capacity)
    ));
    let mut legacy = outbox.reserve_scan_failure(limits(10_000)).unwrap();
    let mut source = Some(occurrence(100));
    assert_eq!(
        outbox.commit_scan_failure(&mut legacy, &mut source),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(source.is_some());
    drop(reservation);
    outbox
        .commit_scan_failure(&mut legacy, &mut source)
        .unwrap();
    assert!(source.is_none());
    assert!(
        outbox
            .reserve_scan_failure_with_source_capacity(limits(10_000), 1)
            .is_ok()
    );
}

#[test]
fn source_larger_than_its_reservation_is_not_consumed() {
    let outbox = outbox();
    let mut reservation = outbox
        .reserve_scan_failure_with_source_capacity(limits(10_000), 1)
        .unwrap();
    let mut source = Some(occurrence(100));
    assert_eq!(
        outbox.commit_scan_failure(&mut reservation, &mut source),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(source.is_some());
    assert_eq!(outbox.pending_scan_failure_owners(), 0);
}

#[test]
fn exact_source_reservation_can_transfer_and_release_unused_capacity() {
    let outbox = outbox();
    let occurrence = occurrence(100);
    let actual = occurrence
        .retained_source_capacity(SCAN_FAILURE_SOURCE_BYTES)
        .unwrap();
    let mut reservation = outbox
        .reserve_scan_failure_with_source_capacity(limits(10_000), SCAN_FAILURE_SOURCE_BYTES)
        .unwrap();
    outbox
        .commit_scan_failure(&mut reservation, &mut Some(occurrence))
        .unwrap();
    let available = SCAN_FAILURE_SOURCE_BYTES - actual;
    assert!(
        outbox
            .reserve_scan_failure_with_source_capacity(limits(10_000), available)
            .is_ok()
    );
}

#[test]
fn prefix_and_large_failure_survive_failed_transport_and_exact_receipts() {
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(2_000_000)).unwrap();
    outbox.push_control(json!({"t":"prefix"}));
    let mut source = Some(occurrence(1_100_000));
    let ordinal = outbox.commit_scan_failure(&mut slot, &mut source).unwrap();
    assert!(source.is_none());
    outbox.push_control(json!({"t":"after"}));
    assert!(outbox.scan_failure_head(member(1)).unwrap().is_none());
    let prefix = outbox.claim_drain(10, 1024).unwrap().unwrap();
    assert_eq!(prefix.records.len(), 1);
    assert_eq!(prefix.records[0]["t"], "prefix");
    assert_eq!(prefix, outbox.claim_drain(10, 1024).unwrap().unwrap());
    assert!(outbox.acknowledge_drain(prefix.id));
    let first = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    assert_eq!(first.ordinal, ordinal);
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
    assert_eq!(first, outbox.scan_failure_head(member(1)).unwrap().unwrap());
    assert!(outbox.claim_drain(10, 2_000_000).is_err());
    assert!(outbox.drain(10, 2_000_000).get("error").is_some());
    assert_eq!(
        outbox.acknowledge_scan_failure(&first),
        Err(ScanFailureOutboxError::Recovery(RecoveryError::Incomplete))
    );
    let mut other = first;
    other.ordinal += 1;
    assert!(outbox.read_scan_failure(&other, 0).is_err());
    let second = outbox.scan_failure_head(member(2)).unwrap().unwrap();
    for descriptor in [first, second] {
        let mut offset = 0;
        let mut body = Vec::new();
        while offset < descriptor.evidence.encoded_length {
            let part = outbox.read_scan_failure(&descriptor, offset).unwrap();
            assert_eq!(part, outbox.read_scan_failure(&descriptor, offset).unwrap());
            assert!(part.bytes.len() <= 4096);
            offset = part.next_offset;
            body.extend(part.bytes);
        }
        assert_eq!(&body[0..10], &[9, 0, 0, 0, 0, 0, 0, 0, 3, 7]);
        assert_eq!(&body[18..18 + 1_100_000], vec![0x9a; 1_100_000]);
        let mut expected_tail = vec![6];
        expected_tail.extend_from_slice(&14u64.to_be_bytes());
        expected_tail.extend_from_slice(b"raw provenance");
        expected_tail.push(4);
        expected_tail.extend_from_slice(&17u64.to_be_bytes());
        assert_eq!(&body[18 + 1_100_000..], expected_tail);
        outbox.acknowledge_scan_failure(&descriptor).unwrap();
        outbox.acknowledge_scan_failure(&descriptor).unwrap();
        outbox.retire_scan_failure(&descriptor).unwrap();
        outbox.retire_scan_failure(&descriptor).unwrap();
        outbox.acknowledge_scan_failure(&descriptor).unwrap();
        if descriptor == first {
            assert!(outbox.claim_drain(10, 1024).is_err());
            assert!(outbox.release_scan_failure_receipt(&first).is_err());
        }
    }
    let after = outbox.claim_drain(10, 1024).unwrap().unwrap();
    assert_eq!(after.records[0]["t"], "after");
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
    outbox.finish_scan_failure_producer(&mut slot).unwrap();
    outbox.release_scan_failure_receipt(&first).unwrap();
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
    let receipt = outbox.confirm_scan_failure_receipt_release(&first).unwrap();
    outbox.finalize_scan_failure_release(&receipt).unwrap();
}

#[test]
fn legacy_drain_stops_at_owner_even_before_java_conversion() {
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(10000)).unwrap();
    outbox.push_control(json!({"t":"prefix"}));
    let mut source = Some(occurrence(100));
    outbox.commit_scan_failure(&mut slot, &mut source).unwrap();
    let prefix = outbox.drain(10, 1000);
    assert_eq!(prefix["records"].as_array().unwrap().len(), 1);
    assert_eq!(prefix["more"], true);
    assert_eq!(outbox.drain(10, 1000)["error"], "retained-scan-failure");
    let descriptor = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    assert_eq!(
        outbox.read_scan_failure(&descriptor, 0).unwrap(),
        outbox.read_scan_failure(&descriptor, 0).unwrap()
    );
}

#[test]
fn quota_and_seal_refusal_preserve_ownership() {
    let outbox = outbox();
    let mut slots = Vec::new();
    for _ in 0..SCAN_FAILURE_OWNER_CAP {
        slots.push(outbox.reserve_scan_failure(limits(20)).unwrap());
    }
    assert!(matches!(
        outbox.reserve_scan_failure(limits(20)),
        Err(ScanFailureOutboxError::Capacity)
    ));
    let mut source = Some(occurrence(100));
    assert!(
        outbox
            .commit_scan_failure(&mut slots[0], &mut source)
            .is_err()
    );
    assert!(source.is_none());
    drop(slots.pop());
    let mut replacement = outbox.reserve_scan_failure(limits(10000)).unwrap();
    let mut later_source = Some(occurrence_with_id(100, 6));
    outbox.seal();
    assert!(matches!(
        outbox.commit_scan_failure(&mut replacement, &mut later_source),
        Err(ScanFailureOutboxError::Sealed)
    ));
    assert!(later_source.is_some());
}

fn retire_head(outbox: &Outbox, id: u8) -> u64 {
    let mut ordinal = 0;
    for recipient in [member(1), member(2)] {
        let head = outbox.scan_failure_head(recipient).unwrap().unwrap();
        assert_eq!(head.evidence.identity.occurrence, [id; 16]);
        ordinal = head.ordinal;
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            offset = outbox.read_scan_failure(&head, offset).unwrap().next_offset;
        }
        outbox.acknowledge_scan_failure(&head).unwrap();
        outbox.retire_scan_failure(&head).unwrap();
    }
    ordinal
}

#[test]
fn pending_owners_recover_in_capture_order_across_cancellation_and_seal() {
    let outbox = Arc::new(outbox());
    let retained = Arc::clone(&outbox);
    outbox.push_control(json!({"t":"prefix"}));
    let prefix = outbox.claim_drain(10, 4096).unwrap().unwrap();
    let mut a = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut b = outbox.reserve_scan_failure(limits(20)).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut a, &mut Some(occurrence_with_id(100, 5)))
            .is_err()
    );
    assert!(
        outbox
            .commit_scan_failure(&mut b, &mut Some(occurrence_with_id(120, 6)))
            .is_err()
    );
    let a_key = outbox.pending_scan_failure(&a).unwrap();
    let b_key = outbox.pending_scan_failure(&b).unwrap();
    assert!(a_key.ordinal < b_key.ordinal);
    let mut duplicate = outbox.reserve_scan_failure(limits(10000)).unwrap();
    let mut duplicate_source = Some(occurrence_with_id(1, 5));
    assert_eq!(
        outbox.commit_scan_failure(&mut duplicate, &mut duplicate_source),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::IdentityMismatch
        ))
    );
    assert!(duplicate_source.is_some());
    let mut substitution = Some(occurrence_with_id(1, 9));
    assert_eq!(
        outbox.commit_scan_failure(&mut a, &mut substitution),
        Err(ScanFailureOutboxError::InvalidReservation)
    );
    assert!(substitution.is_some());
    assert!(outbox.resume_scan_failure(a_key).is_err());
    drop(a);
    assert_eq!(outbox.detached_scan_failure(), Some(a_key));
    let mut bad_key = a_key;
    bad_key.capability[0] ^= 1;
    assert!(outbox.resume_scan_failure(bad_key).is_err());
    assert_eq!(outbox.claim_drain(10, 4096).unwrap().unwrap(), prefix);
    assert!(outbox.acknowledge_drain(prefix.id));
    outbox.push_control(json!({"t":"later"}));
    outbox.seal();
    drop(outbox); // session owner disposed; retained recovery owner survives
    assert!(retained.claim_drain(10, 4096).is_err());
    let mut a = retained.resume_scan_failure(a_key).unwrap();
    assert_eq!(retained.detached_scan_failure(), None);
    assert!(retained.resume_scan_failure(a_key).is_err());
    let mut excessive = limits(100_000_000);
    excessive.max_bookkeeping_bytes = 65 * 1024 * 1024;
    assert_eq!(
        retained.resize_scan_failure_reservation(&a, excessive),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(retained.claim_drain(10, 4096).is_err());
    retained
        .resize_scan_failure_reservation(&b, limits(10000))
        .unwrap();
    assert_eq!(
        retained.commit_scan_failure(&mut b, &mut None).unwrap(),
        b_key.ordinal
    );
    assert!(retained.scan_failure_head(member(1)).is_err());
    retained
        .resize_scan_failure_reservation(&a, limits(10000))
        .unwrap();
    assert_eq!(
        retained.commit_scan_failure(&mut a, &mut None).unwrap(),
        a_key.ordinal
    );
    assert_eq!(retire_head(&retained, 5), a_key.ordinal);
    assert_eq!(retire_head(&retained, 6), b_key.ordinal);
    assert_eq!(
        retained.claim_drain(10, 4096).unwrap().unwrap().records[0]["t"],
        "later"
    );
}

#[test]
fn pending_fence_blocks_both_legacy_modes_and_saturated_control_queue() {
    use ubm_desktop::continuation_outbox::{
        CONTROL_RECORD_CAP, LegacyEvidenceError, LegacyEvidenceOwner,
    };
    for preserving in [false, true] {
        let outbox = outbox();
        let mut slot = outbox.reserve_scan_failure(limits(20)).unwrap();
        assert!(
            outbox
                .commit_scan_failure(&mut slot, &mut Some(occurrence(100)))
                .is_err()
        );
        for _ in 0..=CONTROL_RECORD_CAP {
            outbox.push_control(json!({"t":"later"}));
        }
        let owner = LegacyEvidenceOwner::new(2, 10000);
        if preserving {
            assert_eq!(
                outbox.drain_legacy_preserving(&owner, 1, 10000, |v| Ok(Some(v.clone()))),
                Err(LegacyEvidenceError::ModeConflict)
            );
        } else {
            assert_eq!(outbox.drain(1, 10000)["error"], "retained-scan-failure");
        }
        outbox
            .resize_scan_failure_reservation(&slot, limits(10000))
            .unwrap();
        outbox.commit_scan_failure(&mut slot, &mut None).unwrap();
        retire_head(&outbox, 5);
        let result = if preserving {
            outbox
                .drain_legacy_preserving(&owner, 1, 10000, |v| Ok(Some(v.clone())))
                .unwrap()
        } else {
            outbox.drain(1, 10000)
        };
        assert_eq!(result["records"][0]["t"], "later");
        assert_eq!(result["controlLost"], 1);
    }
}

#[test]
fn capture_bypasses_full_control_quota_without_losing_the_fence() {
    use ubm_desktop::continuation_outbox::CONTROL_RECORD_CAP;
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(20)).unwrap();
    for _ in 0..CONTROL_RECORD_CAP {
        outbox.push_control(json!({"t":"prefix"}));
    }
    assert!(
        outbox
            .commit_scan_failure(&mut slot, &mut Some(occurrence(100)))
            .is_err()
    );
    outbox
        .push_data(json!({"t":"value","valueB64":"AQ=="}))
        .unwrap();
    let prefix = outbox
        .claim_drain(CONTROL_RECORD_CAP + 1, 100_000)
        .unwrap()
        .unwrap();
    assert_eq!(prefix.records.len(), CONTROL_RECORD_CAP);
    assert!(outbox.acknowledge_drain(prefix.id));
    assert!(outbox.claim_drain(1, 4096).is_err());
    outbox
        .resize_scan_failure_reservation(&slot, limits(10000))
        .unwrap();
    outbox.commit_scan_failure(&mut slot, &mut None).unwrap();
    retire_head(&outbox, 5);
    assert_eq!(
        outbox.claim_drain(1, 4096).unwrap().unwrap().records[0]["t"],
        "value"
    );
}

#[test]
fn retained_owner_survives_seal_and_preserving_drain_cannot_cross_it() {
    use ubm_desktop::continuation_outbox::{LegacyEvidenceError, LegacyEvidenceOwner};
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(10000)).unwrap();
    outbox.push_control(json!({"t":"before"}));
    let mut source = Some(occurrence(100));
    outbox.commit_scan_failure(&mut slot, &mut source).unwrap();
    outbox.push_control(json!({"t":"after"}));
    outbox.seal();
    let owner = LegacyEvidenceOwner::new(10, 10000);
    let prefix = outbox
        .drain_legacy_preserving(&owner, 10, 10000, |v| Ok(Some(v.clone())))
        .unwrap();
    assert_eq!(prefix["records"].as_array().unwrap().len(), 1);
    assert!(matches!(
        outbox.drain_legacy_preserving(&owner, 10, 10000, |v| Ok(Some(v.clone()))),
        Err(LegacyEvidenceError::ModeConflict)
    ));
    let head = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    assert!(outbox.read_scan_failure(&head, 0).is_ok());
    assert!(outbox.release_scan_failure_receipt(&head).is_err());
}

#[test]
fn quota_can_be_increased_and_same_source_retried() {
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut source = Some(occurrence(100));
    assert!(outbox.commit_scan_failure(&mut slot, &mut source).is_err());
    assert!(source.is_none());
    outbox
        .resize_scan_failure_reservation(&slot, limits(10000))
        .unwrap();
    let ordinal = outbox.commit_scan_failure(&mut slot, &mut source).unwrap();
    assert_eq!(
        outbox
            .scan_failure_head(member(1))
            .unwrap()
            .unwrap()
            .ordinal,
        ordinal
    );
}

#[test]
fn capacity_refusal_fences_later_claimed_record_until_exact_failure_retires() {
    let outbox = outbox();
    let mut slot = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut source = Some(occurrence(100));
    assert_eq!(
        outbox.commit_scan_failure(&mut slot, &mut source),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::EncodedLimit
        ))
    );
    outbox.push_control(json!({"t":"later"}));
    outbox
        .push_data(json!({"t":"value","valueB64":"AQ=="}))
        .unwrap();
    assert!(
        outbox.claim_drain(10, 4096).is_err(),
        "later record crossed pending failure"
    );
    outbox
        .resize_scan_failure_reservation(&slot, limits(10000))
        .unwrap();
    let ordinal = outbox.commit_scan_failure(&mut slot, &mut source).unwrap();
    assert_eq!(
        outbox.commit_scan_failure(&mut slot, &mut source),
        Ok(ordinal)
    );
    for recipient in [member(1), member(2)] {
        let head = outbox.scan_failure_head(recipient).unwrap().unwrap();
        assert_eq!(head.ordinal, ordinal);
        assert_eq!(head.evidence.identity.occurrence, [5; 16]);
        let mut offset = 0;
        while offset < head.evidence.encoded_length {
            offset = outbox.read_scan_failure(&head, offset).unwrap().next_offset;
        }
        outbox.acknowledge_scan_failure(&head).unwrap();
        outbox.retire_scan_failure(&head).unwrap();
    }
    let later = outbox.claim_drain(10, 4096).unwrap().unwrap();
    assert_eq!(later.records[0]["t"], "later");
    assert_eq!(later.records[1]["t"], "value");
    assert!(later.records[0]["ordinal"].as_u64().unwrap() > ordinal);
}

#[test]
fn descriptors_cannot_cross_outboxes_or_members_even_with_reused_ids() {
    let a = outbox();
    let b = outbox();
    let mut a_slot = a.reserve_scan_failure(limits(10000)).unwrap();
    let mut b_slot = b.reserve_scan_failure(limits(10000)).unwrap();
    a.commit_scan_failure(&mut a_slot, &mut Some(occurrence(100)))
        .unwrap();
    b.commit_scan_failure(&mut b_slot, &mut Some(occurrence(100)))
        .unwrap();
    let head = a.scan_failure_head(member(1)).unwrap().unwrap();
    let other = b.scan_failure_head(member(1)).unwrap().unwrap();
    assert_ne!(head.capability, other.capability);
    assert!(b.read_scan_failure(&head, 0).is_err());
    assert!(b.acknowledge_scan_failure(&head).is_err());
    let mut wrong = head;
    wrong.evidence.member.delivery = [99; 16];
    assert_eq!(
        a.read_scan_failure(&wrong, 0),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::IdentityMismatch
        ))
    );
    assert_eq!(
        a.acknowledge_scan_failure(&wrong),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::IdentityMismatch
        ))
    );
    wrong = head;
    wrong.evidence.encoded_length += 1;
    assert_eq!(
        a.acknowledge_scan_failure(&wrong),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::IdentityMismatch
        ))
    );
    assert_eq!(a.scan_failure_head(member(1)).unwrap(), Some(head));
}

#[test]
fn later_prepared_owner_releases_unused_budget_for_earlier_retry() {
    let outbox = outbox();
    let mut a = outbox.reserve_scan_failure(limits(32_000_000)).unwrap();
    let mut b = outbox.reserve_scan_failure(limits(32_000_000)).unwrap();
    let mut too_small = limits(20);
    too_small.evidence.max_encoded_bytes = 20;
    outbox
        .resize_scan_failure_reservation(&a, too_small)
        .unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut a, &mut Some(occurrence_with_id(100, 5)))
            .is_err()
    );
    outbox
        .commit_scan_failure(&mut b, &mut Some(occurrence_with_id(10, 6)))
        .unwrap();
    outbox
        .resize_scan_failure_reservation(&a, limits(40_000_000))
        .unwrap();
    outbox.commit_scan_failure(&mut a, &mut None).unwrap();
    assert!(retire_head(&outbox, 5) < retire_head(&outbox, 6));
}

#[test]
fn original_32m_reservations_and_40m_head_prepare_in_order() {
    let outbox = outbox();
    let mut initial = limits(32_000_000);
    initial.evidence.max_chunks = 20_000;
    initial.max_bookkeeping_bytes = 10_000;
    let mut a = outbox.reserve_scan_failure(initial).unwrap();
    let mut b = outbox.reserve_scan_failure(initial).unwrap();
    assert_eq!(
        outbox.commit_scan_failure(&mut a, &mut Some(occurrence_with_id(40_000_000, 5))),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::EncodedLimit
        ))
    );
    let b_ordinal = outbox
        .commit_scan_failure(&mut b, &mut Some(occurrence_with_id(10, 6)))
        .unwrap();
    let mut increased = initial;
    increased.evidence.max_encoded_bytes = 40_001_000;
    outbox
        .resize_scan_failure_reservation(&a, increased)
        .unwrap();
    let a_ordinal = outbox.commit_scan_failure(&mut a, &mut None).unwrap();
    assert!(a_ordinal < b_ordinal);
    assert_eq!(
        outbox
            .scan_failure_head(member(1))
            .unwrap()
            .unwrap()
            .ordinal,
        a_ordinal
    );
}

#[test]
fn later_prepared_owner_releases_unused_bookkeeping_credit() {
    let outbox = outbox();
    let mut a = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut large_bookkeeping = limits(1_000_000);
    large_bookkeeping.max_bookkeeping_bytes = 30_000_000;
    let mut b = outbox.reserve_scan_failure(large_bookkeeping).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut a, &mut Some(occurrence_with_id(100, 5)))
            .is_err()
    );
    outbox
        .commit_scan_failure(&mut b, &mut Some(occurrence_with_id(10, 6)))
        .unwrap();
    outbox
        .resize_scan_failure_reservation(&a, limits(40_000_000))
        .unwrap();
}

#[test]
fn actual_bookkeeping_pressure_can_use_raw_export_to_release_the_head() {
    let outbox = outbox();
    let mut a = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut b_limits = limits(1_100_000);
    b_limits.evidence.chunk_bytes = 1;
    b_limits.evidence.max_chunks = 1_100_000;
    b_limits.max_recipients = 64;
    b_limits.max_bookkeeping_bytes = 10_000_000;
    let mut b = outbox.reserve_scan_failure(b_limits).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut a, &mut Some(occurrence_with_id(100, 5)))
            .is_err()
    );
    let source = ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [6; 16],
        },
        (1..=64).map(member).collect(),
        Arc::new(EvidenceSource::new(vec![
            EvidenceToken::Array(3),
            EvidenceToken::Bytes(vec![0; 1_000_000]),
            EvidenceToken::Null,
            EvidenceToken::Null,
        ])),
    );
    outbox
        .commit_scan_failure(&mut b, &mut Some(source))
        .unwrap();
    let mut a_limits = limits(1_000_000);
    a_limits.max_bookkeeping_bytes = 62_000_000;
    assert_eq!(
        outbox.resize_scan_failure_reservation(&a, a_limits),
        Err(ScanFailureOutboxError::Capacity)
    );
    let key = outbox.pending_scan_failure(&a).unwrap();
    let info = outbox.inspect_scan_failure(&key).unwrap();
    assert_eq!(info.token_count, 4);
    for i in 0..info.member_count {
        outbox.inspect_scan_failure_member(&key, i).unwrap();
    }
    for i in 0..info.token_count {
        outbox.inspect_scan_failure_token(&key, i, 0, 4096).unwrap();
    }
    outbox.finish_scan_failure_producer(&mut a).unwrap();
    outbox.retire_unprepared_scan_failure(&key).unwrap();
    assert_eq!(
        outbox
            .scan_failure_head(member(1))
            .unwrap()
            .unwrap()
            .evidence
            .identity
            .occurrence,
        [6; 16]
    );
}

#[test]
fn prepared_owner_can_reattach_and_replay_commit() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(10000)).unwrap();
    let ordinal = outbox
        .commit_scan_failure(&mut token, &mut Some(occurrence(10)))
        .unwrap();
    drop(token);
    let key = outbox.detached_scan_failure().unwrap();
    let mut resumed = outbox.resume_scan_failure(key).unwrap();
    assert_eq!(
        outbox.commit_scan_failure(&mut resumed, &mut None),
        Ok(ordinal)
    );
}

#[test]
fn blocked_claim_rearms_for_resize_and_preparation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counter(AtomicUsize);
    impl WakeSink for Counter {
        fn wake(&self, _: u64) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let wake = Arc::new(Counter(AtomicUsize::new(0)));
    let outbox = Outbox::new(42, wake.clone());
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut token, &mut Some(occurrence(100)))
            .is_err()
    );
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert!(outbox.claim_drain(10, 4096).is_err());
    outbox
        .resize_scan_failure_reservation(&token, limits(10000))
        .unwrap();
    assert_eq!(wake.0.load(Ordering::SeqCst), 2);
    assert!(outbox.claim_drain(10, 4096).is_err());
    outbox.commit_scan_failure(&mut token, &mut None).unwrap();
    assert_eq!(wake.0.load(Ordering::SeqCst), 3);
}

#[test]
fn blocked_modes_rearm_before_retirement_reveals_later_record() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use ubm_desktop::continuation_outbox::LegacyEvidenceOwner;
    struct Counter(AtomicUsize);
    impl WakeSink for Counter {
        fn wake(&self, _: u64) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for mode in 0..3 {
        let wake = Arc::new(Counter(AtomicUsize::new(0)));
        let outbox = Outbox::new(42, wake.clone());
        let mut token = outbox.reserve_scan_failure(limits(10000)).unwrap();
        outbox
            .commit_scan_failure(&mut token, &mut Some(occurrence(10)))
            .unwrap();
        outbox.push_control(json!({"t":"later"}));
        match mode {
            0 => {
                assert!(outbox.claim_drain(1, 4096).is_err());
            }
            1 => {
                assert!(outbox.drain(1, 4096).get("error").is_some());
            }
            _ => {
                assert!(
                    outbox
                        .drain_legacy_preserving(&LegacyEvidenceOwner::new(2, 4096), 1, 4096, |v| {
                            Ok(Some(v.clone()))
                        })
                        .is_err()
                );
            }
        }
        let before = wake.0.load(Ordering::SeqCst);
        retire_head(&outbox, 5);
        assert_eq!(wake.0.load(Ordering::SeqCst), before + 1, "mode {mode}");
    }
}

#[test]
fn release_waits_for_producer_and_replays_exact_response() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(10000)).unwrap();
    outbox
        .commit_scan_failure(&mut token, &mut Some(occurrence(10)))
        .unwrap();
    let head = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    retire_head(&outbox, 5);
    assert!(outbox.release_scan_failure_receipt(&head).is_err());
    assert_eq!(
        outbox.commit_scan_failure(&mut token, &mut None),
        Ok(head.ordinal)
    );
    drop(token);
    assert!(outbox.release_scan_failure_receipt(&head).is_err());
    let key = outbox.detached_scan_failure().unwrap();
    let mut token = outbox.resume_scan_failure(key).unwrap();
    assert_eq!(
        outbox.commit_scan_failure(&mut token, &mut None),
        Ok(head.ordinal)
    );
    outbox.finish_scan_failure_producer(&mut token).unwrap();
    outbox.release_scan_failure_receipt(&head).unwrap();
    outbox.release_scan_failure_receipt(&head).unwrap();
    let mut wrong = head;
    wrong.evidence.encoded_length += 1;
    assert!(outbox.release_scan_failure_receipt(&wrong).is_err());
    let receipt = outbox.confirm_scan_failure_receipt_release(&head).unwrap();
    assert_eq!(
        receipt,
        outbox.confirm_scan_failure_receipt_release(&head).unwrap()
    );
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
    let mut forged = receipt;
    forged.capability[0] ^= 1;
    assert!(outbox.finalize_scan_failure_release(&forged).is_err());
    assert!(
        Outbox::new(999, Arc::new(Wake))
            .finalize_scan_failure_release(&receipt)
            .is_err()
    );
    outbox.finalize_scan_failure_release(&receipt).unwrap();
    assert!(matches!(
        outbox.finalize_scan_failure_release(&receipt),
        Err(ScanFailureOutboxError::NotHead)
    ));
    assert_eq!(outbox.pending_scan_failure_owners(), 0);
    assert!(outbox.release_scan_failure_receipt(&head).is_err());
}

#[test]
fn raw_inspection_recovers_malformed_membership_and_token_in_bounded_parts() {
    use ubm_desktop::continuation_outbox::ScanFailureRawToken;
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    let source = ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [8; 16],
        },
        vec![member(1), member(1)],
        Arc::new(EvidenceSource::new(vec![EvidenceToken::Bytes(vec![
            1, 2, 3, 4, 5,
        ])])),
    );
    assert!(
        outbox
            .commit_scan_failure(&mut token, &mut Some(source))
            .is_err()
    );
    let key = outbox.pending_scan_failure(&token).unwrap();
    let info = outbox.inspect_scan_failure(&key).unwrap();
    assert_eq!(info.member_count, 2);
    assert_eq!(info.token_count, 1);
    assert_eq!(
        outbox.inspect_scan_failure_member(&key, 1).unwrap(),
        member(1)
    );
    assert_eq!(
        outbox.inspect_scan_failure_token(&key, 0, 1, 2).unwrap(),
        ScanFailureRawToken::Bytes {
            total: 5,
            bytes: vec![2, 3]
        }
    );
    assert_eq!(
        outbox.inspect_scan_failure_token(&key, 0, 0, 1).unwrap(),
        ScanFailureRawToken::Bytes {
            total: 5,
            bytes: vec![1]
        }
    );
    assert_eq!(
        outbox.inspect_scan_failure_token(&key, 0, 3, 2).unwrap(),
        ScanFailureRawToken::Bytes {
            total: 5,
            bytes: vec![4, 5]
        }
    );
    let mut bad = key;
    bad.capability[0] ^= 1;
    assert!(outbox.inspect_scan_failure(&bad).is_err());
    assert!(
        outbox
            .inspect_scan_failure_token(&key, 0, 0, 65_537)
            .is_err()
    );
    outbox.push_control(json!({"t":"later"}));
    assert!(outbox.retire_unprepared_scan_failure(&key).is_err());
    outbox.finish_scan_failure_producer(&mut token).unwrap();
    outbox.retire_unprepared_scan_failure(&key).unwrap();
    outbox.retire_unprepared_scan_failure(&key).unwrap();
    assert_eq!(outbox.inspect_scan_failure(&key).unwrap(), info);
    assert_eq!(
        outbox.claim_drain(1, 4096).unwrap().unwrap().records[0]["t"],
        "later"
    );
    outbox
        .release_unprepared_scan_failure_receipt(&key)
        .unwrap();
    outbox
        .release_unprepared_scan_failure_receipt(&key)
        .unwrap();
    let receipt = outbox
        .confirm_unprepared_scan_failure_release(&key)
        .unwrap();
    assert_eq!(
        receipt,
        outbox
            .confirm_unprepared_scan_failure_release(&key)
            .unwrap()
    );
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
    outbox.finalize_scan_failure_release(&receipt).unwrap();
    assert!(matches!(
        outbox.finalize_scan_failure_release(&receipt),
        Err(ScanFailureOutboxError::NotHead)
    ));
    assert_eq!(outbox.pending_scan_failure_owners(), 0);
    assert!(
        outbox
            .release_unprepared_scan_failure_receipt(&key)
            .is_err()
    );
}

#[test]
fn capture_rejects_source_capacity_beyond_outbox_bound_before_transfer() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut bytes = Vec::with_capacity(64 * 1024 * 1024 + 1);
    bytes.push(7);
    let mut source = Some(ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [8; 16],
        },
        vec![member(1)],
        Arc::new(EvidenceSource::new(vec![EvidenceToken::Bytes(bytes)])),
    ));
    assert_eq!(
        outbox.commit_scan_failure(&mut token, &mut source),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(source.is_some());
    assert_eq!(outbox.pending_scan_failure_owners(), 0);
}

#[test]
fn source_capacity_counts_all_captured_owners_and_member_capacity() {
    let outbox = outbox();
    let mut first = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut second = outbox.reserve_scan_failure(limits(20)).unwrap();
    let make = |id| {
        let mut bytes = Vec::with_capacity(34 * 1024 * 1024);
        bytes.push(id);
        let mut members = Vec::with_capacity(32);
        members.push(member(1));
        ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [id; 16],
            },
            members,
            Arc::new(EvidenceSource::new(vec![EvidenceToken::Bytes(bytes)])),
        )
    };
    assert!(
        outbox
            .commit_scan_failure(&mut first, &mut Some(make(1)))
            .is_err()
    );
    let mut source = Some(make(2));
    assert_eq!(
        outbox.commit_scan_failure(&mut second, &mut source),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(source.is_some());
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
}

#[test]
fn capture_counts_reserved_member_vector_capacity_even_when_most_is_unused() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    let capacity = ubm_desktop::continuation_outbox::SCAN_FAILURE_SOURCE_BYTES as usize
        / std::mem::size_of::<ScanFailureMember>()
        + 1;
    let mut members = Vec::with_capacity(capacity);
    members.push(member(1));
    let mut source = Some(ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [8; 16],
        },
        members,
        Arc::new(EvidenceSource::new(vec![EvidenceToken::Null])),
    ));
    assert_eq!(
        outbox.commit_scan_failure(&mut token, &mut source),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(source.is_some());
}

#[test]
fn raw_inspection_survives_empty_duplicate_and_limit_failures() {
    for members in [vec![], vec![member(1), member(1)]] {
        let outbox = outbox();
        let mut token = outbox.reserve_scan_failure(limits(10000)).unwrap();
        let source = ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence: [9; 16],
            },
            members.clone(),
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Null,
            ])),
        );
        assert!(
            outbox
                .commit_scan_failure(&mut token, &mut Some(source))
                .is_err()
        );
        let key = outbox.pending_scan_failure(&token).unwrap();
        assert_eq!(
            outbox.inspect_scan_failure(&key).unwrap().member_count,
            members.len()
        );
        for (i, member) in members.iter().enumerate() {
            assert_eq!(
                outbox.inspect_scan_failure_member(&key, i).unwrap(),
                *member
            );
        }
    }
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut token, &mut Some(occurrence(100)))
            .is_err()
    );
    let key = outbox.pending_scan_failure(&token).unwrap();
    assert_eq!(outbox.inspect_scan_failure(&key).unwrap().token_count, 4);
}

#[test]
fn finished_unprepared_owner_remains_discoverable_for_raw_recovery() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut token, &mut Some(occurrence(100)))
            .is_err()
    );
    outbox.finish_scan_failure_producer(&mut token).unwrap();
    let key = outbox
        .detached_scan_failure()
        .expect("finished source must be discoverable");
    assert_eq!(
        outbox
            .inspect_scan_failure(&key)
            .unwrap()
            .identity
            .occurrence,
        [5; 16]
    );
    outbox.retire_unprepared_scan_failure(&key).unwrap();
}

#[test]
fn released_receipt_keeps_owner_and_identity_until_cleanup() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(10000)).unwrap();
    outbox
        .commit_scan_failure(&mut token, &mut Some(occurrence(10)))
        .unwrap();
    let head = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    retire_head(&outbox, 5);
    outbox.finish_scan_failure_producer(&mut token).unwrap();
    outbox.release_scan_failure_receipt(&head).unwrap();
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
    let mut another = outbox.reserve_scan_failure(limits(10000)).unwrap();
    let mut duplicate = Some(occurrence(10));
    assert_eq!(
        outbox.commit_scan_failure(&mut another, &mut duplicate),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::IdentityMismatch
        ))
    );
    assert!(duplicate.is_some());
}

#[test]
fn source_charge_drops_at_last_ack_even_before_producer_finish() {
    let outbox = outbox();
    let mut config = limits(40_000_000);
    config.evidence.max_chunks = 20_000;
    let mut first = outbox.reserve_scan_failure(config).unwrap();
    outbox
        .commit_scan_failure(&mut first, &mut Some(occurrence_with_id(35_000_000, 5)))
        .unwrap();
    let a = outbox.scan_failure_head(member(1)).unwrap().unwrap();
    let b = outbox.scan_failure_head(member(2)).unwrap().unwrap();
    let mut offset = 0;
    while offset < a.evidence.encoded_length {
        offset = outbox.read_scan_failure(&a, offset).unwrap().next_offset;
    }
    outbox.acknowledge_scan_failure(&a).unwrap();
    let mut second = outbox.reserve_scan_failure(limits(20)).unwrap();
    let mut next = Some(occurrence_with_id(35_000_000, 6));
    assert_eq!(
        outbox.commit_scan_failure(&mut second, &mut next),
        Err(ScanFailureOutboxError::Capacity)
    );
    assert!(next.is_some());
    offset = 0;
    while offset < b.evidence.encoded_length {
        offset = outbox.read_scan_failure(&b, offset).unwrap().next_offset;
    }
    outbox.acknowledge_scan_failure(&b).unwrap();
    assert_eq!(
        outbox.commit_scan_failure(&mut second, &mut next),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::EncodedLimit
        ))
    );
    assert!(next.is_none());
}

#[test]
fn captured_resize_uses_separate_source_and_bookkeeping_limits() {
    let outbox = outbox();
    let mut token = outbox.reserve_scan_failure(limits(20)).unwrap();
    let source = ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        },
        (1..=5).map(member).collect(),
        Arc::new(EvidenceSource::new(vec![
            EvidenceToken::Array(3),
            EvidenceToken::Bytes(vec![0; 40_000_000]),
            EvidenceToken::Null,
            EvidenceToken::Null,
        ])),
    );
    assert!(
        outbox
            .commit_scan_failure(&mut token, &mut Some(source))
            .is_err()
    );
    let mut expanded = limits(40_001_000);
    expanded.evidence.chunk_bytes = 1;
    expanded.evidence.max_chunks = 40_001_000;
    expanded.max_recipients = 5;
    expanded.max_bookkeeping_bytes = 31_000_000;
    outbox
        .resize_scan_failure_reservation(&token, expanded)
        .unwrap();
    outbox.commit_scan_failure(&mut token, &mut None).unwrap();
}

#[test]
fn finished_prepared_owner_does_not_hide_later_unprepared_recovery() {
    let outbox = outbox();
    let mut prepared = outbox.reserve_scan_failure(limits(10000)).unwrap();
    outbox
        .commit_scan_failure(&mut prepared, &mut Some(occurrence_with_id(10, 5)))
        .unwrap();
    outbox.finish_scan_failure_producer(&mut prepared).unwrap();
    let mut broken = outbox.reserve_scan_failure(limits(20)).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut broken, &mut Some(occurrence_with_id(100, 6)))
            .is_err()
    );
    outbox.finish_scan_failure_producer(&mut broken).unwrap();
    let key = outbox
        .detached_scan_failure()
        .expect("unprepared owner remains discoverable");
    assert_eq!(key.ordinal, 2);
    retire_head(&outbox, 5);
    assert_eq!(
        outbox
            .inspect_scan_failure(&key)
            .unwrap()
            .identity
            .occurrence,
        [6; 16]
    );
}

#[test]
fn raw_release_blocks_identity_reuse_until_exact_cleanup() {
    let outbox = outbox();
    let mut a = outbox.reserve_scan_failure(limits(20)).unwrap();
    assert!(
        outbox
            .commit_scan_failure(&mut a, &mut Some(occurrence(100)))
            .is_err()
    );
    let key = outbox.pending_scan_failure(&a).unwrap();
    outbox.finish_scan_failure_producer(&mut a).unwrap();
    outbox.retire_unprepared_scan_failure(&key).unwrap();
    outbox
        .release_unprepared_scan_failure_receipt(&key)
        .unwrap();
    let mut b = outbox.reserve_scan_failure(limits(10000)).unwrap();
    let mut same = Some(occurrence(10));
    assert_eq!(
        outbox.commit_scan_failure(&mut b, &mut same),
        Err(ScanFailureOutboxError::Recovery(
            RecoveryError::IdentityMismatch
        ))
    );
    let receipt = outbox
        .confirm_unprepared_scan_failure_release(&key)
        .unwrap();
    outbox.finalize_scan_failure_release(&receipt).unwrap();
    outbox.commit_scan_failure(&mut b, &mut same).unwrap();
    assert!(matches!(
        outbox.finalize_scan_failure_release(&receipt),
        Err(ScanFailureOutboxError::NotHead)
    ));
    assert_eq!(outbox.pending_scan_failure_owners(), 1);
}
