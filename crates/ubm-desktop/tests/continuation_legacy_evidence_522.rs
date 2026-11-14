use serde_json::{Value, json};
use std::sync::Arc;
use ubm_desktop::continuation_outbox::{
    LegacyEvidenceBudget, LegacyEvidenceError, LegacyEvidenceOwner, Outbox, WakeSink,
};

struct Wake;
impl WakeSink for Wake {
    fn wake(&self, _: u64) {}
}
fn project(record: &Value) -> Result<Option<Value>, ()> {
    if record["t"] == "peer-source-loss" {
        return Ok(None);
    }
    let mut value = record.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove("provenance");
    }
    Ok(Some(value))
}

#[test]
fn legacy_projection_retains_complete_originals_through_owner_lifetime() {
    let evidence = LegacyEvidenceOwner::new(4, 4096);
    let outbox = Outbox::new(1, Arc::new(Wake));
    outbox.push_data(json!({"t":"value", "consumer":"a", "valueB64":"AQ==", "delivery":"notification", "provenance":"unknown", "sourceOrdinal":"9"})).unwrap();
    outbox
        .push_data(json!({"t":"peer-source-loss", "consumer":"a", "detail":"original"}))
        .unwrap();
    outbox.seal();
    assert!(
        outbox
            .push_data(json!({"t":"value","valueB64":"Ag=="}))
            .unwrap_err()
            .after_cutoff()
    );
    assert_eq!(outbox.after_cutoff_loss().items, 1);
    outbox.push_control(json!({"t":"cleanup", "detail":"tail"}));
    let batch = outbox
        .drain_legacy_preserving(&evidence, 10, 4096, project)
        .unwrap();
    assert_eq!(batch["records"].as_array().unwrap().len(), 2);
    assert_eq!(batch["records"][0]["ordinal"], 1);
    assert_eq!(batch["records"][1]["ordinal"], 3);
    assert!(batch["records"][0].get("provenance").is_none());
    drop(outbox);
    assert_eq!(evidence.get(1).unwrap()["sourceOrdinal"], "9");
    assert_eq!(evidence.get(2).unwrap()["detail"], "original");
    assert_eq!(evidence.get(3).unwrap()["detail"], "tail");
    assert!(!evidence.retire(4));
    assert!(evidence.retire(2));
    assert!(!evidence.retire(2));
    assert!(evidence.get(1).is_some());
}

#[test]
fn quota_refusal_is_atomic_and_recoverable_and_modes_are_fenced() {
    let evidence = LegacyEvidenceOwner::new(1, 4096);
    let outbox = Outbox::new(2, Arc::new(Wake));
    outbox
        .push_data(json!({"t":"value", "valueB64":"AQ=="}))
        .unwrap();
    outbox.push_control(json!({"t":"cleanup"}));
    assert_eq!(
        outbox.drain_legacy_preserving(&evidence, 10, 4096, project),
        Err(LegacyEvidenceError::Capacity)
    );
    assert_eq!(outbox.queued_data(), 1);
    assert!(evidence.get(1).is_none());
    assert_eq!(
        outbox
            .drain_legacy_preserving(&evidence, 1, 1, project)
            .unwrap()["records"][0]["ordinal"],
        1
    );
    assert_eq!(
        outbox.drain_legacy_preserving(&evidence, 1, 1, project),
        Err(LegacyEvidenceError::Capacity)
    );
    assert!(evidence.retire(1));
    assert_eq!(
        outbox
            .drain_legacy_preserving(&evidence, 1, 1, project)
            .unwrap()["records"][0]["ordinal"],
        2
    );
    assert!(outbox.claim_drain(1, 4096).is_err());
    assert_eq!(outbox.drain(1, 4096)["error"], "drain-mode-conflict");
    let other = LegacyEvidenceOwner::new(5, 4096);
    assert_eq!(
        outbox.drain_legacy_preserving(&other, 1, 1, project),
        Err(LegacyEvidenceError::OwnerMismatch)
    );
}

#[test]
fn shared_exact_evidence_budget_refuses_before_source_ack_and_reclaims_on_retire() {
    let budget = Arc::new(LegacyEvidenceBudget::new(1, 4096));
    let first = Outbox::new(20, Arc::new(Wake));
    let second = Outbox::new(21, Arc::new(Wake));
    let first_evidence = LegacyEvidenceOwner::with_budget(4, 4096, budget.clone());
    let second_evidence = LegacyEvidenceOwner::with_budget(4, 4096, budget);
    first
        .push_data(json!({"t":"value", "valueB64":"AQ=="}))
        .unwrap();
    second
        .push_data(json!({"t":"value", "valueB64":"Ag=="}))
        .unwrap();

    let first_claim = first
        .claim_drain_projected(&first_evidence, 1, 4096, project)
        .unwrap()
        .unwrap();
    assert_eq!(first_evidence.ordinals(0, 4), vec![1]);
    assert_eq!(
        second.claim_drain_projected(&second_evidence, 1, 4096, project),
        Err(LegacyEvidenceError::Capacity)
    );
    // Capacity refusal happens before the second source has a pending claim
    // or acknowledgement, so no original was silently advanced.
    assert_eq!(second.queued_data(), 1);
    assert!(second_evidence.ordinals(0, 4).is_empty());

    assert!(first.acknowledge_drain(first_claim.id));
    assert!(first_evidence.retire(1));
    let second_claim = second
        .claim_drain_projected(&second_evidence, 1, 4096, project)
        .unwrap()
        .unwrap();
    assert!(second.acknowledge_drain(second_claim.id));
}

#[test]
fn legacy_drain_shares_the_exact_evidence_budget_before_source_removal() {
    let budget = Arc::new(LegacyEvidenceBudget::new(1, 4096));
    let first = Outbox::new(22, Arc::new(Wake));
    let second = Outbox::new(23, Arc::new(Wake));
    let first_evidence = LegacyEvidenceOwner::with_budget(4, 4096, budget.clone());
    let second_evidence = LegacyEvidenceOwner::with_budget(4, 4096, budget);
    first
        .push_data(json!({"t":"value", "valueB64":"AQ=="}))
        .unwrap();
    second
        .push_data(json!({"t":"value", "valueB64":"Ag=="}))
        .unwrap();

    first
        .drain_legacy_preserving(&first_evidence, 1, 4096, project)
        .unwrap();
    assert_eq!(
        second.drain_legacy_preserving(&second_evidence, 1, 4096, project),
        Err(LegacyEvidenceError::Capacity)
    );
    assert_eq!(second.queued_data(), 1);
    assert!(second_evidence.ordinals(0, 4).is_empty());

    assert!(first_evidence.retire(1));
    second
        .drain_legacy_preserving(&second_evidence, 1, 4096, project)
        .unwrap();
}

#[test]
fn projection_cannot_change_identity_or_consume_on_failure() {
    let evidence = LegacyEvidenceOwner::new(2, 4096);
    let outbox = Outbox::new(3, Arc::new(Wake));
    outbox
        .push_data(json!({"t":"value", "valueB64":"AQ=="}))
        .unwrap();
    assert_eq!(
        outbox.drain_legacy_preserving(&evidence, 1, 4096, |_| Ok(Some(json!({"ordinal":42})))),
        Err(LegacyEvidenceError::InvalidProjection)
    );
    assert_eq!(outbox.queued_data(), 1);
    assert!(evidence.get(1).is_none());
}

#[test]
fn evidence_pages_exact_originals_atomically_and_requires_explicit_retirement() {
    let evidence = LegacyEvidenceOwner::new(4, 4096);
    let outbox = Outbox::new(30, Arc::new(Wake));
    let original = json!({"t":"peer-source-loss","consumer":"a","peerId":"p","generation":"1","skippedRecords":null});
    outbox.push_data(original.clone()).unwrap();
    outbox.push_data(json!({"t":"value","consumer":"a","valueB64":"AQ==","delivery":"notification","provenance":"unknown"})).unwrap();
    outbox
        .drain_legacy_preserving(&evidence, 8, 4096, project)
        .unwrap();
    let (page, more) = evidence.page(0, 1, 4096).unwrap();
    assert!(more);
    assert_eq!(
        page,
        vec![
            serde_json::json!({"t":"peer-source-loss","consumer":"a","peerId":"p","generation":"1","skippedRecords":null,"ordinal":1})
        ]
    );
    assert!(evidence.retire(1));
    assert!(evidence.get(1).is_none());
    assert!(!evidence.retire(1));
}

#[test]
fn byte_accounting_counts_data_only_and_reports_loss_with_original_ordinal() {
    let evidence = LegacyEvidenceOwner::new(8, 8192);
    let outbox = Outbox::new(4, Arc::new(Wake));
    let first = json!({"t":"value","valueB64":"AQ=="});
    let first_bytes = first.to_string().len();
    outbox.push_data(first).unwrap();
    outbox.push_control(json!({"t":"control","payload":"a long control"}));
    outbox
        .push_data(json!({"t":"value","valueB64":"Ag=="}))
        .unwrap();
    outbox.note_control_loss(4);
    let batch = outbox
        .drain_legacy_preserving(&evidence, 8, first_bytes, project)
        .unwrap();
    assert_eq!(batch["records"].as_array().unwrap().len(), 2);
    assert_eq!(batch["records"][0]["ordinal"], 1);
    assert_eq!(batch["records"][1]["ordinal"], 2);
    assert_eq!(batch["more"], true);
    let batch = outbox
        .drain_legacy_preserving(&evidence, 8, 1, project)
        .unwrap();
    assert_eq!(batch["records"][0]["ordinal"], 3);
    assert_eq!(batch["records"][1]["ordinal"], 4);
    assert_eq!(batch["records"][1]["count"], 4);
    assert_eq!(batch["controlLost"], 4);
    assert_eq!(evidence.ordinals(1, 2), vec![2, 3]);
    assert_eq!(evidence.get(4).unwrap()["count"], 4);
}

#[test]
fn an_owner_never_aliases_a_second_outbox_even_after_retirement() {
    let evidence = LegacyEvidenceOwner::new(2, 4096);
    let first = Outbox::new(5, Arc::new(Wake));
    first.push_control(json!({"t":"first"}));
    first
        .drain_legacy_preserving(&evidence, 1, 4096, project)
        .unwrap();
    assert!(evidence.retire(1));
    drop(first);
    let second = Outbox::new(5, Arc::new(Wake));
    second.push_control(json!({"t":"second"}));
    assert_eq!(
        second.drain_legacy_preserving(&evidence, 1, 4096, project),
        Err(LegacyEvidenceError::OwnerMismatch)
    );
    assert_eq!(second.drain(1, 4096)["records"][0]["t"], "second");
}

#[test]
fn concurrent_drain_and_ingress_never_duplicate_or_reorder_evidence() {
    let evidence = Arc::new(LegacyEvidenceOwner::new(256, 100_000));
    let outbox = Arc::new(Outbox::new(6, Arc::new(Wake)));
    std::thread::scope(|scope| {
        let source = outbox.clone();
        scope.spawn(move || {
            for n in 0..100 {
                source.push_control(json!({"t":"control", "n":n}));
            }
        });
        let source = outbox.clone();
        let owner = evidence.clone();
        scope.spawn(move || {
            for _ in 0..100 {
                source
                    .drain_legacy_preserving(&owner, 3, 1, project)
                    .unwrap();
            }
        });
    });
    while outbox
        .drain_legacy_preserving(&evidence, 3, 1, project)
        .unwrap()["more"]
        == true
    {}
    let ordinals = evidence.ordinals(0, 256);
    assert_eq!(ordinals, (1..=100).collect::<Vec<_>>());
    for ordinal in ordinals {
        assert_eq!(evidence.get(ordinal).unwrap()["n"], ordinal - 1);
    }
}

#[test]
fn oversized_evidence_can_be_retried_after_explicit_quota_increase() {
    let evidence = LegacyEvidenceOwner::new(2, 100);
    let outbox = Outbox::new(7, Arc::new(Wake));
    outbox.push_control(json!({"t":"small"}));
    outbox
        .drain_legacy_preserving(&evidence, 1, 1, project)
        .unwrap();
    outbox.push_control(json!({"t":"large", "payload":"x".repeat(200)}));
    assert_eq!(
        outbox.drain_legacy_preserving(&evidence, 1, 1, project),
        Err(LegacyEvidenceError::Capacity)
    );
    assert!(!evidence.set_limits(0, 0));
    assert!(evidence.set_limits(2, 1024));
    assert_eq!(
        outbox
            .drain_legacy_preserving(&evidence, 1, 1, project)
            .unwrap()["records"][0]["ordinal"],
        2
    );
    assert_eq!(evidence.get(2).unwrap()["payload"], "x".repeat(200));
}

#[test]
fn journal_backed_outbox_transfers_controls_without_acknowledging_recording() {
    use ubm_desktop::continuation_journal::{JournalQuota, JournalRegistry};
    let directory =
        std::env::temp_dir().join(format!("ubm-legacy-evidence-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 100,
    };
    let registry = JournalRegistry::default();
    #[cfg(windows)]
    registry
        .configure_directory_with_scan_recovery_storage(
            &directory,
            Arc::new(
                ubm_desktop::continuation_journal::WindowsCurrentUserScanRecoveryStorageProtector::new(),
            ),
        )
        .unwrap();
    #[cfg(not(windows))]
    registry.configure_directory(&directory).unwrap();
    let journal = registry.open("recording", &json!({}), quota).unwrap();
    let outbox = Outbox::new(8, Arc::new(Wake));
    outbox.attach_journal(journal.clone(), json!({})).unwrap();
    outbox.register_journal_consumer("c", json!({})).unwrap();
    let original = json!({"t":"value", "consumer":"c", "valueB64":"AQ==", "provenance":"unknown"});
    outbox.push_data(original.clone()).unwrap();
    outbox.push_control(json!({"t":"cleanup"}));
    let evidence = LegacyEvidenceOwner::new(0, 4096);
    assert_eq!(
        outbox.drain_legacy_preserving(&evidence, 1, 1, project),
        Err(LegacyEvidenceError::Capacity)
    );
    assert!(evidence.ordinals(0, 10).is_empty());
    assert!(evidence.set_limits(1, 4096));
    let batch = outbox
        .drain_legacy_preserving(&evidence, 1, 1, project)
        .unwrap();
    assert_eq!(batch["records"][0]["t"], "cleanup");
    assert!(evidence.get(1).is_none());
    assert_eq!(evidence.get(2).unwrap()["t"], "cleanup");
    let page = journal.prepare(10, 4096).unwrap();
    assert!(
        page["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["record"] == original)
    );
    drop(outbox);
    assert_eq!(evidence.get(2).unwrap()["t"], "cleanup");
    assert_eq!(journal.prepare(10, 4096).unwrap(), page);
    assert!(evidence.retire(2));
    assert_eq!(journal.prepare(10, 4096).unwrap(), page);
    drop(journal);
    registry.release_idle_after_host_shutdown().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
