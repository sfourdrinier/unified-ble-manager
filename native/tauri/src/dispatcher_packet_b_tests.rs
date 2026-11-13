//! PR #210 packet B acceptance: the Tauri dispatcher over one shared
//! `DesktopCentral` on a scripted `FakeRadio` (no hardware).
//!
//! Every test runs the real dispatcher against the real core, holds radio
//! calls open with deterministic `FakeRadio` gates, and asserts delivered
//! data — the events the renderer's channel actually received, the radio
//! calls actually made, the mappings actually kept — never just a handle.
//! Evidence level: deterministic core + dispatcher; not physical-radio proof.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::Notify;
use ubm_core::contracts::{BleErrorCode, CommitState};
use ubm_desktop::central::{
    DurablePeerFailureProducerAck, DurablePeerFailureRecoveryOccurrence,
    DurablePeerFailureRecoveryRecipient, PeerFailureRecoveryLedger,
};
use ubm_desktop::continuation_journal::{JournalError, JournalQuota, ScanRecoveryStorageProtector};
use ubm_desktop::continuation_outbox::{Outbox, WakeSink};
use ubm_desktop::{
    AdapterAuthorization, AdapterPowerState, AdapterResetEvent, AdmissionPolicy, CentralProfile,
    CharacteristicSnapshot, DeliveryMode, DescriptorSnapshot, DesktopCentral, DesktopError,
    FakeRadio, FaultOp, LifecycleEvent, LifecycleKind, ObservedDelivery, OpControl, OpTicket,
    OperationId, PeerSnapshot, PlatformDetail, PlatformValue, PropertyFlags, RadioEvent,
    Retryability, ServiceSnapshot, WriteLimits,
};

use super::{
    attachment_identity_matches, attachment_record, caller_key, into_object, object,
    required_value, string, Attachment, AuthorityOpener, BtleplugDispatcher, CallerState,
    DispatchError, IpcEventSink, IpcValue, OrphanResource, ReleasePhase,
    RendererPeerFailureRecoveryHandle, RendererScanFailureRecoveryHandle, StreamEnd,
    MAX_PENDING_EVENTS,
};
use crate::desktop_core::{CoreAuthority, CoreSelector};
use crate::{AuthenticatedCaller, IpcDispatcher};

#[path = "../../../crates/test-support/recording_fixture.rs"]
mod recording_fixture;

const HRM_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
/// Notify-only (the Polar H10 heart-rate measurement shape).
const NOTIFY_ONLY: &str = "00002a37-0000-1000-8000-00805f9b34fb";
/// Indicate-only.
const INDICATE_ONLY: &str = "00002a38-0000-1000-8000-00805f9b34fb";
/// Readable and writable with response.
const CONTROL_POINT: &str = "00002a39-0000-1000-8000-00805f9b34fb";
const LEASE_ID: &str = "lease-1";
const LEASE_GENERATION: &str = "generation-1";
const WAIT: Duration = Duration::from_secs(5);

struct ScanRecoveryTestWake;

impl WakeSink for ScanRecoveryTestWake {
    fn wake(&self, _session_id: u64) {}
}

/// The dispatcher accepts only a host-supplied storage protector. This test
/// provider is deliberately inert because the fixture's private directory is
/// created by the test process; it still exercises the real trusted-host API
/// rather than the ordinary unprotected recording configuration.
struct ScanRecoveryTestProtector;

impl ScanRecoveryStorageProtector for ScanRecoveryTestProtector {
    fn provision(&self, _path: &std::path::Path) -> Result<(), JournalError> {
        Ok(())
    }

    fn validate(&self, _path: &std::path::Path) -> Result<(), JournalError> {
        Ok(())
    }
}

fn scan_recovery_test_directory(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "ubm-tauri-scan-recovery-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

fn create_private_fixture_directory(path: &std::path::Path) {
    std::fs::create_dir(path).expect("private fixture directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("secure private fixture directory");
    }
}

fn scan_recovery_test_limits() -> ubm_core::oversized_evidence::ScanFailureLimits {
    ubm_core::oversized_evidence::ScanFailureLimits {
        evidence: ubm_core::oversized_evidence::EvidenceLimits {
            max_depth: 8,
            max_tokens: 8,
            max_encoded_bytes: 4096,
            max_chunks: 8,
            chunk_bytes: 64,
        },
        max_recipients: 1,
        max_bookkeeping_bytes: 4096,
    }
}

/// Create a v2 sidecar only through the public outbox lifecycle. The simulated
/// renderer receives only a recording ID, never a capability or sidecar key.
fn record_prepared_scan_recovery(harness: &Harness, recording_id: &str, generation: u64) {
    use ubm_core::oversized_evidence::{
        EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
        ScanFailureOccurrence,
    };

    let journal = harness
        .dispatcher
        .recordings
        .open(
            recording_id,
            &serde_json::json!({}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 8,
            },
        )
        .expect("recording journal");
    let outbox = Outbox::new(generation + 1, Arc::new(ScanRecoveryTestWake));
    outbox
        .attach_journal(Arc::clone(&journal), serde_json::json!({}))
        .expect("durable outbox");
    outbox
        .configure_scan_failure_recovery_authority([0xA5; 32])
        .expect("recovery authority before capture");
    let byte = u8::try_from(generation % 251).expect("bounded test generation");
    let mut reservation = outbox
        .reserve_scan_failure(scan_recovery_test_limits())
        .expect("reserve scan failure");
    let mut source = Some(ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [byte; 16],
            scan: [byte.wrapping_add(1); 16],
            start: [byte.wrapping_add(2); 16],
            source_generation: [byte.wrapping_add(3); 16],
            occurrence: [byte.wrapping_add(4); 16],
        },
        vec![ScanFailureMember {
            recipient: [byte.wrapping_add(5); 16],
            delivery: [byte.wrapping_add(6); 16],
        }],
        Arc::new(EvidenceSource::new(vec![
            EvidenceToken::Array(3),
            EvidenceToken::Text(format!("durable-scan-recovery-{generation}")),
            EvidenceToken::Null,
            EvidenceToken::Null,
        ])),
    ));
    outbox
        .commit_scan_failure(&mut reservation, &mut source)
        .expect("capture and prepare durable scan source");
    assert!(source.is_none(), "durable capture consumes its source");
    outbox
        .finish_scan_failure_producer(&mut reservation)
        .expect("finish scan failure producer");
}

async fn complete_scan_recovery_through_tauri_routes(
    harness: &Harness,
    recording_id: &str,
    correlation_prefix: &str,
) {
    harness
        .dispatcher
        .enable_scan_failure_recovery_for(harness.caller.clone(), recording_id)
        .await
        .expect("trusted host grants recording recovery");
    let recovery = harness
        .route(
            "scanFailureRecovery.list",
            &format!("{correlation_prefix}-list"),
            vec![],
            None,
        )
        .await
        .expect("renderer lists granted recovery");
    let IpcValue::Array(recoveries) = field(&recovery, "recoveries") else {
        panic!("recovery aliases")
    };
    let recovery_id = text(&recoveries[0], "handleId");
    let owners = harness
        .route(
            "scanFailureRecovery.owners",
            &format!("{correlation_prefix}-owners"),
            vec![("recoveryId", string(recovery_id.clone()))],
            None,
        )
        .await
        .expect("renderer lists recovered owner");
    let IpcValue::Array(owners) = field(&owners, "owners") else {
        panic!("owner aliases")
    };
    let owner_id = text(&owners[0], "handleId");
    let head = harness
        .route(
            "scanFailureRecovery.head",
            &format!("{correlation_prefix}-head"),
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(owner_id)),
                ("index", number(0)),
            ],
            None,
        )
        .await
        .expect("renderer opens recovered head");
    let head_id = text(field(&head, "head"), "handleId");
    let page = harness
        .route(
            "scanFailureRecovery.read",
            &format!("{correlation_prefix}-read"),
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
                ("offset", string("0")),
            ],
            None,
        )
        .await
        .expect("renderer reads durable recovered bytes");
    assert!(matches!(field(&page, "bytes"), IpcValue::Bytes(bytes) if !bytes.is_empty()));
    for command in [
        "scanFailureRecovery.ack",
        "scanFailureRecovery.retire",
        "scanFailureRecovery.release",
    ] {
        harness
            .route(
                command,
                &format!("{correlation_prefix}-{command}"),
                vec![
                    ("recoveryId", string(recovery_id.clone())),
                    ("handleId", string(head_id.clone())),
                ],
                None,
            )
            .await
            .expect("durable lifecycle transition");
    }
    let finalized = harness
        .route(
            "scanFailureRecovery.finalize",
            &format!("{correlation_prefix}-finalize"),
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id)),
            ],
            None,
        )
        .await
        .expect("renderer finalizes released recovery");
    let cleanup_id = text(&finalized, "cleanupReceiptId");
    harness
        .route(
            "scanFailureRecovery.cleanup.retire",
            &format!("{correlation_prefix}-cleanup"),
            vec![
                ("recoveryId", string(recovery_id)),
                ("handleId", string(cleanup_id)),
            ],
            None,
        )
        .await
        .expect("renderer retires exact cleanup receipt");
}

fn test_recovery_handle(owner_generation: u64) -> RendererPeerFailureRecoveryHandle {
    RendererPeerFailureRecoveryHandle {
        owner_generation,
        descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor {
            protocol: 1,
            representation: ubm_core::oversized_evidence::PEER_SOURCE_FAILURE_REPRESENTATION,
            identity: ubm_core::oversized_evidence::PeerSourceFailureIdentity {
                host: [1; 16],
                peer: [2; 16],
                source_generation: [3; 16],
                native_group: [4; 16],
                transfer: [5; 16],
                recovery: [6; 16],
            },
            encoded_length: 1,
            chunk_bytes: 1,
        },
        peer_id: "peer".into(),
        selector: CoreSelector {
            service_uuid: HRM_SERVICE.into(),
            service_occurrence: Some(0),
            characteristic_uuid: Some(NOTIFY_ONLY.into()),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        },
        consumer: "consumer".into(),
        recipient_lease: "lease".into(),
    }
}

/// Exercise scan-failure recovery through the same protocol-10 dispatcher
/// route that a Tauri webview uses. The durable owner is created through the
/// public outbox lifecycle and the trusted registry's v2 sidecar boundary;
/// the test never manufactures a recovery owner or capability.
#[tokio::test]
async fn scan_failure_recovery_routes_use_real_v2_sidecar_and_revoke_forged_or_stale_handles() {
    use ubm_core::oversized_evidence::{
        EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureMember,
        ScanFailureOccurrence,
    };

    let harness = Harness::new().await;
    let directory = std::env::temp_dir().join(format!(
        "ubm-tauri-scan-recovery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    create_private_fixture_directory(&directory);
    let recording_id = "tauri-scan-recovery";
    let quota = JournalQuota {
        max_bytes: 1 << 20,
        max_records: 8,
    };
    harness
        .dispatcher
        .recordings
        .configure_directory_with_scan_recovery_storage(
            &directory,
            Arc::new(ScanRecoveryTestProtector),
        )
        .expect("trusted registry configuration");
    let journal = harness
        .dispatcher
        .recordings
        .open(recording_id, &serde_json::json!({}), quota)
        .expect("recording journal");
    let outbox = Outbox::new(41, Arc::new(ScanRecoveryTestWake));
    outbox
        .attach_journal(Arc::clone(&journal), serde_json::json!({}))
        .expect("durable outbox");
    outbox
        .configure_scan_failure_recovery_authority([0xA5; 32])
        .expect("recovery authority before capture");
    let limits = ubm_core::oversized_evidence::ScanFailureLimits {
        evidence: EvidenceLimits {
            max_depth: 8,
            max_tokens: 8,
            max_encoded_bytes: 4096,
            max_chunks: 8,
            chunk_bytes: 64,
        },
        max_recipients: 1,
        max_bookkeeping_bytes: 4096,
    };
    // Keep the unresolved raw cut first in source order. Raw inspection is
    // intentionally head-only, so a later prepared occurrence must not let a
    // renderer skip the captured failure that blocked the scan producer.
    let mut raw_reservation = outbox
        .reserve_scan_failure(limits)
        .expect("reserve unresolved raw source");
    let mut raw_source = Some(ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [11; 16],
            scan: [12; 16],
            start: [13; 16],
            source_generation: [14; 16],
            occurrence: [15; 16],
        },
        vec![ScanFailureMember {
            recipient: [16; 16],
            delivery: [17; 16],
        }],
        Arc::new(EvidenceSource::new(vec![EvidenceToken::Text(
            "unresolved-raw-source".to_owned(),
        )])),
    ));
    assert!(
        outbox
            .commit_scan_failure(&mut raw_reservation, &mut raw_source)
            .is_err(),
        "an unresolved source stays recoverable as raw evidence"
    );
    assert!(
        raw_source.is_none(),
        "the raw cut is durable before preparation fails"
    );
    outbox
        .finish_scan_failure_producer(&mut raw_reservation)
        .expect("finish unresolved producer");
    drop(raw_reservation);
    let mut reservation = outbox
        .reserve_scan_failure(limits)
        .expect("reserve scan failure");
    let mut source = Some(ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: [5; 16],
        },
        vec![ScanFailureMember {
            recipient: [6; 16],
            delivery: [7; 16],
        }],
        Arc::new(EvidenceSource::new(vec![
            EvidenceToken::Array(3),
            EvidenceToken::Text("raw-scan-failure".to_owned()),
            EvidenceToken::Null,
            EvidenceToken::Null,
        ])),
    ));
    outbox
        .commit_scan_failure(&mut reservation, &mut source)
        .expect("capture and prepare durable scan source");
    assert!(
        source.is_none(),
        "capture takes the exact source only after durable write"
    );
    outbox
        .finish_scan_failure_producer(&mut reservation)
        .expect("producer hands durable source to recovery");
    drop(reservation);
    drop(outbox);
    drop(journal);

    let other = harness.other_caller().await;
    let denied = other
        .route("scanFailureRecovery.list", "scan-denied", vec![], None)
        .await
        .expect_err("a non-granted caller cannot enumerate recovery");
    assert_eq!(denied.code, BleErrorCode::OwnershipDenied);

    harness
        .dispatcher
        .enable_scan_failure_recovery_for(harness.caller.clone(), recording_id)
        .await
        .expect("trusted host grants one protocol-10 caller");
    let listed = harness
        .route("scanFailureRecovery.list", "scan-list", vec![], None)
        .await
        .expect("granted caller lists opaque recovery aliases");
    let IpcValue::Array(recoveries) = field(&listed, "recoveries") else {
        panic!("recovery list")
    };
    assert_eq!(recoveries.len(), 1);
    let recovery_id = text(&recoveries[0], "handleId");

    let forged = harness
        .route(
            "scanFailureRecovery.owners",
            "scan-forged",
            vec![("recoveryId", string("forged"))],
            None,
        )
        .await
        .expect_err("opaque aliases are caller-bound and unguessable");
    assert_eq!(forged.code, BleErrorCode::OwnershipDenied);

    let owners = harness
        .route(
            "scanFailureRecovery.owners",
            "scan-owners",
            vec![("recoveryId", string(recovery_id.clone()))],
            None,
        )
        .await
        .expect("owner inventory");
    let IpcValue::Array(owners) = field(&owners, "owners") else {
        panic!("owner inventory")
    };
    assert_eq!(owners.len(), 2);
    let prepared_owner = owners
        .iter()
        .find(|owner| field(owner, "prepared") == &IpcValue::Bool(true))
        .expect("prepared owner");
    let raw_owner = owners
        .iter()
        .find(|owner| field(owner, "prepared") == &IpcValue::Bool(false))
        .expect("unresolved raw owner");
    let owner_id = text(prepared_owner, "handleId");
    let raw_owner_id = text(raw_owner, "handleId");
    assert_eq!(field(prepared_owner, "memberCount"), &number(1));

    // Inventory is observational. Repeating it must preserve live opaque
    // aliases instead of retaining an unbounded new set for the same durable
    // owners.
    let owners_again = harness
        .route(
            "scanFailureRecovery.owners",
            "scan-owners-again",
            vec![("recoveryId", string(recovery_id.clone()))],
            None,
        )
        .await
        .expect("repeated owner inventory");
    let IpcValue::Array(owners_again) = field(&owners_again, "owners") else {
        panic!("repeated owner inventory")
    };
    assert_eq!(
        owners
            .iter()
            .map(|owner| text(owner, "handleId"))
            .collect::<Vec<_>>(),
        owners_again
            .iter()
            .map(|owner| text(owner, "handleId"))
            .collect::<Vec<_>>(),
        "repeated inventory reuses the same live aliases"
    );

    let raw_info = harness
        .route(
            "scanFailureRecovery.raw.info",
            "scan-raw-info",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id.clone())),
            ],
            None,
        )
        .await
        .expect("raw owner inventory stays behind the trusted grant");
    let raw_identity = field(field(&raw_info, "info"), "identity");
    assert_eq!(text(raw_identity, "host"), "0b".repeat(16));

    for max_bytes in [0, 65_537] {
        let error = harness
            .route(
                "scanFailureRecovery.raw.token",
                &format!("scan-raw-token-bounded-{max_bytes}"),
                vec![
                    ("recoveryId", string(recovery_id.clone())),
                    ("handleId", string(raw_owner_id.clone())),
                    ("index", number(0)),
                    ("offset", number(0)),
                    ("maxBytes", number(max_bytes)),
                ],
                None,
            )
            .await
            .expect_err("native route rejects an invalid raw token window before core access");
        assert_eq!(
            error.identity().0,
            "argument.invalid",
            "maxBytes={max_bytes}, identity={:?}",
            error.identity()
        );
    }

    let member = harness
        .route(
            "scanFailureRecovery.member",
            "scan-member",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id.clone())),
                ("index", number(0)),
            ],
            None,
        )
        .await
        .expect("member projection");
    assert_eq!(text(field(&member, "member"), "recipient"), "10".repeat(16));

    let raw_token = harness
        .route(
            "scanFailureRecovery.raw.token",
            "scan-raw-token",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id.clone())),
                ("index", number(0)),
                ("offset", number(0)),
                ("maxBytes", number(64)),
            ],
            None,
        )
        .await
        .expect("raw token projection");
    assert_eq!(text(field(&raw_token, "token"), "kind"), "text");

    // The raw cut is the current source fence. Retire and finalize it before
    // asking for the next prepared owner, proving a renderer cannot jump the
    // unresolved prefix while still allowing an authenticated cleanup.
    let raw_retired = harness
        .route(
            "scanFailureRecovery.raw.retire",
            "scan-raw-retire",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id.clone())),
            ],
            None,
        )
        .await
        .expect("retire unresolved raw cut");
    assert_eq!(field(&raw_retired, "completed"), &IpcValue::Bool(true));
    let raw_released = harness
        .route(
            "scanFailureRecovery.raw.release",
            "scan-raw-release",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id.clone())),
            ],
            None,
        )
        .await
        .expect("release unresolved raw cut");
    assert_eq!(field(&raw_released, "completed"), &IpcValue::Bool(true));

    // Fill the renderer alias table after the owner has been retired and
    // released. Finalization must refuse before creating a durable cleanup
    // receipt; after freeing capacity, the same handle finalizes successfully.
    let recovery_key = harness.key();
    let recovery_generation = harness
        .dispatcher
        .scan_failure_recovery_admission
        .lock()
        .expect("scan recovery admission")
        .get(&recovery_key)
        .expect("scan recovery grant")
        .generation;
    {
        let mut aliases = harness
            .dispatcher
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery aliases");
        let handles = aliases.entry(recovery_key.clone()).or_default();
        while handles.len() < MAX_PENDING_EVENTS {
            let id = format!("capacity-{}", handles.len());
            handles.insert(
                id,
                (
                    recovery_generation,
                    RendererScanFailureRecoveryHandle::ReservedCleanup,
                ),
            );
        }
    }
    let saturated = harness
        .route(
            "scanFailureRecovery.raw.finalize",
            "scan-raw-finalize-saturated",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id.clone())),
            ],
            None,
        )
        .await
        .expect_err("capacity is admitted before raw durable finalization");
    assert_eq!(saturated.identity().0, "stream.quota");
    harness
        .dispatcher
        .scan_failure_recovery_handles
        .lock()
        .expect("scan recovery aliases")
        .get_mut(&recovery_key)
        .expect("scan recovery aliases remain")
        .retain(|id, _| !id.starts_with("capacity-"));
    let cleanup_before_finalization = harness
        .route(
            "scanFailureRecovery.cleanup.list",
            "scan-cleanup-list-before-raw-finalize",
            vec![("recoveryId", string(recovery_id.clone()))],
            None,
        )
        .await
        .expect("failed finalization leaves no cleanup receipt");
    assert!(
        matches!(field(&cleanup_before_finalization, "cleanupReceiptIds"), IpcValue::Array(values) if values.is_empty())
    );

    let raw_finalized = harness
        .route(
            "scanFailureRecovery.raw.finalize",
            "scan-raw-finalize",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_owner_id)),
            ],
            None,
        )
        .await
        .expect("finalize unresolved raw cut");
    let raw_cleanup_id = text(&raw_finalized, "cleanupReceiptId");
    let raw_cleanup_retired = harness
        .route(
            "scanFailureRecovery.cleanup.retire",
            "scan-raw-cleanup-retire",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(raw_cleanup_id)),
            ],
            None,
        )
        .await
        .expect("retire raw cleanup receipt before advancing source fence");
    assert_eq!(
        field(&raw_cleanup_retired, "completed"),
        &IpcValue::Bool(true)
    );

    let head = harness
        .route(
            "scanFailureRecovery.head",
            "scan-head",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(owner_id.clone())),
                ("index", number(0)),
            ],
            None,
        )
        .await
        .expect("head projection");
    let head = field(&head, "head");
    let head_id = text(head, "handleId");
    assert_eq!(field(head, "contiguousRead"), &string("0"));
    assert_eq!(field(head, "acknowledged"), &IpcValue::Bool(false));

    let page = harness
        .route(
            "scanFailureRecovery.read",
            "scan-read",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
                ("offset", string("0")),
            ],
            None,
        )
        .await
        .expect("read durable evidence through opaque head");
    assert!(matches!(field(&page, "bytes"), IpcValue::Bytes(bytes) if !bytes.is_empty()));
    assert_eq!(field(&page, "done"), &IpcValue::Bool(true));

    harness
        .route(
            "scanFailureRecovery.ack",
            "scan-ack",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
            ],
            None,
        )
        .await
        .expect("acknowledge exact recovered evidence");
    let retired = harness
        .route(
            "scanFailureRecovery.retire",
            "scan-retire",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
            ],
            None,
        )
        .await
        .expect("retire exact recovered evidence");
    assert_eq!(field(&retired, "completed"), &IpcValue::Bool(true));
    let released = harness
        .route(
            "scanFailureRecovery.release",
            "scan-release",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
            ],
            None,
        )
        .await
        .expect("release after explicit producer finish");
    assert_eq!(field(&released, "completed"), &IpcValue::Bool(true));

    {
        let mut aliases = harness
            .dispatcher
            .scan_failure_recovery_handles
            .lock()
            .expect("scan recovery aliases");
        let handles = aliases.entry(recovery_key.clone()).or_default();
        while handles.len() < MAX_PENDING_EVENTS {
            let id = format!("head-capacity-{}", handles.len());
            handles.insert(
                id,
                (
                    recovery_generation,
                    RendererScanFailureRecoveryHandle::ReservedCleanup,
                ),
            );
        }
    }
    let saturated_head = harness
        .route(
            "scanFailureRecovery.finalize",
            "scan-finalize-saturated",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
            ],
            None,
        )
        .await
        .expect_err("capacity is admitted before head durable finalization");
    assert_eq!(saturated_head.identity().0, "stream.quota");
    harness
        .dispatcher
        .scan_failure_recovery_handles
        .lock()
        .expect("scan recovery aliases")
        .get_mut(&recovery_key)
        .expect("scan recovery aliases remain")
        .retain(|id, _| !id.starts_with("head-capacity-"));

    let finalized = harness
        .route(
            "scanFailureRecovery.finalize",
            "scan-finalize",
            vec![
                ("recoveryId", string(recovery_id.clone())),
                ("handleId", string(head_id.clone())),
            ],
            None,
        )
        .await
        .expect("finalize produces an opaque cleanup alias");
    assert!(
        !text(&finalized, "cleanupReceiptId").is_empty(),
        "finalization returns an opaque cleanup alias"
    );
    let cleanup = harness
        .route(
            "scanFailureRecovery.cleanup.list",
            "scan-cleanup-list",
            vec![("recoveryId", string(recovery_id.clone()))],
            None,
        )
        .await
        .expect("cleanup receipts remain explicitly inspectable");
    let IpcValue::Array(cleanups) = field(&cleanup, "cleanupReceiptIds") else {
        panic!("cleanup receipt aliases")
    };
    let cleanup_again = harness
        .route(
            "scanFailureRecovery.cleanup.list",
            "scan-cleanup-list-again",
            vec![("recoveryId", string(recovery_id.clone()))],
            None,
        )
        .await
        .expect("repeated cleanup inventory");
    assert_eq!(
        field(&cleanup, "cleanupReceiptIds"),
        field(&cleanup_again, "cleanupReceiptIds"),
        "repeated cleanup inventory reuses the same live aliases"
    );
    // The durable cleanup receipt remains opaque to the renderer.
    let cleanup_id = cleanups
        .iter()
        .find_map(|value| match value {
            IpcValue::String(value) => Some(value.clone()),
            _ => None,
        })
        .expect("opaque cleanup alias");
    let cleanup_retired = harness
        .route(
            "scanFailureRecovery.cleanup.retire",
            "scan-cleanup-retire",
            vec![
                ("recoveryId", string(recovery_id)),
                ("handleId", string(cleanup_id)),
            ],
            None,
        )
        .await
        .expect("authenticated cleanup retirement");
    assert_eq!(field(&cleanup_retired, "completed"), &IpcValue::Bool(true));

    // Ordinary IPC release revokes recovery before attempting unrelated radio
    // cleanup. A failed disconnect leaves normal cleanup debt behind, but it
    // must not leave the old recovery grant or its opaque aliases usable.
    let _retained_link = harness.connect("scan-release-failure").await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "retain caller cleanup debt");
    let ordinary_release = harness
        .dispatcher
        .release_request(
            harness.caller.clone(),
            BTreeMap::from([(
                "rendererLease".to_owned(),
                object([
                    ("leaseId", string(LEASE_ID)),
                    ("generation", string(LEASE_GENERATION)),
                ]),
            )]),
        )
        .await
        .expect("ordinary release returns its cleanup outcome");
    assert!(
        !super::is_released(field(&ordinary_release, "cleanup")),
        "the injected disconnect failure retains normal cleanup debt"
    );
    let stale = harness
        .route("scanFailureRecovery.list", "scan-stale", vec![], None)
        .await
        .expect_err("ordinary release revokes recovery before retained cleanup can be retried");
    assert_eq!(stale.code, BleErrorCode::OwnershipDenied);
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test]
async fn scan_failure_recovery_reclaims_alias_capacity_after_256_completed_durable_lifecycles() {
    let harness = Harness::new().await;
    let directory = scan_recovery_test_directory("alias-capacity");
    create_private_fixture_directory(&directory);
    harness
        .dispatcher
        .recordings
        .configure_directory_with_scan_recovery_storage(
            &directory,
            Arc::new(ScanRecoveryTestProtector),
        )
        .expect("trusted registry configuration");

    // One extra lifecycle proves aliases are retired with their completed
    // durable source, rather than merely surviving below a 256-entry cap.
    for generation in 0..=256_u64 {
        let recording_id = format!("tauri-scan-capacity-{generation}");
        record_prepared_scan_recovery(&harness, &recording_id, generation);
        complete_scan_recovery_through_tauri_routes(
            &harness,
            &recording_id,
            &format!("scan-capacity-{generation}"),
        )
        .await;
    }
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test]
async fn scan_failure_recovery_tauri_routes_survive_fresh_process_restart() {
    const CHILD_DIRECTORY: &str = "UBM_TAURI_SCAN_RECOVERY_CHILD_DIRECTORY";
    const CHILD_RECORDING: &str = "UBM_TAURI_SCAN_RECOVERY_CHILD_RECORDING";
    const CHILD_PHASE: &str = "UBM_TAURI_SCAN_RECOVERY_CHILD_PHASE";
    let recording_id = "tauri-scan-fresh-process";

    if let Ok(directory) = std::env::var(CHILD_DIRECTORY) {
        let harness = Harness::new().await;
        harness
            .dispatcher
            .recordings
            .configure_directory_with_scan_recovery_storage(
                std::path::Path::new(&directory),
                Arc::new(ScanRecoveryTestProtector),
            )
            .expect("fresh process configures trusted registry");
        let recording_id = std::env::var(CHILD_RECORDING).expect("child recording id");
        if std::env::var(CHILD_PHASE).as_deref() == Ok("produce") {
            record_prepared_scan_recovery(&harness, &recording_id, 900);
            return;
        }
        harness
            .dispatcher
            .recordings
            .open(
                &recording_id,
                &serde_json::json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 8,
                },
            )
            .expect("fresh process reopens durable recording");
        complete_scan_recovery_through_tauri_routes(&harness, &recording_id, "fresh-process").await;
        return;
    }

    let directory = scan_recovery_test_directory("fresh-process");
    create_private_fixture_directory(&directory);
    let executable = std::env::current_exe().expect("test executable");
    let test_name = "btleplug_dispatcher::dispatcher_packet_b_tests::scan_failure_recovery_tauri_routes_survive_fresh_process_restart";
    let produced = std::process::Command::new(&executable)
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_DIRECTORY, &directory)
        .env(CHILD_RECORDING, recording_id)
        .env(CHILD_PHASE, "produce")
        .status()
        .expect("producer test process starts");
    assert!(
        produced.success(),
        "producer process commits durable recovery"
    );
    let recovered = std::process::Command::new(executable)
        .arg("--exact")
        .arg(test_name)
        .env(CHILD_DIRECTORY, &directory)
        .env(CHILD_RECORDING, recording_id)
        .env(CHILD_PHASE, "recover")
        .status()
        .expect("fresh recovery test process starts");
    assert!(
        recovered.success(),
        "fresh process completes durable Tauri recovery"
    );
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test]
async fn terminal_evidence_recipient_recovery_survives_a_real_process_restart() {
    const DIRECTORY: &str = "UBM_TAURI_TERMINAL_EVIDENCE_CHILD_DIRECTORY";
    const PHASE: &str = "UBM_TAURI_TERMINAL_EVIDENCE_CHILD_PHASE";
    const RECIPIENT: &str = "test-app\0main";
    if let Ok(directory) = std::env::var(DIRECTORY) {
        let ledger = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &std::path::Path::new(&directory).join("terminal.sqlite"),
            1024,
        )
        .expect("open ledger");
        let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
            producer: "subscription".to_owned(),
            manager_id: "manager".to_owned(),
            operation_id: "operation".to_owned(),
            incarnation: "incarnation".to_owned(),
            scope_utf16le: None,
        };
        if std::env::var(PHASE).as_deref() == Ok("produce") {
            let reservation = ledger.reserve(&scope, 512).expect("reserve");
            let bytes = br#"{"formatVersion":1,"terminal":{"droppedBytes":23,"droppedItems":1,"kind":"terminal","reason":"overflow","replacedItems":0}}"#;
            ledger
                .stage(&reservation.reservation_id, RECIPIENT, bytes)
                .expect("stage owner");
            return;
        }
        let harness = Harness::new().await;
        let dispatcher = BtleplugDispatcher::with_core_authority(
            Arc::new(harness.central.clone()) as Arc<dyn CoreAuthority>
        );
        *dispatcher
            .terminal_evidence_ledger
            .lock()
            .expect("ledger mutex") = Some(Arc::new(ledger));
        let caller = AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned());
        dispatcher
            .bootstrap(
                caller.clone(),
                IpcEventSink::new(Channel::new(|_| Ok(()))),
                version_offer(),
            )
            .await
            .expect("cold bootstrap resolves staged receipt");
        let recovered = dispatcher
            .terminal_evidence_route(
                &caller,
                "terminalEvidence.recovery.list",
                &BTreeMap::from([("version".to_owned(), number(1))]),
                None,
            )
            .await
            .expect("route lists committed staged recovery")
            .into_wire();
        assert_eq!(recovered["records"].as_array().expect("records").len(), 1);
        return;
    }
    let directory = scan_recovery_test_directory("terminal-evidence-process");
    std::fs::create_dir(&directory).expect("fixture directory");
    let executable = std::env::current_exe().expect("test executable");
    let test_name = "btleplug_dispatcher::dispatcher_packet_b_tests::terminal_evidence_recipient_recovery_survives_a_real_process_restart";
    for phase in ["produce", "recover"] {
        let status = std::process::Command::new(&executable)
            .arg("--exact")
            .arg(test_name)
            .env(DIRECTORY, &directory)
            .env(PHASE, phase)
            .status()
            .expect("child starts");
        assert!(status.success(), "{phase} child succeeds");
    }
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test]
async fn durable_peer_failure_routes_require_trusted_owner_grant_and_teardown_revokes_it() {
    let harness = Harness::new().await;
    let denied = harness
        .route("peerFailureRecovery.list", "durable-denied", vec![], None)
        .await
        .expect_err("a renderer cannot enumerate recovery before trusted host opt-in");
    assert_eq!(denied.code, BleErrorCode::OwnershipDenied);
    assert_eq!(denied.domain, "restoration");

    harness
        .dispatcher
        .peer_failure_recovery_admission
        .lock()
        .expect("owner grant mutex")
        .entry(harness.key())
        .or_default()
        .owner_generation = Some(0);
    let listed = harness
        .route("peerFailureRecovery.list", "durable-granted", vec![], None)
        .await
        .expect("trusted owner can enumerate the attached durable ledger");
    assert_eq!(field(&listed, "recoveries"), &IpcValue::Array(vec![]));

    harness.dispatcher.release_caller(harness.caller.clone());
    let revoked = harness
        .route("peerFailureRecovery.list", "durable-revoked", vec![], None)
        .await
        .expect_err("caller teardown revokes durable recovery access");
    assert_eq!(revoked.code, BleErrorCode::OwnershipDenied);
}

/// A trusted host's attach may wait on durable storage.  Teardown (and a
/// replacement attachment for the same authenticated caller) must win that
/// race: an older attach completion must never restore a recovery grant.
#[tokio::test]
async fn durable_peer_failure_grant_refuses_a_released_caller_after_attach_waits() {
    let harness = Harness::new().await;
    let (started, observed) = tokio::sync::oneshot::channel();
    let (resume, release) = tokio::sync::oneshot::channel();
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let grant = tokio::spawn(async move {
        dispatcher
            .enable_peer_failure_recovery_for_after(caller, async move {
                started.send(()).expect("test waits for attach");
                release.await.expect("test resumes attach");
                Ok(())
            })
            .await
    });
    observed.await.expect("attach started before teardown");

    harness.dispatcher.release_caller(harness.caller.clone());
    resume.send(()).expect("resume attach after teardown");
    let error = grant
        .await
        .expect("grant task joins")
        .expect_err("a stale attachment must not recreate a caller grant");
    assert_eq!(error["code"], "ownership.denied");
    assert!(harness
        .dispatcher
        .peer_failure_recovery_admission
        .lock()
        .expect("owner grants")
        .get(&harness.key())
        .is_none_or(|entry| entry.owner_generation.is_none()));
}

/// A list result is computed outside the grant mutex.  The final insertion
/// must reject an old generation after release and re-bootstrap, rather than
/// leaving a handle that the replacement caller can use.
#[tokio::test]
async fn durable_peer_failure_list_insert_refuses_release_then_rebootstrap() {
    let harness = Harness::new().await;
    let key = harness.key();
    {
        let mut admission = harness
            .dispatcher
            .peer_failure_recovery_admission
            .lock()
            .expect("admission");
        admission.entry(key.clone()).or_default().owner_generation = Some(0);
    }
    let (_, old_generation) = harness
        .dispatcher
        .peer_failure_recovery_owner(&harness.caller, "test.list")
        .expect("old grant");
    harness.dispatcher.release_caller(harness.caller.clone());
    {
        let mut admission = harness
            .dispatcher
            .peer_failure_recovery_admission
            .lock()
            .expect("admission");
        let entry = admission.get_mut(&key).expect("revoked entry");
        entry.owner_generation = Some(entry.generation);
    }
    let error = harness
        .dispatcher
        .install_peer_failure_recovery_handles(
            key.clone(),
            old_generation,
            HashMap::from([("stale".into(), test_recovery_handle(old_generation))]),
        )
        .expect_err("old list completion cannot publish into replacement generation");
    assert_eq!(error.code, BleErrorCode::OwnershipDenied);
    assert!(harness
        .dispatcher
        .peer_failure_recovery_handles
        .lock()
        .expect("handles")
        .get(&key)
        .is_none());
}

/// ACK has one explicit linearization point.  Release before it denies with
/// no retained native operation; release after it cannot make the handle
/// usable by a caller that reuses the same authenticated key.
#[tokio::test]
async fn durable_peer_failure_ack_generation_fences_release_before_and_after_admission() {
    let harness = Harness::new().await;
    let key = harness.key();
    {
        let mut admission = harness
            .dispatcher
            .peer_failure_recovery_admission
            .lock()
            .expect("admission");
        admission.entry(key.clone()).or_default().owner_generation = Some(0);
    }
    harness
        .dispatcher
        .install_peer_failure_recovery_handles(
            key.clone(),
            0,
            HashMap::from([("before".into(), test_recovery_handle(0))]),
        )
        .expect("install first handle");
    harness.dispatcher.release_caller(harness.caller.clone());
    let error = match harness
        .dispatcher
        .reserve_peer_failure_recovery_acknowledgement(&key, "before")
    {
        Ok(_) => panic!("release before ACK admission never reaches native ACK"),
        Err(error) => error,
    };
    assert_eq!(error.code, BleErrorCode::OwnershipDenied);

    {
        let mut admission = harness
            .dispatcher
            .peer_failure_recovery_admission
            .lock()
            .expect("admission");
        let entry = admission.get_mut(&key).expect("entry");
        entry.owner_generation = Some(entry.generation);
        let generation = entry.generation;
        drop(admission);
        harness
            .dispatcher
            .install_peer_failure_recovery_handles(
                key.clone(),
                generation,
                HashMap::from([("after".into(), test_recovery_handle(generation))]),
            )
            .expect("install replacement handle");
    }
    let (generation, _) = harness
        .dispatcher
        .reserve_peer_failure_recovery_acknowledgement(&key, "after")
        .expect("ACK admitted before release");
    harness.dispatcher.release_caller(harness.caller.clone());
    assert!(
        harness
            .dispatcher
            .renderer_peer_failure_recovery_handle(&harness.caller, "after", "test.after")
            .is_err(),
        "released/rebootstrapped caller cannot reuse an in-flight old handle"
    );
    assert!(harness
        .dispatcher
        .peer_failure_recovery_admission
        .lock()
        .expect("admission")
        .get(&key)
        .expect("entry")
        .in_flight_acknowledgements
        .contains(&(generation, "after".into())));
    harness
        .dispatcher
        .finish_peer_failure_recovery_acknowledgement(&key, generation, "after");
}

/// Ownerless diagnostics are host facts: no caller at observation time, and a
/// failed later channel send, must both leave the exact record for a later
/// authenticated protocol-10 attachment.
#[tokio::test]
async fn attachment_diagnostics_replay_after_zero_caller_and_failed_send() {
    let harness = Harness::new().await;
    let _ = harness.dispatcher.release(&harness.key()).await;
    let diagnostic = object([
        ("kind", string("diagnostic-warning")),
        ("schemaVersion", number(1)),
        ("code", string("gatt.source-uncertain-lagged")),
        ("missed", number(3)),
    ]);
    harness
        .dispatcher
        .emit_attachment_diagnostic(diagnostic)
        .await;

    let refused = IpcEventSink::new(Channel::new(|_| {
        Err(tauri::Error::Io(std::io::Error::other(
            "scripted attachment refusal",
        )))
    }));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), refused, version_offer())
        .await
        .expect("failed replay does not discard a retained diagnostic or fail bootstrap");
    let _ = harness.dispatcher.release(&harness.key()).await;

    let events = Arc::clone(&harness.events);
    let accepted = IpcEventSink::new(Channel::new(move |body| {
        if let InvokeResponseBody::Json(json) = body {
            events
                .lock()
                .expect("event log")
                .push(serde_json::from_str(&json).expect("event json"));
        }
        Ok(())
    }));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), accepted, version_offer())
        .await
        .expect("later attachment");
    assert!(harness.items("attachment").iter().any(|item| {
        item["value"]["code"] == "gatt.source-uncertain-lagged" && item["value"]["missed"] == 3
    }));
}

/// Retention is deliberately bounded, but losing its oldest entry is itself a
/// replayable host fact.  A fresh protocol-10 attachment receives the loss
/// diagnostic before the surviving FIFO tail, rather than a misleadingly
/// complete-looking history.
#[tokio::test]
async fn attachment_diagnostics_overflow_replays_loss_then_surviving_fifo_tail() {
    let harness = Harness::new().await;
    let _ = harness.dispatcher.release(&harness.key()).await;
    for index in 0..=super::MAX_RETAINED_ATTACHMENT_DIAGNOSTICS {
        harness
            .dispatcher
            .emit_attachment_diagnostic(object([
                ("kind", string("diagnostic-warning")),
                ("schemaVersion", number(1)),
                ("code", string("gatt.source-uncertain-lagged")),
                ("missed", number(index as i64)),
            ]))
            .await;
    }

    let events = Arc::clone(&harness.events);
    let accepted = IpcEventSink::new(Channel::new(move |body| {
        if let InvokeResponseBody::Json(json) = body {
            events
                .lock()
                .expect("event log")
                .push(serde_json::from_str(&json).expect("event json"));
        }
        Ok(())
    }));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), accepted, version_offer())
        .await
        .expect("overflowed diagnostics replay to a fresh attachment");

    let items = harness.items("attachment");
    assert_eq!(items.len(), super::MAX_RETAINED_ATTACHMENT_DIAGNOSTICS + 1);
    assert_eq!(
        items[0]["value"]["code"],
        "gatt.source-uncertain-retention-lagged"
    );
    assert_eq!(items[0]["value"]["missed"], 1);
    assert_eq!(items[1]["value"]["missed"], 1);
    assert_eq!(
        items.last().expect("surviving tail")["value"]["missed"],
        super::MAX_RETAINED_ATTACHMENT_DIAGNOSTICS as i64
    );
}

/// A crash after the source-ACK intent is durable cannot recreate the native
/// source owner.  The real sidecar remains readable through the real Tauri
/// renderer routes, while ACK truthfully reports that platform reconciliation
/// is required instead of deleting the evidence or claiming a successful ACK.
#[tokio::test]
async fn durable_inflight_peer_failure_recovery_survives_restart_and_refuses_native_ack() {
    let module = module_path!().split_once("::").unwrap().1;
    let test = format!(
        "{module}::durable_inflight_peer_failure_recovery_survives_restart_and_refuses_native_ack"
    );
    let Some(directory) = recording_fixture::isolated_fixture_process(&test) else {
        return;
    };

    let harness = Harness::new().await;
    harness
        .dispatcher
        .continuation_configure_recording_directory(&directory)
        .await
        .expect("configure private recording directory");
    let journal = harness
        .dispatcher
        .recordings
        .open(
            "inflight-peer-failure",
            &serde_json::json!({"kind":"tauri-inflight-peer-failure"}),
            JournalQuota {
                max_bytes: 1 << 20,
                max_records: 8,
            },
        )
        .expect("open authorized recording");
    let ledger = journal
        .peer_failure_recovery_ledger()
        .expect("open peer failure sidecar");
    let bytes = vec![0x57, 0x48, 0x4f, 0x4f, 0x50];
    let occurrence = DurablePeerFailureRecoveryOccurrence {
        descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor {
            protocol: 1,
            representation: ubm_core::oversized_evidence::PEER_SOURCE_FAILURE_REPRESENTATION,
            identity: ubm_core::oversized_evidence::PeerSourceFailureIdentity {
                host: [1; 16],
                peer: [2; 16],
                source_generation: [3; 16],
                native_group: [4; 16],
                transfer: [5; 16],
                recovery: [6; 16],
            },
            encoded_length: bytes.len() as u64,
            chunk_bytes: 64 * 1024,
        },
        source_ack_id: None,
        recipients: vec![DurablePeerFailureRecoveryRecipient {
            scope: (
                "inflight-peer".into(),
                HRM_SERVICE.into(),
                0,
                NOTIFY_ONLY.into(),
                0,
            ),
            consumer: "original-consumer".into(),
            epoch: 41,
            recipient_lease: "original-recipient-lease".into(),
            contiguous_read: 0,
            acknowledged: false,
        }],
        producer_ack: DurablePeerFailureProducerAck::NotStarted,
        teardown: None,
    };
    ledger
        .begin_stage(&occurrence)
        .await
        .expect("persist recovery envelope");
    ledger
        .append_stage_chunk(&occurrence.descriptor, 0, &bytes, true)
        .await
        .expect("persist exact evidence bytes");
    ledger
        .commit_stage(&occurrence.descriptor)
        .await
        .expect("commit recovery evidence");
    ledger
        .mark_source_ack_inflight(&occurrence.descriptor, "opaque-native-source-ack")
        .await
        .expect("persist native source ACK intent before simulated restart");
    drop(ledger);

    let ungranted = harness.other_caller().await;
    let denied = ungranted
        .route(
            "peerFailureRecovery.list",
            "inflight-ungranted",
            vec![],
            None,
        )
        .await
        .expect_err("another renderer cannot discover a retained occurrence");
    assert_eq!(denied.code, BleErrorCode::OwnershipDenied);
    assert_eq!(denied.domain, "restoration");

    // This opens a new sidecar ledger and attaches it to the actual central;
    // no test double returns a recovery response directly.
    harness
        .dispatcher
        .enable_peer_failure_recovery_for(harness.caller.clone(), "inflight-peer-failure")
        .await
        .expect("trusted host attaches persisted recovery after restart");
    let listed = harness
        .route("peerFailureRecovery.list", "inflight-list", vec![], None)
        .await
        .expect("granted caller lists recovered occurrence");
    let recoveries = match field(&listed, "recoveries") {
        IpcValue::Array(recoveries) => recoveries,
        other => panic!("recoveries is not an array: {other:?}"),
    };
    assert_eq!(recoveries.len(), 1);
    let recovery = &recoveries[0];
    assert_eq!(text(recovery, "peerId"), "inflight-peer");
    assert_eq!(text(recovery, "producerAck"), "in-flight");
    let handle = text(recovery, "handleId");

    let stale = harness
        .route(
            "peerFailureRecovery.read",
            "inflight-stale",
            vec![
                ("handleId", string("not-a-recovery-handle")),
                ("offset", string("0")),
            ],
            None,
        )
        .await
        .expect_err("a guessed or stale opaque handle cannot read evidence");
    assert_eq!(stale.code, BleErrorCode::OwnershipDenied);
    assert_eq!(stale.domain, "restoration");

    let read = harness
        .route(
            "peerFailureRecovery.read",
            "inflight-read",
            vec![
                ("handleId", string(handle.clone())),
                ("offset", string("0")),
            ],
            None,
        )
        .await
        .expect("real Tauri IPC reads retained sidecar bytes");
    assert_eq!(field(&read, "bytes"), &IpcValue::Bytes(bytes.clone()));
    assert_eq!(field(&read, "done"), &IpcValue::Bool(true));

    let ack = harness
        .route(
            "peerFailureRecovery.ack",
            "inflight-ack",
            vec![("handleId", string(handle.clone()))],
            None,
        )
        .await
        .expect_err("a restored in-flight receipt cannot claim its lost native owner retired");
    assert_eq!(ack.code, BleErrorCode::CapabilityUnavailable);
    assert_eq!(ack.domain, "restoration");
    assert_eq!(
        ack.operation,
        "gatt.notification.source-failure-recovery.reconcile"
    );

    // The original opaque handle is intentionally no longer readable after
    // its recipient ACK is durable. A fresh list proves the occurrence was
    // not deleted by the unavailable native reconciliation; it remains
    // explicitly retained in its in-flight state for platform reconciliation.
    let retained = harness
        .route(
            "peerFailureRecovery.list",
            "inflight-retained",
            vec![],
            None,
        )
        .await
        .expect("failed native reconciliation leaves a durable receipt");
    let retained = match field(&retained, "recoveries") {
        IpcValue::Array(recoveries) => recoveries,
        other => panic!("recoveries is not an array: {other:?}"),
    };
    assert_eq!(retained.len(), 1);
    assert_eq!(text(&retained[0], "producerAck"), "in-flight");
    assert_eq!(field(&retained[0], "acknowledged"), &IpcValue::Bool(true));

    harness.dispatcher.release_caller(harness.caller.clone());
    let revoked = tokio::time::timeout(
        WAIT,
        harness.route("peerFailureRecovery.list", "inflight-revoked", vec![], None),
    )
    .await
    .expect("caller teardown must not strand its renderer route")
    .expect_err("caller teardown revokes recovered handles and the grant");
    assert_eq!(revoked.code, BleErrorCode::OwnershipDenied);
    // Grant revocation is synchronous; renderer retirement runs asynchronously.
    // Whichever authority observes the route first must refuse it explicitly.
    assert!(matches!(
        (revoked.domain, revoked.operation),
        ("ipc", "tauri.route-caller") | ("restoration", "tauri.peer-failure-recovery.list")
    ));
    harness.central.shutdown().await;
    recording_fixture::complete_fixture_process(&directory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_watch_retains_racing_event_order_without_regressing_to_an_unsequenced_snapshot() {
    for snapshot_bond in [
        ubm_desktop::BondState::Bonded,
        ubm_desktop::BondState::NotBonded,
    ] {
        let harness = Arc::new(Harness::new().await);
        harness.advertise("watch-race").await;
        harness.radio().set_security(
            "watch-race",
            ubm_desktop::SecurityState {
                bond: snapshot_bond,
                pairing_possible: Some(true),
            },
        );
        harness.radio().block_op(FaultOp::SecurityState);
        let mut observed = harness.central.security_events();
        let watching = Arc::clone(&harness);
        let subscribe = tokio::spawn(async move {
            watching
                .execute(
                    "security.watch.subscribe",
                    vec![("peerId", string("watch-race"))],
                    None,
                    OpControl::budget_ms(5000),
                )
                .await
        });
        tokio::time::timeout(WAIT, async {
            while count(&harness.radio().calls(), "security_state") == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("state read held");
        for bond in [
            ubm_desktop::BondState::NotBonded,
            ubm_desktop::BondState::Bonded,
        ] {
            harness.radio().push_event(RadioEvent::SecurityChanged {
                peer_id: "watch-race".to_owned(),
                state: ubm_desktop::SecurityState {
                    bond,
                    pairing_possible: Some(true),
                },
            });
            tokio::time::timeout(WAIT, observed.recv())
                .await
                .expect("core event observed")
                .expect("security event");
        }
        harness.radio().unblock_op(FaultOp::SecurityState);
        let watch = subscribe
            .await
            .expect("watch task")
            .expect("watch published");
        let handle = text(&watch, "handle");
        for bond in [
            ubm_desktop::BondState::Bonded,
            ubm_desktop::BondState::NotBonded,
        ] {
            harness.radio().push_event(RadioEvent::SecurityChanged {
                peer_id: "watch-race".to_owned(),
                state: ubm_desktop::SecurityState {
                    bond,
                    pairing_possible: Some(true),
                },
            });
            tokio::time::timeout(WAIT, observed.recv())
                .await
                .expect("live event observed")
                .expect("live event");
        }
        tokio::time::timeout(WAIT, async {
            while harness
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event["streamId"] == handle)
                .count()
                < 4
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("events replayed");
        let events = harness
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event["streamId"] == handle)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .map(|event| event["item"]["value"]["state"]["bond"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["not-bonded", "bonded", "bonded", "not-bonded"],
            "an unsequenced snapshot cannot be placed ahead of or after a racing native event"
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event["item"]["value"]["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4],
            "retained racing observations use the watch sequence, not a snapshot/source-counter mixture"
        );
        assert!(events.windows(2).all(|pair| {
            pair[0]["item"]["value"]["state"]["measuredAtMonotonicMs"].as_u64()
                <= pair[1]["item"]["value"]["state"]["measuredAtMonotonicMs"].as_u64()
        }));
        harness.dispatcher.release(&harness.key()).await;
        harness.central.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_address_connect_resolves_before_acquiring_the_core_lease() {
    let harness = Harness::new().await;
    harness.radio().set_address(
        "AA:BB:CC:DD:EE:FF",
        ubm_desktop::AddressType::Random,
        "address-peer",
    );
    let reply = harness
        .execute(
            "connection.connect",
            vec![(
                "address",
                object([
                    ("address", string("AA:BB:CC:DD:EE:FF")),
                    ("addressType", string("random")),
                ]),
            )],
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("typed address connects without app scan");
    assert!(!text(&reply, "handle").is_empty());
    assert_eq!(count(&harness.radio().calls(), "resolve_address"), 1);
    assert_eq!(count(&harness.radio().calls(), "connect"), 1);
    harness.dispatcher.release(&harness.key()).await;
    harness.central.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn address_resolution_uses_original_deadline_and_parent_cancellation() {
    let harness = Arc::new(Harness::new().await);
    harness.radio().set_address(
        "AA:BB:CC:DD:EE:FF",
        ubm_desktop::AddressType::Random,
        "address-peer",
    );
    harness.radio().block_op(FaultOp::ResolveAddress);
    let target = || {
        object([
            ("address", string("AA:BB:CC:DD:EE:FF")),
            ("addressType", string("random")),
        ])
    };
    let error = harness
        .execute(
            "connection.connect",
            vec![("address", target())],
            None,
            OpControl::budget_ms(25),
        )
        .await
        .expect_err("resolution shares original deadline");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(count(&harness.radio().calls(), "connect"), 0);
    let ctl = OpControl::budget_ms(5000);
    let cancellation = ctl.ticket.clone();
    let pairing_harness = Arc::clone(&harness);
    let acquisition = tokio::spawn(async move {
        pairing_harness
            .execute(
                "connection.connect",
                vec![(
                    "address",
                    object([
                        ("address", string("AA:BB:CC:DD:EE:FF")),
                        ("addressType", string("random")),
                    ]),
                )],
                None,
                ctl,
            )
            .await
    });
    tokio::time::timeout(WAIT, async {
        while count(&harness.radio().calls(), "resolve_address") < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second resolve held");
    cancellation.request_cancel();
    let error = acquisition
        .await
        .expect("acquisition task")
        .expect_err("parent cancellation reaches resolution");
    assert_eq!(error.code, BleErrorCode::OperationAborted);
    assert_eq!(count(&harness.radio().calls(), "connect"), 0);
    harness.central.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_cancel_reports_the_native_ceremony_and_invalid_options_do_not_pair() {
    let harness = Arc::new(Harness::new().await);
    harness.advertise("pair-peer").await;
    harness.radio().set_security(
        "pair-peer",
        ubm_desktop::SecurityState {
            bond: ubm_desktop::BondState::NotBonded,
            pairing_possible: Some(true),
        },
    );
    let error = harness
        .execute(
            "security.pair",
            vec![
                ("peerId", string("pair-peer")),
                ("protection", string("authenticated")),
            ],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("unsupported protection never downgraded");
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    assert_eq!(count(&harness.radio().calls(), "pair"), 0);
    harness.radio().block_op(FaultOp::Pair);
    let pairing_harness = Arc::clone(&harness);
    let pair = tokio::spawn(async move {
        pairing_harness
            .execute(
                "security.pair",
                vec![("peerId", string("pair-peer"))],
                None,
                OpControl::budget_ms(5000),
            )
            .await
    });
    tokio::time::timeout(WAIT, async {
        while count(&harness.radio().calls(), "pair") == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native pairing in flight");
    let cancelled = harness
        .execute(
            "security.cancel-pairing",
            vec![("peerId", string("pair-peer"))],
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("cancel native pairing");
    assert_eq!(text(field(&cancelled, "result"), "outcome"), "cancelled");
    assert_eq!(
        text(
            field(
                &pair.await.expect("pair task").expect("pair native answer"),
                "result"
            ),
            "outcome"
        ),
        "cancelled"
    );
    harness.central.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_routes_reach_core_and_watch_is_lease_owned() {
    let harness = Harness::new().await;
    harness.advertise("security-peer").await;
    harness.radio().set_security(
        "security-peer",
        ubm_desktop::SecurityState {
            bond: ubm_desktop::BondState::NotBonded,
            pairing_possible: Some(true),
        },
    );
    let watch = harness
        .execute(
            "security.watch.subscribe",
            vec![("peerId", string("security-peer"))],
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("security watch");
    let handle = text(&watch, "handle");
    tokio::time::timeout(WAIT, async {
        while !harness
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["streamId"] == handle)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial snapshot delivered before native changes");
    assert_eq!(
        harness
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|event| event["streamId"] == handle)
            .unwrap()["item"]["value"]["sequence"],
        1,
        "the public security event guard requires a positive initial observation sequence"
    );
    let state = harness
        .execute(
            "security.state",
            vec![("peerId", string("security-peer"))],
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("security state");
    assert_eq!(field(field(&state, "state"), "bond"), &string("not-bonded"));
    let paired = harness
        .execute(
            "security.pair",
            vec![
                ("peerId", string("security-peer")),
                ("ceremony", string("system")),
            ],
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("pair through core");
    assert_eq!(
        field(field(&paired, "result"), "outcome"),
        &string("paired")
    );
    tokio::time::timeout(WAIT, async {
        loop {
            if harness.events.lock().unwrap().iter().any(|event| {
                event["streamId"] == handle && event["item"]["value"]["state"]["bond"] == "bonded"
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native security change forwarded");
    let sequences = harness
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event["streamId"] == handle)
        .map(|event| event["item"]["value"]["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        sequences,
        [1, 2],
        "snapshot and native event share one watch-local observation order"
    );
    let removed = harness
        .execute(
            "security.unpair",
            vec![("peerId", string("security-peer"))],
            None,
            OpControl::budget_ms(5000),
        )
        .await
        .expect("unpair");
    assert_eq!(
        field(field(&removed, "result"), "outcome"),
        &string("unpaired")
    );
    let released = harness
        .route(
            "security.watch.unsubscribe",
            "release-security-watch-1",
            vec![("handle", string(handle.clone()))],
            None,
        )
        .await
        .expect("watch cleanup");
    assert_eq!(released, super::released());
    let repeated = harness
        .route(
            "security.watch.unsubscribe",
            "release-security-watch-2",
            vec![("handle", string(handle.clone()))],
            None,
        )
        .await
        .expect("owned released handle is idempotent");
    assert_eq!(repeated, super::released());
    let other = harness.other_caller().await;
    let foreign = other
        .execute(
            "security.watch.unsubscribe",
            vec![("handle", string(handle))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("foreign handle cannot fabricate release");
    assert_eq!(foreign.code, BleErrorCode::OwnershipDenied);
    assert_eq!(count(&harness.radio().calls(), "pair"), 1);
    harness.central.shutdown().await;
}

#[test]
fn scan_projection_preserves_exact_native_origin_and_service_union() {
    for source in [
        ubm_desktop::ObservationSource::Advertisement,
        ubm_desktop::ObservationSource::DeviceState,
    ] {
        let RadioEvent::Advertisement(mut snapshot) = advertisement("origin-peer") else {
            unreachable!()
        };
        snapshot.extras.source = source;
        snapshot.address = Some("AA:BB:CC:DD:EE:FF".to_owned());
        snapshot.extras.connectable = Some(true);
        snapshot
            .service_uuids
            .push("0000110b-0000-1000-8000-00805f9b34fb".into());
        let value = super::core_scan_observation(&snapshot, None);
        assert_eq!(field(&value, "address"), &string("AA:BB:CC:DD:EE:FF"));
        assert_eq!(field(&value, "connectable"), &IpcValue::Bool(true));
        assert_eq!(field(&value, "origin"), &string(source.as_str()));
        assert_eq!(
            field(&value, "serviceUuids"),
            &IpcValue::Array(
                snapshot
                    .service_uuids
                    .iter()
                    .map(|uuid| string(uuid.clone()))
                    .collect()
            )
        );
    }
}

#[tokio::test]
async fn scan_projection_preserves_authoritative_address_types_and_unknown() {
    let harness = Harness::new().await;
    let mut cache = std::collections::HashMap::new();
    for (peer, kind) in [
        ("public-peer", Some(ubm_desktop::AddressType::Public)),
        ("random-peer", Some(ubm_desktop::AddressType::Random)),
        ("unknown-peer", None),
    ] {
        harness.advertise(peer).await;
        if let Some(kind) = kind {
            harness.radio().set_address_type(peer, kind);
        }
        let RadioEvent::Advertisement(mut snapshot) = advertisement(peer) else {
            unreachable!()
        };
        snapshot.address = Some("AA:BB:CC:DD:EE:FF".into());
        let value = super::typed_core_scan_observation(&harness.central, &snapshot, &mut cache)
            .await
            .unwrap();
        assert_eq!(
            field(&value, "addressType"),
            &kind.map_or(IpcValue::Null, |kind| string(kind.as_str()))
        );
        super::typed_core_scan_observation(&harness.central, &snapshot, &mut cache)
            .await
            .unwrap();
    }
    assert_eq!(count(&harness.radio().calls(), "address_type"), 3);
    let RadioEvent::Advertisement(mut missing) = advertisement("missing-peer") else {
        unreachable!()
    };
    missing.address = Some("AA:BB:CC:DD:EE:FF".into());
    let error = super::typed_core_scan_observation(&harness.central, &missing, &mut cache)
        .await
        .expect_err("native failures are not unknown address types");
    assert_eq!(error.code, BleErrorCode::PeerNotFound);
    assert!(!cache.contains_key("missing-peer"));
    missing.address = None;
    let value = super::typed_core_scan_observation(&harness.central, &missing, &mut cache)
        .await
        .unwrap();
    assert_eq!(field(&value, "addressType"), &IpcValue::Null);
    harness.central.shutdown().await;
}

#[tokio::test]
async fn scan_projection_prefers_captured_identity_over_a_mutated_native_cache() {
    let harness = Harness::new().await;
    let RadioEvent::Advertisement(mut snapshot) = advertisement("captured-peer") else {
        unreachable!()
    };
    snapshot.address = Some("AA:BB:CC:DD:EE:FF".into());
    let mut cache = HashMap::new();
    cache.insert(snapshot.id.clone(), Some(ubm_desktop::AddressType::Public));
    for kind in [
        ubm_desktop::AddressType::Random,
        ubm_desktop::AddressType::Public,
    ] {
        snapshot.extras.address_type = Some(kind);
        let value = super::typed_core_scan_observation(&harness.central, &snapshot, &mut cache)
            .await
            .unwrap();
        assert_eq!(
            field(&value, "addressType"),
            &IpcValue::String(kind.as_str().into())
        );
    }
    assert_eq!(count(&harness.radio().calls(), "address_type"), 0);
    harness.central.shutdown().await;
}

#[tokio::test]
async fn parameter_gap_reconciles_the_live_native_answer_and_getter_failure_ends_the_watch() {
    let harness = Harness::new().await;
    let link = harness.connect("parameter-gap-peer").await;
    harness.radio().set_connection_parameters(
        "parameter-gap-peer",
        ubm_desktop::boundary::ObservedConnectionParameters {
            interval_us: 30_000,
            latency: 1,
            supervision_timeout_us: 4_000_000,
        },
    );
    let mut entries = Harness::link_entries(&link);
    entries.push(("parameterEventsHandle", string("parameter-gap-stream")));
    harness
        .execute(
            "connection.parameters.subscribe",
            entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.wait_items("parameter-gap-stream", 1).await;
    harness.radio().set_connection_parameters(
        "parameter-gap-peer",
        ubm_desktop::boundary::ObservedConnectionParameters {
            interval_us: 90_000,
            latency: 4,
            supervision_timeout_us: 6_000_000,
        },
    );
    harness
        .radio()
        .push_event(RadioEvent::ConnectionParameterGap {
            peer_id: "parameter-gap-peer".into(),
            missed: 3,
        });
    let values = harness.wait_items("parameter-gap-stream", 2).await;
    assert_eq!(values[1]["value"]["intervalUs"], 90_000);
    assert_eq!(count(&harness.radio().calls(), "connection_parameters"), 2);
    harness
        .radio()
        .fail_next(FaultOp::ConnectionParameters, "gap re-read refused");
    harness
        .radio()
        .push_event(RadioEvent::ConnectionParameterGap {
            peer_id: "parameter-gap-peer".into(),
            missed: 1,
        });
    let values = harness.wait_items("parameter-gap-stream", 3).await;
    assert_eq!(values[2]["kind"], "terminal");
    assert_eq!(values[2]["reason"], "source-failed");
    harness
        .execute(
            "connection.parameters.unsubscribe",
            vec![("parameterEventsHandle", string("parameter-gap-stream"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.central.shutdown().await;
}

#[tokio::test]
async fn delayed_initial_parameter_probe_cannot_overtake_a_newer_native_event() {
    let harness = Harness::new().await;
    let link = harness.connect("parameter-opening-peer").await;
    harness.radio().set_connection_parameters(
        "parameter-opening-peer",
        ubm_desktop::ObservedConnectionParameters {
            interval_us: 30_000,
            latency: 1,
            supervision_timeout_us: 4_000_000,
        },
    );
    harness.radio().block_op(FaultOp::ConnectionParameters);
    let mut observed = harness.central.connection_parameter_events();
    let mut entries = Harness::link_entries(&link);
    entries.push(("parameterEventsHandle", string("parameter-opening-stream")));
    let opening = harness.execute(
        "connection.parameters.subscribe",
        entries,
        None,
        OpControl::unbounded(),
    );
    tokio::pin!(opening);
    tokio::select! {
        answer = &mut opening => panic!("probe must be held: {answer:?}"),
        _ = harness.wait_calls("connection_parameters", 1) => {}
    }
    harness
        .radio()
        .push_event(RadioEvent::ConnectionParameters {
            peer_id: "parameter-opening-peer".into(),
            interval_us: 60_000,
            latency: 2,
            supervision_timeout_us: 5_000_000,
        });
    tokio::time::timeout(WAIT, observed.recv())
        .await
        .unwrap()
        .unwrap();
    harness
        .radio()
        .push_event(RadioEvent::ConnectionParameters {
            peer_id: "parameter-opening-peer".into(),
            interval_us: 90_000,
            latency: 3,
            supervision_timeout_us: 6_000_000,
        });
    tokio::time::timeout(WAIT, observed.recv())
        .await
        .unwrap()
        .unwrap();
    harness.radio().unblock_op(FaultOp::ConnectionParameters);
    opening.await.unwrap();
    let items = harness.wait_items("parameter-opening-stream", 2).await;
    assert_eq!(items[0]["value"]["intervalUs"], 60_000);
    assert_eq!(items[1]["value"]["intervalUs"], 90_000);
    assert_eq!(
        items.len(),
        2,
        "buffered native observations keep order and the stale initial probe stays absent"
    );
    harness
        .execute(
            "connection.parameters.unsubscribe",
            vec![("parameterEventsHandle", string("parameter-opening-stream"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.central.shutdown().await;
}

#[tokio::test]
async fn acquired_routes_use_the_native_lease_and_retire_both_children_on_disconnect() {
    fn linux(core: &mut ubm_core::central::Central) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::Linux),
            false,
        )
    }
    let mut profile = CentralProfile::desktop("tauri-acquired-test");
    profile.directory_os = ubm_desktop::DesktopOs::Linux;
    profile.register_capabilities = linux;
    let central = DesktopCentral::open_with(FakeRadio::new(), profile)
        .await
        .unwrap();
    let harness = Harness::over(central).await;
    let link = harness.connect("acquired-peer").await;
    harness
        .radio()
        .acquired_gatt()
        .configure(
            "acquired-peer",
            ubm_desktop::acquired_gatt::synthetic::SyntheticAcquisition {
                write: true,
                notify: true,
                mtu: 23,
            },
        )
        .unwrap();
    // AcquireWrite requires the native write-without-response property.
    let mut service = hrm_service();
    service
        .characteristics
        .iter_mut()
        .find(|value| value.uuid == CONTROL_POINT)
        .unwrap()
        .properties
        .write_without_response = true;
    harness.radio().set_services("acquired-peer", vec![service]);
    let database = harness
        .execute(
            "gatt.discover",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let rows = match field(&database, "characteristics") {
        IpcValue::Array(rows) => rows,
        _ => panic!("characteristics"),
    };
    let characteristic_handle = |uuid: &str| {
        rows.iter()
            .find(|row| text(row, "characteristicUuid") == uuid)
            .map(|row| text(row, "handle"))
            .unwrap()
    };
    let target = |uuid: &str| {
        let mut entries = Harness::link_entries(&link);
        entries.extend([
            ("databaseHandle", string(text(&database, "handle"))),
            ("databaseId", field(&database, "databaseId").clone()),
            (
                "databaseGeneration",
                field(&database, "databaseGeneration").clone(),
            ),
            ("characteristicHandle", string(characteristic_handle(uuid))),
        ]);
        entries
    };
    let writer = harness
        .execute(
            "gatt.acquire-write",
            target(CONTROL_POINT),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(field(&writer, "mtuBytes"), &number(23));
    let writer_handle = text(&writer, "handle");
    let mut write = target(CONTROL_POINT);
    write.push(("acquiredHandle", string(&writer_handle)));
    let receipt = harness
        .execute(
            "gatt.acquired-write",
            write,
            Some(vec![42]),
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&receipt, "commitState"), "unknown");
    assert_eq!(harness.radio().acquired_gatt().writes(), vec![vec![42]]);
    let notifications = harness
        .execute(
            "gatt.acquire-notifications",
            target(NOTIFY_ONLY),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let stream_handle = text(&notifications, "handle");
    harness.radio().acquired_gatt().notify(vec![7, 8]);
    let values = harness.wait_items(&stream_handle, 1).await;
    assert_eq!(value_bytes(&values[0]), vec![7, 8]);
    let other = harness.other_caller().await;
    let foreign = other
        .execute(
            "gatt.acquired-write.close",
            vec![("acquiredHandle", string(&writer_handle))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap_err();
    assert_eq!(foreign.code, BleErrorCode::OwnershipDenied);
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        harness
            .central
            .resource_counters()
            .await
            .acquired_gatt_transports,
        0
    );
    harness
        .execute(
            "gatt.acquired-write.close",
            vec![("acquiredHandle", string(&writer_handle))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness
        .execute(
            "gatt.unsubscribe",
            vec![("subscriptionHandle", string(&stream_handle))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.central.shutdown().await;
}

#[tokio::test]
async fn parameter_source_failure_survives_for_a_later_watch_until_native_recovery() {
    let harness = Harness::new().await;
    let link = harness.connect("parameter-health-peer").await;
    harness.radio().set_connection_parameters(
        "parameter-health-peer",
        ubm_desktop::ObservedConnectionParameters {
            interval_us: 30_000,
            latency: 1,
            supervision_timeout_us: 4_000_000,
        },
    );
    let mut observed = harness.central.connection_parameter_events();
    harness
        .radio()
        .push_event(RadioEvent::ConnectionParameterSourceFailed {
            peer_id: "parameter-health-peer".into(),
            error: DesktopError::new(
                BleErrorCode::StreamClosed,
                ubm_core::contracts::BleErrorDomain::Stream,
                "native.parameter-source",
            ),
        });
    tokio::time::timeout(WAIT, observed.recv())
        .await
        .unwrap()
        .unwrap();
    let mut entries = Harness::link_entries(&link);
    entries.push(("parameterEventsHandle", string("parameter-health-stream")));
    let denied = harness
        .execute(
            "connection.parameters.subscribe",
            entries.clone(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, BleErrorCode::StreamClosed);
    assert_eq!(denied.operation, "native.parameter-source");
    assert_eq!(count(&harness.radio().calls(), "connection_parameters"), 0);
    harness.radio().set_connection_parameters(
        "parameter-health-peer",
        ubm_desktop::ObservedConnectionParameters {
            interval_us: 60_000,
            latency: 2,
            supervision_timeout_us: 5_000_000,
        },
    );
    harness
        .radio()
        .push_event(RadioEvent::ConnectionParameters {
            peer_id: "parameter-health-peer".into(),
            interval_us: 60_000,
            latency: 2,
            supervision_timeout_us: 5_000_000,
        });
    tokio::time::timeout(WAIT, observed.recv())
        .await
        .unwrap()
        .unwrap();
    harness
        .execute(
            "connection.parameters.subscribe",
            entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let items = harness.wait_items("parameter-health-stream", 1).await;
    assert_eq!(items[0]["value"]["intervalUs"], 60_000);
    harness
        .execute(
            "connection.parameters.unsubscribe",
            vec![("parameterEventsHandle", string("parameter-health-stream"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.central.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_stream_serializes_native_public_random_and_unknown_address_types() {
    let harness = Harness::new().await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let stream = text(&scan, "handle");
    for (index, (peer, kind)) in [
        ("public-scan", Some(ubm_desktop::AddressType::Public)),
        ("random-scan", Some(ubm_desktop::AddressType::Random)),
        ("unknown-scan", None),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(kind) = kind {
            harness.radio().set_address_type(peer, kind);
        }
        let RadioEvent::Advertisement(mut snapshot) = advertisement(peer) else {
            unreachable!()
        };
        snapshot.address = Some("AA:BB:CC:DD:EE:FF".into());
        harness
            .radio()
            .push_event(RadioEvent::Advertisement(snapshot));
        let items = harness.wait_items(&stream, index + 1).await;
        assert_eq!(items[index]["value"]["address"], "AA:BB:CC:DD:EE:FF");
        assert_eq!(
            items[index]["value"]["addressType"],
            kind.map_or(serde_json::Value::Null, |kind| serde_json::Value::String(
                kind.as_str().into()
            ))
        );
    }
    harness
        .execute(
            "scan.stop",
            vec![("scanHandle", string(stream))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.central.shutdown().await;
}

#[tokio::test]
async fn idle_process_handoff_never_opens_bluetooth_even_after_shutdown() {
    let opens = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&opens);
    let dispatcher = BtleplugDispatcher::with_authority_opener(Arc::new(move || {
        counted.fetch_add(1, AtomicOrdering::SeqCst);
        Box::pin(async {
            Err(DispatchError::new(
                ubm_core::contracts::BleErrorCode::AdapterUnavailable,
                "adapter",
                "test.unexpected-open",
            ))
        })
    }));
    let harness = Harness::new().await;
    let canonical = ubm_desktop::continuation::NativeContinuation::new(Arc::new(
        ubm_desktop::continuation_adapter::DesktopContinuationHost::new(harness.central.clone()),
    ));
    let empty = canonical.prepare_claim(256, 65536).await.unwrap();
    let no_token = canonical.acknowledge_claim("unknown").await.unwrap_err();
    let bad_bounds = canonical.prepare_claim(0, 65536).await.unwrap_err();
    for closed in [false, true] {
        if closed {
            assert!(dispatcher.authority_shutdown().await.core.is_none());
        }
        assert_eq!(
            dispatcher.continuation_describe_backlog().await.unwrap(),
            Value::Null
        );
        assert_eq!(
            dispatcher
                .continuation_prepare_claim(256, 65536)
                .await
                .unwrap(),
            empty
        );
        assert_eq!(
            dispatcher
                .continuation_prepare_claim(0, 65536)
                .await
                .unwrap_err(),
            bad_bounds
        );
        assert_eq!(
            dispatcher
                .continuation_acknowledge_claim("unknown")
                .await
                .unwrap_err(),
            no_token
        );
        assert_eq!(opens.load(AtomicOrdering::SeqCst), 0);
    }
    assert!(harness
        .dispatcher
        .authority_shutdown()
        .await
        .core
        .unwrap()
        .is_released());
}

#[tokio::test]
async fn process_shutdown_retains_native_claim_and_retry_owner() {
    let harness = Harness::new().await;
    let peer = "native-parent-close";
    harness.advertise(peer).await;
    let declaration = serde_json::json!({"onAppearance":"native","peerId":peer,
        "resubscribe":[{"serviceUuid":HRM_SERVICE,"characteristicUuid":NOTIFY_ONLY}]})
    .to_string();
    harness
        .dispatcher
        .continuation_execute(peer, &declaration)
        .await
        .unwrap();
    harness.radio().push_event(RadioEvent::Notification {
        peer_id: peer.into(),
        service_uuid: HRM_SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: NOTIFY_ONLY.into(),
        characteristic_occurrence: 0,
        value: vec![0, 75],
        epoch: harness.central.routing_epoch(peer).await,
    });
    // A central wake only proves native ingress, not collection into the
    // continuation outbox. Prove the retained value before racing shutdown;
    // the post-shutdown claim below must still contain that exact value.
    tokio::time::timeout(WAIT, async {
        loop {
            let backlog = harness
                .dispatcher
                .continuation_describe_backlog()
                .await
                .unwrap();
            assert!(backlog["lastError"].is_null(), "{backlog}");
            if backlog["queuedData"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("notification must enter the continuation outbox before shutdown");
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "retained cleanup");
    assert!(!harness
        .dispatcher
        .authority_shutdown()
        .await
        .core
        .unwrap()
        .is_released());
    assert!(harness
        .dispatcher
        .continuation_describe_backlog()
        .await
        .unwrap()
        .is_object());
    assert!(harness
        .dispatcher
        .authority_shutdown()
        .await
        .core
        .unwrap()
        .is_released());
    let claim = harness
        .dispatcher
        .continuation_prepare_claim(256, 65536)
        .await
        .unwrap();
    let batches = claim["batches"].as_array().unwrap();
    assert!(
        batches.iter().any(|batch| {
            let drain: Value = serde_json::from_str(batch.as_str().unwrap()).unwrap();
            drain["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["valueB64"] == "AEs=")
        }),
        "{claim}"
    );
    let token = claim["claimToken"].as_str().unwrap();
    assert_eq!(
        harness
            .dispatcher
            .continuation_prepare_claim(256, 65536)
            .await
            .unwrap(),
        claim
    );
    assert_eq!(
        harness
            .dispatcher
            .continuation_acknowledge_claim(token)
            .await
            .unwrap()["disposed"],
        true
    );
    assert!(harness
        .dispatcher
        .continuation_execute(peer, &declaration)
        .await
        .is_err());
}

fn directory_mac_profile(
    core: &mut ubm_core::central::Central,
) -> Result<(), ubm_core::contracts::CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(ubm_desktop::DesktopOs::MacOs), false)
}

async fn directory_harness() -> Harness {
    let mut profile = ubm_desktop::CentralProfile::desktop("tauri-directory-test");
    profile.register_capabilities = directory_mac_profile;
    profile.directory_os = ubm_desktop::DesktopOs::MacOs;
    Harness::over(
        DesktopCentral::open_with(os_radio(AdmissionPolicy::LifecycleOnly), profile)
            .await
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn deferred_connect_dispatch_uses_shared_native_lease_authority() {
    let harness = directory_harness().await;
    let result = harness
        .route(
            "connection.connect",
            "deferred-dispatch",
            vec![
                ("peerId", string("00112233-4455-6677-8899-aabbccddeeff")),
                ("intent", string("when-available")),
            ],
            None,
        )
        .await
        .expect("native deferred route")
        .into_wire();
    assert!(result["handle"].is_string());
    assert_eq!(count(&harness.radio().calls(), "connect_when_available"), 1);
}

#[tokio::test]
async fn peer_directory_bonded_preserves_native_fact_without_link_ownership() {
    fn bonded_profile(
        core: &mut ubm_core::central::Central,
    ) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::Windows),
            false,
        )
    }
    let mut profile = ubm_desktop::CentralProfile::desktop("tauri-bonded-test");
    profile.register_capabilities = bonded_profile;
    profile.directory_os = ubm_desktop::DesktopOs::Windows;
    let harness = Harness::over(
        DesktopCentral::open_with(os_radio(AdmissionPolicy::LifecycleOnly), profile)
            .await
            .unwrap(),
    )
    .await;
    let id = "public:AA:BB:CC:DD:EE:FF";
    harness
        .radio()
        .set_bonded_directory_peers(vec![ubm_desktop::DirectoryPeer {
            peer_id: id.to_owned(),
            name: Some("bonded native peer".into()),
            connection: "disconnected",
        }]);
    harness
        .radio()
        .set_known_directory_peers(vec![ubm_desktop::DirectoryPeer {
            peer_id: id.into(),
            name: Some("unbonded cached observation".into()),
            connection: "unknown",
        }]);
    let backend = "unified-ble:winrt";
    let foreign_backend = if backend == "unified-ble:corebluetooth" {
        "unified-ble:winrt"
    } else {
        "unified-ble:corebluetooth"
    };
    let reference = |scope_backend| {
        object([
            ("version", IpcValue::Number(1.into())),
            ("backendId", string(scope_backend)),
            ("scope", string("application")),
            ("opaqueId", string(id)),
        ])
    };
    let before = count(&harness.radio().calls(), "bonded_peers");
    let foreign = harness
        .route(
            "peers.bonded",
            "bonded-foreign-reference",
            vec![(
                "query",
                object([(
                    "references",
                    IpcValue::Array(vec![reference(foreign_backend)]),
                )]),
            )],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(foreign.code, BleErrorCode::PeerScopeMismatch);
    assert_eq!(count(&harness.radio().calls(), "bonded_peers"), before);
    let result = harness
        .route(
            "peers.bonded",
            "bonded-positive",
            vec![(
                "query",
                object([("references", IpcValue::Array(vec![reference(backend)]))]),
            )],
            None,
        )
        .await
        .expect("native bonded route")
        .into_wire();
    assert_eq!(result["peers"][0]["peerId"], id);
    assert_eq!(result["peers"].as_array().unwrap().len(), 1);
    assert_eq!(count(&harness.radio().calls(), "bonded_peers"), before + 1);
    assert_eq!(result["peers"][0]["state"]["bond"], "bonded");
    assert_eq!(result["peers"][0]["state"]["connection"], "disconnected");
    let resolved = harness
        .route(
            "peers.resolve",
            "bonded-reference-resolve",
            vec![("reference", reference(backend))],
            None,
        )
        .await
        .expect("bonded reference roundtrip")
        .into_wire();
    assert_eq!(resolved["peer"]["peerId"], id);
    assert_eq!(resolved["peer"]["source"], "app-reference");
    assert_eq!(resolved["peer"]["state"]["bond"], "unknown");
    assert_eq!(count(&harness.radio().calls(), "bonded_peers"), before + 1);
    assert_eq!(count(&harness.radio().calls(), "resolve_peer"), 1);
    assert!(
        harness.dispatcher.inner.lock().await.callers[&harness.key()]
            .connections
            .is_empty()
    );
    assert!(!harness
        .radio()
        .calls()
        .iter()
        .any(|call| call.starts_with("connect:")));
    let connected = harness
        .execute(
            "connection.connect",
            vec![("peerId", string(id))],
            None,
            OpControl::default(),
        )
        .await
        .expect("explicit resolved identity connection");
    assert_eq!(text(&connected, "peerId"), id);
    harness.radio().set_bonded_directory_peers(Vec::new());
    harness.radio().set_known_directory_peers(Vec::new());
    let removed = harness
        .route(
            "peers.resolve",
            "removed-bond-reference",
            vec![("reference", reference(backend))],
            None,
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(removed["peer"], Value::Null);
}

#[tokio::test]
async fn peer_directory_native_inventory_keeps_backend_scope_and_foreign_link_unowned() {
    type DirectoryRegistration =
        fn(&mut ubm_core::central::Central) -> Result<(), ubm_core::contracts::CoreError>;
    type DirectoryCase = (
        ubm_desktop::DesktopOs,
        &'static str,
        &'static str,
        DirectoryRegistration,
    );
    let cases: [DirectoryCase; 2] = [
        (
            ubm_desktop::DesktopOs::Windows,
            "unified-ble:winrt",
            "random:AA:BB:CC:DD:EE:FF",
            windows_directory_profile,
        ),
        (
            ubm_desktop::DesktopOs::Linux,
            "unified-ble:bluez-dbus",
            "hci0/dev_AA_BB_CC_DD_EE_FF",
            linux_directory_profile,
        ),
    ];
    for (os, backend, id, registration) in cases {
        let mut profile = ubm_desktop::CentralProfile::desktop("native-directory");
        profile.register_capabilities = registration;
        profile.directory_os = os;
        let harness = Harness::over(
            DesktopCentral::open_with(os_radio(AdmissionPolicy::LifecycleOnly), profile)
                .await
                .unwrap(),
        )
        .await;
        let peer = ubm_desktop::DirectoryPeer {
            peer_id: id.into(),
            name: Some("foreign unbonded peer".into()),
            connection: "connected",
        };
        harness
            .radio()
            .set_known_directory_peers(vec![peer.clone()]);
        harness.radio().set_directory_peers(vec![peer]);
        for command in ["peers.known", "peers.connected"] {
            let result = harness
                .route(command, command, vec![("query", object([]))], None)
                .await
                .unwrap()
                .into_wire();
            assert_eq!(result["peers"][0]["reference"]["backendId"], backend);
            assert_eq!(result["peers"][0]["peerId"], id);
            assert_eq!(result["peers"][0]["state"]["bond"], "unknown");
        }
        let reference = object([
            ("version", IpcValue::Number(1.into())),
            ("backendId", string(backend)),
            ("scope", string("application")),
            ("opaqueId", string(id)),
        ]);
        let resolved = harness
            .route(
                "peers.resolve",
                "resolve-native",
                vec![("reference", reference)],
                None,
            )
            .await
            .unwrap()
            .into_wire();
        assert_eq!(resolved["peer"]["peerId"], id);
        assert_eq!(count(&harness.radio().calls(), "bonded_peers"), 0);
        assert!(
            harness.dispatcher.inner.lock().await.callers[&harness.key()]
                .connections
                .is_empty()
        );
        assert!(!harness
            .radio()
            .calls()
            .iter()
            .any(|call| call.starts_with("connect:") || call.starts_with("start_scan:")));
        harness.central.shutdown().await;
    }
}

fn windows_directory_profile(
    core: &mut ubm_core::central::Central,
) -> Result<(), ubm_core::contracts::CoreError> {
    ubm_desktop::register_desktop_capabilities_for(
        core,
        Some(ubm_desktop::DesktopOs::Windows),
        false,
    )
}
fn linux_directory_profile(
    core: &mut ubm_core::central::Central,
) -> Result<(), ubm_core::contracts::CoreError> {
    ubm_desktop::register_desktop_capabilities_for(core, Some(ubm_desktop::DesktopOs::Linux), false)
}

#[tokio::test]
async fn peer_directory_connected_returns_identity_without_connection_ownership() {
    let harness = directory_harness().await;
    let id = "00112233-4455-6677-8899-aabbccddeeff";
    harness
        .radio()
        .set_directory_peers(vec![ubm_desktop::DirectoryPeer {
            peer_id: id.to_owned(),
            name: Some("directory peer".to_owned()),
            connection: "connected",
        }]);
    let result = harness
        .route(
            "peers.connected",
            "directory-positive",
            vec![(
                "query",
                object([("services", IpcValue::Array(vec![string(HRM_SERVICE)]))]),
            )],
            None,
        )
        .await
        .expect("directory supported");
    let json = result.into_wire();
    assert_eq!(json["peers"][0]["peerId"], id);
    assert_eq!(
        json["peers"][0]["reference"]["backendId"],
        "unified-ble:corebluetooth"
    );
    assert_eq!(
        json["peers"][0]["state"]["lastSeenAtMonotonicMs"],
        Value::Null
    );
    assert!(
        harness.dispatcher.inner.lock().await.callers[&harness.key()]
            .connections
            .is_empty()
    );
    assert!(!harness
        .radio()
        .calls()
        .iter()
        .any(|call| call.starts_with("connect:")));
}

fn directory_reference(id: &str) -> IpcValue {
    object([
        ("version", IpcValue::Number(1.into())),
        ("backendId", string("unified-ble:corebluetooth")),
        ("scope", string("application")),
        ("opaqueId", string(id)),
    ])
}

#[tokio::test]
async fn peer_directory_validates_entire_query_before_native_dispatch() {
    let harness = directory_harness().await;
    for (command, query, code, operation) in [
        (
            "peers.connected",
            object([]),
            BleErrorCode::CapabilityUnsupported,
            "peers.connected.services-required",
        ),
        (
            "peers.known",
            object([]),
            BleErrorCode::CapabilityUnsupported,
            "peers.known.references-required",
        ),
        (
            "peers.bonded",
            object([]),
            BleErrorCode::CapabilityUnsupported,
            "peers.bonded",
        ),
        (
            "peers.authorized",
            object([]),
            BleErrorCode::CapabilityUnsupported,
            "peers.authorized",
        ),
        (
            "peers.restored",
            object([]),
            BleErrorCode::CapabilityUnsupported,
            "peers.restored",
        ),
        (
            "peers.known",
            object([(
                "references",
                IpcValue::Array(vec![
                    directory_reference("00112233-4455-6677-8899-aabbccddeeff"),
                    directory_reference("bad-id"),
                ]),
            )]),
            BleErrorCode::PeerReferenceInvalid,
            "peers.reference",
        ),
        (
            "peers.connected",
            object([("services", IpcValue::Array(vec![string("180d")]))]),
            BleErrorCode::ProtocolMalformed,
            "peers.connected",
        ),
        (
            "peers.connected",
            object([("includeUnavailable", IpcValue::Null)]),
            BleErrorCode::ProtocolMalformed,
            "peers.connected",
        ),
    ] {
        let error = harness
            .execute(command, vec![("query", query)], None, OpControl::default())
            .await
            .unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.operation, operation);
    }
    assert!(!harness
        .radio()
        .calls()
        .iter()
        .any(|call| call == "resolve_peer" || call == "connected_peers"));
}

#[tokio::test]
async fn peer_directory_known_resolve_null_and_existing_connect_path() {
    let harness = directory_harness().await;
    let id = "00112233-4455-6677-8899-aabbccddeeff";
    harness
        .radio()
        .set_directory_peers(vec![ubm_desktop::DirectoryPeer {
            peer_id: id.to_owned(),
            name: None,
            connection: "connected",
        }]);
    let result = harness
        .execute(
            "peers.known",
            vec![(
                "query",
                object([(
                    "references",
                    IpcValue::Array(vec![
                        directory_reference(&id.to_uppercase()),
                        directory_reference(id),
                    ]),
                )]),
            )],
            None,
            OpControl::default(),
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(result["peers"].as_array().unwrap().len(), 1);
    assert_eq!(result["peers"][0]["state"]["connection"], "unknown");
    let resolved = harness
        .execute(
            "peers.resolve",
            vec![("reference", directory_reference(id))],
            None,
            OpControl::default(),
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(resolved["peer"]["peerId"], id);
    assert_eq!(
        resolved["peer"]["reference"]["backendId"],
        "unified-ble:corebluetooth"
    );
    let calls = count(&harness.radio().calls(), "resolve_peer");
    let foreign = object([
        ("version", IpcValue::Number(1.into())),
        ("backendId", string("unified-ble:winrt")),
        ("scope", string("application")),
        ("opaqueId", string("AA:BB:CC:DD:EE:FF")),
    ]);
    assert_eq!(
        harness
            .execute(
                "peers.resolve",
                vec![("reference", foreign)],
                None,
                OpControl::default()
            )
            .await
            .unwrap_err()
            .code,
        BleErrorCode::PeerScopeMismatch
    );
    assert_eq!(count(&harness.radio().calls(), "resolve_peer"), calls);
    let missing = harness
        .execute(
            "peers.resolve",
            vec![(
                "reference",
                directory_reference("ffffffff-ffff-ffff-ffff-ffffffffffff"),
            )],
            None,
            OpControl::default(),
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(missing["peer"], Value::Null);
    // Explicit connect uses the returned OS identity, without manufacturing an advertisement.
    let connected = harness
        .execute(
            "connection.connect",
            vec![("peerId", string(id))],
            None,
            OpControl::default(),
        )
        .await
        .unwrap();
    assert_eq!(text(&connected, "peerId"), id);
}

#[tokio::test]
async fn peer_directory_known_second_lookup_keeps_caller_cancellation() {
    let harness = directory_harness().await;
    harness.radio().set_directory_peers(Vec::new());
    harness.radio().block_op(FaultOp::PeerDirectory);
    harness.radio().set_directory_unblocked_reads(1);
    let ctl = OpControl::default();
    let ticket = ctl.ticket.clone();
    let work = harness.execute(
        "peers.known",
        vec![(
            "query",
            object([(
                "references",
                IpcValue::Array(vec![
                    directory_reference("00112233-4455-6677-8899-aabbccddeeff"),
                    directory_reference("11112233-4455-6677-8899-aabbccddeeff"),
                    directory_reference("22112233-4455-6677-8899-aabbccddeeff"),
                ]),
            )]),
        )],
        None,
        ctl,
    );
    tokio::pin!(work);
    tokio::select! { result = &mut work => panic!("unexpected completion {result:?}"), observed = tokio::time::timeout(WAIT,harness.radio().wait_for_calls("resolve_peer",2)) => { observed.unwrap(); } }
    ticket.request_cancel();
    let error = tokio::time::timeout(WAIT, &mut work)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::OperationAborted);
    harness.radio().unblock_op(FaultOp::PeerDirectory);
    assert_eq!(count(&harness.radio().calls(), "resolve_peer"), 2);
}

#[tokio::test]
async fn peer_directory_reset_during_batch_never_publishes_partial_records() {
    let harness = directory_harness().await;
    harness
        .radio()
        .set_directory_peers(vec![ubm_desktop::DirectoryPeer {
            peer_id: "00112233-4455-6677-8899-aabbccddeeff".to_owned(),
            name: None,
            connection: "connected",
        }]);
    harness.radio().block_op(FaultOp::PeerDirectory);
    harness.radio().set_directory_unblocked_reads(1);
    let work = harness.execute(
        "peers.known",
        vec![(
            "query",
            object([(
                "references",
                IpcValue::Array(vec![
                    directory_reference("00112233-4455-6677-8899-aabbccddeeff"),
                    directory_reference("11112233-4455-6677-8899-aabbccddeeff"),
                ]),
            )]),
        )],
        None,
        OpControl::default(),
    );
    tokio::pin!(work);
    tokio::select! { result = &mut work => panic!("unexpected {result:?}"), observed = tokio::time::timeout(WAIT,harness.radio().wait_for_calls("resolve_peer",2)) => { observed.unwrap(); } }
    harness.lose_adapter().await;
    harness.radio().unblock_op(FaultOp::PeerDirectory);
    let error = tokio::time::timeout(WAIT, work).await.unwrap().unwrap_err();
    assert!(matches!(
        error.code,
        BleErrorCode::OperationReset | BleErrorCode::BackendReset
    ));
    assert_eq!(count(&harness.radio().calls(), "resolve_peer"), 2);
}

#[tokio::test]
async fn peer_directory_unavailable_hosts_never_short_circuit_to_success_or_mac_requirements() {
    fn linux(core: &mut ubm_core::central::Central) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::Linux),
            false,
        )
    }
    fn windows(
        core: &mut ubm_core::central::Central,
    ) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::Windows),
            false,
        )
    }
    for registration in [
        linux as fn(&mut ubm_core::central::Central) -> Result<(), ubm_core::contracts::CoreError>,
        windows,
    ] {
        let mut profile = ubm_desktop::CentralProfile::desktop("directory-unsupported");
        profile.register_capabilities = registration;
        let radio = FakeRadio::new();
        radio.set_directory_capability_limitations(Ok((
            Some("known-directory-unavailable"),
            Some("connected-directory-unavailable"),
        )));
        let harness = Harness::over(DesktopCentral::open_with(radio, profile).await.unwrap()).await;
        for (command, query) in [
            ("peers.connected", object([])),
            (
                "peers.connected",
                object([
                    ("services", IpcValue::Array(vec![string(HRM_SERVICE)])),
                    ("sources", IpcValue::Array(vec![])),
                ]),
            ),
            (
                "peers.known",
                object([("references", IpcValue::Array(vec![]))]),
            ),
        ] {
            let error = harness
                .execute(command, vec![("query", query)], None, OpControl::default())
                .await
                .unwrap_err();
            assert_eq!(error.code, BleErrorCode::CapabilityUnavailable);
            assert_eq!(error.operation, command);
        }
        assert!(!harness
            .radio()
            .calls()
            .iter()
            .any(|call| call == "resolve_peer" || call == "connected_peers"));
    }
}

#[tokio::test(start_paused = true)]
async fn peer_directory_deadline_bounds_held_read() {
    let harness = directory_harness().await;
    harness.radio().set_directory_peers(Vec::new());
    harness.radio().block_op(FaultOp::PeerDirectory);
    let ctl = OpControl::budget_ms(20);
    let work = harness.execute(
        "peers.connected",
        vec![(
            "query",
            object([("services", IpcValue::Array(vec![string(HRM_SERVICE)]))]),
        )],
        None,
        ctl,
    );
    tokio::pin!(work);
    tokio::select! { result = &mut work => panic!("unexpected {result:?}"), _ = harness.radio().wait_for_calls("connected_peers",1) => {} }
    tokio::time::advance(Duration::from_millis(20)).await;
    assert_eq!(
        work.await.unwrap_err().code,
        BleErrorCode::OperationTimedOut
    );
}

#[tokio::test(start_paused = true)]
async fn peer_directory_batch_keeps_original_budget_after_first_read() {
    let harness = directory_harness().await;
    harness.radio().set_directory_peers(Vec::new());
    harness.radio().block_op(FaultOp::PeerDirectory);
    harness.radio().set_directory_unblocked_reads(1);
    let ctl = OpControl::budget_ms(20);
    tokio::time::advance(Duration::from_millis(10)).await;
    let work = harness.execute(
        "peers.known",
        vec![(
            "query",
            object([(
                "references",
                IpcValue::Array(vec![
                    directory_reference("00112233-4455-6677-8899-aabbccddeeff"),
                    directory_reference("11112233-4455-6677-8899-aabbccddeeff"),
                    directory_reference("22112233-4455-6677-8899-aabbccddeeff"),
                ]),
            )]),
        )],
        None,
        ctl,
    );
    tokio::pin!(work);
    tokio::select! { result = &mut work => panic!("unexpected {result:?}"), observed = tokio::time::timeout(WAIT,harness.radio().wait_for_calls("resolve_peer",2)) => { observed.unwrap(); } }
    tokio::time::advance(Duration::from_millis(10)).await;
    assert_eq!(
        work.await.unwrap_err().code,
        BleErrorCode::OperationTimedOut
    );
    assert_eq!(count(&harness.radio().calls(), "resolve_peer"), 2);
}

#[tokio::test]
async fn peer_directory_retired_caller_cannot_receive_held_lookup() {
    let harness = directory_harness().await;
    harness.radio().set_directory_peers(Vec::new());
    harness.radio().block_op(FaultOp::PeerDirectory);
    let work = harness.execute(
        "peers.connected",
        vec![(
            "query",
            object([("services", IpcValue::Array(vec![string(HRM_SERVICE)]))]),
        )],
        None,
        OpControl::default(),
    );
    tokio::pin!(work);
    tokio::select! { result = &mut work => panic!("unexpected {result:?}"), observed = tokio::time::timeout(WAIT,harness.radio().wait_for_calls("connected_peers",1)) => { observed.unwrap(); } }
    harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .get_mut(&harness.key())
        .unwrap()
        .retired = true;
    harness.radio().unblock_op(FaultOp::PeerDirectory);
    assert_eq!(
        tokio::time::timeout(WAIT, work)
            .await
            .unwrap()
            .unwrap_err()
            .code,
        BleErrorCode::OwnershipDenied
    );
}

#[tokio::test]
async fn peer_directory_excluded_source_preserves_native_refusal() {
    let harness = directory_harness().await;
    // Capability exists, but this boundary refuses the actual lookup.
    let error = harness
        .execute(
            "peers.connected",
            vec![(
                "query",
                object([
                    ("services", IpcValue::Array(vec![string(HRM_SERVICE)])),
                    ("sources", IpcValue::Array(vec![string("app-reference")])),
                ]),
            )],
            None,
            OpControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    assert_eq!(error.operation, "peers.connected");
    assert_eq!(count(&harness.radio().calls(), "connected_peers"), 1);
}

#[tokio::test]
async fn peer_directory_foreign_reference_and_malformed_reply_fail_closed() {
    let harness = directory_harness().await;
    let mut foreign = into_object(
        directory_reference("00112233-4455-6677-8899-aabbccddeeff"),
        "test",
    )
    .unwrap();
    foreign.insert("backendId".to_owned(), string("other-backend"));
    let error = harness
        .execute(
            "peers.resolve",
            vec![("reference", IpcValue::Object(foreign))],
            None,
            OpControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::PeerScopeMismatch);
    assert_eq!(count(&harness.radio().calls(), "resolve_peer"), 0);
    harness
        .radio()
        .set_directory_peers(vec![ubm_desktop::DirectoryPeer {
            peer_id: "invalid".to_owned(),
            name: None,
            connection: "connected",
        }]);
    let error = harness
        .execute(
            "peers.connected",
            vec![(
                "query",
                object([("services", IpcValue::Array(vec![string(HRM_SERVICE)]))]),
            )],
            None,
            OpControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::ProtocolMalformed);
}

#[tokio::test(start_paused = true)]
async fn authority_shutdown_retains_half_open_compensation_cause_until_confirmed_retry() {
    let harness = Harness::on_current_runtime().await;
    harness
        .radio()
        .fail_next(FaultOp::Connect, "connect refused");
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "initial compensation refused");
    assert!(harness
        .central
        .connect("half-open", "owner", OpControl::unbounded())
        .await
        .is_err());
    let platform = PlatformDetail::new("android", "133").with_message("disconnect refused");
    harness
        .radio()
        .fail_next_with_platform(FaultOp::Disconnect, "retry refused", platform.clone());
    let first = harness
        .dispatcher
        .authority_shutdown()
        .await
        .core
        .expect("report");
    assert!(!first.is_released());
    assert_eq!(first.half_open_close_failures.len(), 1);
    let failure = &first.half_open_close_failures[0];
    assert_eq!(failure.code_str(), "connection.lost");
    assert_eq!(failure.domain().as_str(), "connection");
    assert_eq!(failure.operation(), "connection.disconnect");
    assert_eq!(failure.platform(), Some(&platform));
    let retry = harness
        .dispatcher
        .authority_shutdown()
        .await
        .core
        .expect("debt retains retry owner");
    assert!(retry.half_open_close_failures.is_empty());
    assert!(retry.is_released());
    assert!(harness.dispatcher.authority_shutdown().await.core.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_connection_release_preserves_another_native_lease() {
    let harness = Harness::new().await;
    let link = harness.connect("shared-release").await;
    let other = harness.other_caller().await;
    let other_link = other.connect("shared-release").await;
    harness
        .connection_events(&link, "released-owner-events")
        .await;
    harness.ready("released-owner-events").await;
    other
        .connection_events(&other_link, "remaining-owner-events")
        .await;
    other.ready("remaining-owner-events").await;
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        count(&harness.radio().calls(), "disconnect"),
        0,
        "releasing one caller must not physically disconnect the other owner"
    );
    let items = harness.items("released-owner-events");
    assert_eq!(
        items.len(),
        2,
        "one initial lifecycle and one owner terminal only"
    );
    assert_eq!(items[1]["kind"], "terminal");
    assert_eq!(items[1]["reason"], "owner-released");
    assert_eq!(other.items("remaining-owner-events").len(), 1);
    assert!(
        harness
            .with_caller(|caller| caller.connection_events.is_empty())
            .await
    );
    other.discover(&other_link).await;
    assert!(harness
        .execute(
            "gatt.discover",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded()
        )
        .await
        .is_err());
    harness.dispatcher.authority_shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_connection_release_retires_only_its_subscription() {
    let harness = Harness::new().await;
    let link = harness.connect("shared-subscriptions").await;
    let database = harness.discover(&link).await;
    harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .unwrap();
    let other = harness.other_caller().await;
    let other_link = other.connect("shared-subscriptions").await;
    assert_eq!(count(&harness.radio().calls(), "set_notifications"), 1);
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 0);
    assert_eq!(
        count(&harness.radio().calls(), "set_notifications"),
        2,
        "caller release must disable its native subscription before forgetting the handle"
    );
    other.discover(&other_link).await;
    harness.dispatcher.authority_shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_owner_stream_send_failure_retains_exact_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("shared-stream-retry").await;
    let other = harness.other_caller().await;
    let other_link = other.connect("shared-stream-retry").await;
    harness.connection_events(&link, "owner-stream").await;
    harness.ready("owner-stream").await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let sends = attempts.clone();
    let events = harness.events.clone();
    let sink = IpcEventSink::new(Channel::new(move |body| {
        if sends.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            return Err(tauri::Error::Io(std::io::Error::other("terminal refused")));
        }
        if let InvokeResponseBody::Json(json) = body {
            events
                .lock()
                .unwrap()
                .push(serde_json::from_str(&json).unwrap());
        }
        Ok(())
    }));
    harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .get_mut(&harness.key())
        .unwrap()
        .event_sink = sink;
    let first = harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap_err();
    assert_eq!(first.code, BleErrorCode::PlatformTransport);
    assert!(
        harness
            .with_caller(|caller| caller.connections.contains_key(&link.handle)
                && caller.connection_events.contains_key("owner-stream"))
            .await
    );
    let (native_lease, native_release) = harness
        .with_caller(|caller| {
            let connection = &caller.connections[&link.handle];
            (connection.lease.clone(), connection.native_release)
        })
        .await;
    assert_eq!(native_release, Some(false));
    // The core deliberately refuses an already-retired lease. Thus the
    // successful dispatcher retry below proves it skips the native stage,
    // rather than relying on global released-lease tombstones.
    let repeated_native = harness
        .central
        .release_connection_lease("shared-stream-retry", &native_lease, OpControl::unbounded())
        .await
        .unwrap_err();
    assert_eq!(
        DispatchError::from_core(&repeated_native).code,
        BleErrorCode::OwnershipDenied
    );
    other.discover(&other_link).await;
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        attempts.load(AtomicOrdering::SeqCst),
        2,
        "one refused send and one successful terminal"
    );
    assert_eq!(harness.items("owner-stream").len(), 2);
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 0);
    other.discover(&other_link).await;
    harness.dispatcher.authority_shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_gatt_operation_cannot_borrow_another_native_lease() {
    let harness = Harness::new().await;
    let link = harness.connect("shared-gatt").await;
    let mut services = vec![hrm_service()];
    services[0].characteristics[2]
        .descriptors
        .push(DescriptorSnapshot {
            uuid: "00002901-0000-1000-8000-00805f9b34fb".into(),
            occurrence: 0,
        });
    harness.radio().set_services("shared-gatt", services);
    let database = harness.discover(&link).await;
    let other = harness.other_caller().await;
    let other_link = other.connect("shared-gatt").await;
    let own_lease = harness
        .with_caller(|caller| caller.connections[&link.handle].lease.clone())
        .await;
    assert_ne!(
        own_lease, LEASE_ID,
        "native connection lease is not renderer authority lease"
    );
    harness
        .execute(
            "gatt.read",
            Harness::gatt_entries(&link, &database, CONTROL_POINT),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let descriptor = harness
        .with_caller(|caller| {
            caller.databases[&database.handle]
                .descriptors
                .keys()
                .next()
                .unwrap()
                .clone()
        })
        .await;
    harness
        .central
        .release_connection_lease("shared-gatt", &own_lease, OpControl::unbounded())
        .await
        .unwrap();
    let before = harness.radio().calls();
    for operation in [
        "gatt.read",
        "gatt.write",
        "gatt.subscribe",
        "gatt.descriptor.read",
        "gatt.descriptor.write",
    ] {
        let mut entries = Harness::gatt_entries(
            &link,
            &database,
            if operation == "gatt.subscribe" {
                NOTIFY_ONLY
            } else {
                CONTROL_POINT
            },
        );
        entries.push(("descriptorHandle", string(descriptor.clone())));
        entries.push(("mode", string("with-response")));
        let bytes = operation.ends_with("write").then(|| vec![1]);
        let error = harness
            .execute(operation, entries, bytes, OpControl::unbounded())
            .await
            .expect_err("stale caller cannot borrow another owner's lease");
        assert_eq!(
            error.code,
            BleErrorCode::OwnershipDenied,
            "{operation}: {error:?}"
        );
    }
    assert_eq!(
        harness.radio().calls(),
        before,
        "refused GATT work never reaches native radio"
    );
    other.discover(&other_link).await;
    harness.dispatcher.authority_shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_orphan_drain_keeps_its_exact_identity_owned() {
    let harness = Harness::new().await;
    let scan = harness.orphan_a_scan(1).await;
    harness.radio().block_op(FaultOp::StopScan);
    let dispatcher = harness.dispatcher.clone();
    let authority = harness.dispatcher.ensure_authority().await.unwrap();
    let drain = tokio::spawn(async move { dispatcher.settle_orphan_debt(&authority, None).await });
    harness.wait_calls("stop_scan", 2).await;
    drain.abort();
    assert!(matches!(drain.await, Err(error) if error.is_cancelled()));
    assert!(harness
        .dispatcher
        .inner
        .lock()
        .await
        .orphan_debt
        .iter()
        .any(|debt| matches!(&debt.resource, OrphanResource::Scan(id) if *id == scan)));
    harness.radio().unblock_op(FaultOp::StopScan);
    harness.dispatcher.authority_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn held_orphan_drain_cannot_prevent_authoritative_shutdown() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(1).await;
    harness.radio().block_op(FaultOp::StopScan);
    elapse(100).await;
    assert_eq!(harness.stops(), 2, "automatic release is held in flight");
    let dispatcher = harness.dispatcher.clone();
    let shutdown = tokio::spawn(async move { dispatcher.authority_shutdown().await });
    elapse(6_000).await;
    assert!(
        harness.central.is_shut_down(),
        "authoritative parent shutdown must start after the bounded orphan drain"
    );
    harness.radio().unblock_op(FaultOp::StopScan);
    let report = shutdown.await.unwrap();
    assert!(report.core.is_some());
    assert!(
        !report.orphan_failures.is_empty(),
        "timed-out drain remains diagnostic evidence"
    );
    assert!(
        harness.debt().await.is_empty(),
        "confirmed parent cleanup retires child debt"
    );
    assert!(harness.dispatcher.authority_shutdown().await.core.is_none());
    let stops = harness.stops();
    elapse(60_000).await;
    assert_eq!(
        harness.stops(),
        stops,
        "retired debt cannot restart a native release"
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_orphan_snapshot_keeps_both_resources_and_other_callers() {
    let harness = Harness::on_current_runtime().await;
    let link = harness.connect("peer-a").await;
    let lease = harness
        .with_caller(|caller| caller.connections[&link.handle].lease.clone())
        .await;
    let authority = harness.dispatcher.ensure_authority().await.unwrap();
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "orphan disconnect refused");
    harness
        .dispatcher
        .compensate(
            &authority,
            &harness.key(),
            OrphanResource::Link {
                peer_id: link.peer_id,
                lease,
            },
        )
        .await;
    let scan = harness.orphan_a_scan(1).await;
    assert_eq!(harness.debt().await.len(), 2);
    assert!(harness
        .dispatcher
        .settle_orphan_debt(&authority, Some("unrelated-caller"))
        .await
        .is_empty());
    assert_eq!(
        harness.debt().await.len(),
        2,
        "caller-scoped release preserves other callers"
    );
    harness.radio().block_op(FaultOp::Disconnect);
    let dispatcher = harness.dispatcher.clone();
    let drain = tokio::spawn(async move { dispatcher.settle_orphan_debt(&authority, None).await });
    harness.wait_calls("disconnect", 2).await;
    drain.abort();
    assert!(matches!(drain.await, Err(error) if error.is_cancelled()));
    let state = harness.dispatcher.inner.lock().await;
    assert_eq!(
        state.orphan_debt.len(),
        2,
        "both the in-flight and unvisited identity stay owned"
    );
    assert!(state
        .orphan_debt
        .iter()
        .any(|debt| matches!(&debt.resource, OrphanResource::Scan(id) if *id == scan)));
    drop(state);
    harness.radio().unblock_op(FaultOp::Disconnect);
    harness.dispatcher.authority_shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn refused_parent_retains_timed_out_orphan_debt_until_clean_retry() {
    let harness = Harness::on_current_runtime().await;
    let scan = harness.orphan_a_scan(1).await;
    harness.radio().block_op(FaultOp::StopScan);
    harness
        .radio()
        .fail_next(FaultOp::FinishClose, "parent transport close refused");
    let dispatcher = harness.dispatcher.clone();
    let shutdown = tokio::spawn(async move { dispatcher.authority_shutdown().await });
    elapse(6_000).await;
    assert!(harness.stops() >= 3);
    harness.radio().unblock_op(FaultOp::StopScan);
    let first = shutdown.await.unwrap();
    assert!(!first.core.unwrap().transport_close_failures.is_empty());
    assert!(harness
        .dispatcher
        .inner
        .lock()
        .await
        .orphan_debt
        .iter()
        .any(|debt| matches!(&debt.resource, OrphanResource::Scan(id) if *id == scan)));
    assert!(harness.dispatcher.ensure_authority().await.is_err());
    let retry = harness.dispatcher.authority_shutdown().await;
    assert!(retry.core.unwrap().transport_close_failures.is_empty());
    assert!(harness.debt().await.is_empty());
    assert!(harness.dispatcher.authority_shutdown().await.core.is_none());
}

#[tokio::test(start_paused = true)]
async fn held_orphan_release_still_reaches_bounded_parent_transport_close() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(1).await;
    harness.radio().block_op(FaultOp::StopScan);
    elapse(100).await;
    let dispatcher = harness.dispatcher.clone();
    let shutdown = tokio::spawn(async move { dispatcher.authority_shutdown().await });
    elapse(16_000).await;
    assert_eq!(
        count(&harness.radio().calls(), "finish_close"),
        1,
        "parent transport close runs even while child radio gate stays blocked"
    );
    let report = shutdown.await.unwrap();
    assert!(report.core.unwrap().scan_stop_failure.is_some());
    assert_eq!(
        harness.debt().await.len(),
        1,
        "unconfirmed parent preserves child identity"
    );
    harness.radio().unblock_op(FaultOp::StopScan);
    let stops_before_retry = harness.stops();
    let retry = harness.dispatcher.authority_shutdown().await;
    assert!(
        harness.stops() > stops_before_retry,
        "retry must reach the retained native scan"
    );
    assert!(
        !harness.radio().scan_active(),
        "successful cleanup stops the physical scan"
    );
    assert_eq!(
        retry.core.unwrap().record.unwrap().state(),
        ubm_core::ownership::CleanupState::Released,
        "a later successful radio release must clear current parent debt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authority_shutdown_retains_transport_cleanup_until_a_clean_retry() {
    let harness = Harness::new().await;
    harness
        .radio()
        .fail_next(FaultOp::FinishClose, "transport close refused");
    let first = harness.dispatcher.authority_shutdown().await;
    assert!(!first
        .core
        .expect("first shutdown report")
        .transport_close_failures
        .is_empty());
    assert!(
        harness.dispatcher.ensure_authority().await.is_err(),
        "failed shutdown never reopens admission"
    );

    let second = harness.dispatcher.authority_shutdown().await;
    let report = second
        .core
        .expect("failed cleanup authority must survive for retry");
    assert!(
        report.transport_close_failures.is_empty(),
        "the latest successful retry is not poisoned by history"
    );
    assert_eq!(
        report.record.expect("clean core report").state(),
        ubm_core::ownership::CleanupState::Released
    );
    assert!(report.scan_stop_failure.is_none());
    assert!(report.radio_close_failures.is_empty());
    assert!(second.orphan_failures.is_empty());
    assert_eq!(count(&harness.radio().calls(), "finish_close"), 2);
    assert!(
        harness.dispatcher.authority_shutdown().await.core.is_none(),
        "successful cleanup retires authority debt"
    );
    assert!(harness.dispatcher.ensure_authority().await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authority_shutdown_closes_admission_during_transport_cleanup() {
    let harness = Harness::new().await;
    harness.radio().block_op(FaultOp::FinishClose);
    let dispatcher = harness.dispatcher.clone();
    let shutdown = tokio::spawn(async move { dispatcher.authority_shutdown().await });
    harness.wait_calls("finish_close", 1).await;
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        harness.dispatcher.ensure_authority()
    )
    .await
    .expect("closed admission must not wait behind cleanup")
    .is_err());
    harness.radio().unblock_op(FaultOp::FinishClose);
    assert!(shutdown
        .await
        .expect("shutdown task")
        .core
        .expect("report")
        .transport_close_failures
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_authority_shutdown_preserves_the_closed_retry_owner() {
    let harness = Harness::new().await;
    harness.radio().block_op(FaultOp::FinishClose);
    let dispatcher = harness.dispatcher.clone();
    let shutdown = tokio::spawn(async move { dispatcher.authority_shutdown().await });
    harness.wait_calls("finish_close", 1).await;
    shutdown.abort();
    assert!(matches!(shutdown.await, Err(error) if error.is_cancelled()));
    assert!(harness.dispatcher.ensure_authority().await.is_err());
    harness.radio().unblock_op(FaultOp::FinishClose);
    let retry = harness.dispatcher.authority_shutdown().await;
    assert!(retry
        .core
        .expect("cancelled shutdown retains authority")
        .transport_close_failures
        .is_empty());
    assert_eq!(count(&harness.radio().calls(), "finish_close"), 2);
    assert!(harness.dispatcher.authority_shutdown().await.core.is_none());
}

#[tokio::test(start_paused = true)]
async fn hung_authority_transport_cleanup_is_bounded_and_retryable() {
    let harness = Harness::on_current_runtime().await;
    harness.radio().block_op(FaultOp::FinishClose);
    let start = tokio::time::Instant::now();
    let first = harness.dispatcher.authority_shutdown().await;
    let failure = first
        .core
        .expect("bounded report")
        .transport_close_failures
        .into_iter()
        .next()
        .expect("transport timeout is reported");
    assert_eq!(failure.code(), BleErrorCode::OperationTimedOut);
    assert!(start.elapsed() <= Duration::from_secs(6));
    assert!(harness.dispatcher.ensure_authority().await.is_err());
    harness.radio().unblock_op(FaultOp::FinishClose);
    let retry = harness.dispatcher.authority_shutdown().await;
    assert!(retry
        .core
        .expect("timeout retains authority")
        .transport_close_failures
        .is_empty());
    assert!(harness.dispatcher.authority_shutdown().await.core.is_none());
}

fn flags(read: bool, write: bool, notify: bool, indicate: bool) -> PropertyFlags {
    PropertyFlags {
        read,
        write,
        write_without_response: false,
        notify,
        indicate,
    }
}

fn characteristic(uuid: &str, properties: PropertyFlags) -> CharacteristicSnapshot {
    CharacteristicSnapshot {
        uuid: uuid.to_owned(),
        occurrence: 0,
        properties,
        descriptors: Vec::new(),
    }
}

fn hrm_service() -> ServiceSnapshot {
    ServiceSnapshot {
        primary: None,
        included_services: None,
        uuid: HRM_SERVICE.to_owned(),
        occurrence: 0,
        characteristics: vec![
            characteristic(NOTIFY_ONLY, flags(false, false, true, false)),
            characteristic(INDICATE_ONLY, flags(false, false, false, true)),
            characteristic(CONTROL_POINT, flags(true, true, false, false)),
        ],
        access: std::default::Default::default(),
    }
}

fn advertisement(peer_id: &str) -> RadioEvent {
    RadioEvent::Advertisement(PeerSnapshot {
        id: peer_id.to_owned(),
        address: None,
        service_uuids: vec![HRM_SERVICE.to_owned()],
        rssi: Some(-60),
        local_name: None,
        manufacturer_data: Vec::new(),
        service_data: Vec::new(),
        tx_power_level: None,
        extras: ubm_desktop::AdvertisementExtras::default(),
    })
}

fn test_attachment() -> Attachment {
    Attachment {
        attachment_id: "attachment-test".to_owned(),
        backend_instance_id: "backend-test".to_owned(),
        backend_generation: "generation-test".to_owned(),
        adapter_id: "adapter-test".to_owned(),
        adapter_name: "fake".to_owned(),
        adapter_generation: "adapter-generation-test".to_owned(),
    }
}

fn number(value: i64) -> IpcValue {
    IpcValue::Number(serde_json::Number::from(value))
}

fn field<'a>(value: &'a IpcValue, key: &str) -> &'a IpcValue {
    match value {
        IpcValue::Object(fields) => fields
            .get(key)
            .unwrap_or_else(|| panic!("missing field {key} in {value:?}")),
        _ => panic!("expected an object, got {value:?}"),
    }
}

fn text(value: &IpcValue, key: &str) -> String {
    match field(value, key) {
        IpcValue::String(text) => text.clone(),
        other => panic!("field {key} is not a string: {other:?}"),
    }
}

fn count(calls: &[String], name: &str) -> usize {
    calls.iter().filter(|call| call.as_str() == name).count()
}

/// One connected peer as the renderer holds it.
#[derive(Clone)]
struct Link {
    handle: String,
    peer_id: String,
    connection_id: String,
    owner_lease_id: String,
    generation: String,
}

/// One discovered database as the renderer holds it.
#[derive(Clone)]
struct Database {
    handle: String,
    id: String,
    generation: String,
    characteristics: HashMap<String, String>,
}

struct Harness {
    dispatcher: BtleplugDispatcher,
    central: DesktopCentral<FakeRadio>,
    caller: AuthenticatedCaller,
    events: Arc<StdMutex<Vec<Value>>>,
    /// The attachment the caller was bound to at attach.
    attachment: Attachment,
}

impl Harness {
    /// Central opened on the shared desktop executor, as production opens
    /// it; one attached caller whose event channel records every delivery.
    async fn new() -> Self {
        Self::with_radio(FakeRadio::new()).await
    }

    /// Central over a radio scripted before the open (adapter facts and
    /// OS policy are read at open).
    async fn with_radio(radio: FakeRadio) -> Self {
        let central = ubm_desktop::executor::desktop_runtime()
            .spawn(DesktopCentral::open(radio, "tauri-test"))
            .await
            .expect("open task joins")
            .expect("fake radio opens");
        Self::over(central).await
    }

    /// Central opened on the current runtime (paused-clock tests).
    async fn on_current_runtime() -> Self {
        let central = DesktopCentral::open(FakeRadio::new(), "tauri-test")
            .await
            .expect("fake radio opens");
        Self::over(central).await
    }

    async fn over(central: DesktopCentral<FakeRadio>) -> Self {
        let authority: Arc<dyn CoreAuthority> = Arc::new(central.clone());
        let dispatcher = BtleplugDispatcher::with_core_authority(authority);
        let caller = AuthenticatedCaller::new("test-app".to_owned(), "main".to_owned());
        Self::over_dispatcher(central, dispatcher, caller).await
    }

    async fn other_caller(&self) -> Self {
        Self::over_dispatcher(
            self.central.clone(),
            self.dispatcher.clone(),
            AuthenticatedCaller::new("test-app".to_owned(), "other-window".to_owned()),
        )
        .await
    }

    async fn over_dispatcher(
        central: DesktopCentral<FakeRadio>,
        dispatcher: BtleplugDispatcher,
        caller: AuthenticatedCaller,
    ) -> Self {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let log = Arc::clone(&events);
        let sink = IpcEventSink::new(Channel::new(move |body| {
            if let InvokeResponseBody::Json(json) = body {
                log.lock()
                    .expect("event log")
                    .push(serde_json::from_str(&json).expect("event json"));
            }
            Ok(())
        }));
        // The attachment is the shared central's own (finding 43), read the
        // way bootstrap reads it.
        let attachment = dispatcher
            .ensure_adapter()
            .await
            .expect("the central's attachment");
        {
            let mut state = dispatcher.inner.lock().await;
            state.callers.insert(
                caller_key(&caller),
                CallerState {
                    lease_id: LEASE_ID.to_owned(),
                    lease_generation: LEASE_GENERATION.to_owned(),
                    versions: object([(
                        "ipcProtocol",
                        object([("selected", object([("value", number(10))]))]),
                    )]),
                    attachment: attachment.clone(),
                    event_sink: sink,
                    retired: false,
                    scan: None,
                    connections: HashMap::new(),
                    databases: HashMap::new(),
                    subscriptions: HashMap::new(),
                    ordered_groups: HashMap::new(),
                    acquired_writers: HashMap::new(),
                    acquired_releases: Default::default(),
                    connection_events: HashMap::new(),
                    security_watches: HashMap::new(),
                    security_watch_releases: std::collections::HashSet::new(),
                    write_readiness_watches: HashMap::new(),
                    write_readiness_releases: Default::default(),
                    parameter_watches: HashMap::new(),
                    parameter_releases: Default::default(),
                    operations: HashMap::new(),
                    completed_correlations: HashMap::new(),
                    pending_events: std::collections::HashSet::new(),
                },
            );
        }
        Self {
            dispatcher,
            central,
            caller,
            events,
            attachment,
        }
    }

    fn radio(&self) -> &FakeRadio {
        self.central.boundary()
    }

    fn key(&self) -> String {
        caller_key(&self.caller)
    }

    fn lease_payload(entries: Vec<(&str, IpcValue)>) -> BTreeMap<String, IpcValue> {
        let mut payload: BTreeMap<String, IpcValue> = entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        payload.insert("__expectedLeaseId".to_owned(), string(LEASE_ID));
        payload.insert(
            "__expectedLeaseGeneration".to_owned(),
            string(LEASE_GENERATION),
        );
        payload
    }

    /// Run one command on the current runtime, as `route` would after
    /// admission, with an explicit control.
    async fn execute(
        &self,
        command: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
        ctl: OpControl,
    ) -> Result<IpcValue, DispatchError> {
        self.dispatcher
            .execute(
                &self.caller,
                command,
                Self::lease_payload(entries),
                bytes,
                ctl,
            )
            .await
    }

    /// The full IPC route request the webview sends.
    fn route_request(
        &self,
        command: &str,
        correlation: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
    ) -> BTreeMap<String, IpcValue> {
        let payload = IpcValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        );
        let envelope = object([
            ("command", string(command)),
            ("correlation", string(correlation)),
            ("payload", payload),
            (
                "rendererLease",
                object([
                    ("leaseId", string(LEASE_ID)),
                    ("generation", string(LEASE_GENERATION)),
                ]),
            ),
            (
                "attachmentId",
                string(self.attachment.attachment_id.clone()),
            ),
            ("attachment", attachment_record(&self.attachment)),
            (
                "renderer",
                object([
                    (
                        "clientId",
                        string(format!(
                            "{}:{}",
                            self.caller.app_identifier, self.caller.window_label
                        )),
                    ),
                    ("windowScope", string(self.caller.window_label.clone())),
                    ("sessionScope", string(LEASE_GENERATION)),
                ]),
            ),
            // A route echoes the exact negotiated protocol selection from
            // bootstrap; an empty object is not a stand-in for that proof.
            // These packet-B harness callers are installed at protocol 10.
            (
                "versions",
                object([(
                    "ipcProtocol",
                    object([("selected", object([("value", number(10))]))]),
                )]),
            ),
            (
                "binaryPayload",
                bytes.map_or(IpcValue::Null, IpcValue::Bytes),
            ),
        ]);
        BTreeMap::from([
            ("kind".to_owned(), string("route")),
            ("envelope".to_owned(), envelope),
        ])
    }

    async fn route(
        &self,
        command: &str,
        correlation: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
    ) -> Result<IpcValue, DispatchError> {
        self.dispatcher
            .route(
                self.caller.clone(),
                self.route_request(command, correlation, entries, bytes),
            )
            .await
            .map(|response| field(&response, "payload").clone())
    }

    fn spawn_route(
        &self,
        command: &str,
        correlation: &str,
        entries: Vec<(&str, IpcValue)>,
        bytes: Option<Vec<u8>>,
    ) -> tokio::task::JoinHandle<Result<IpcValue, DispatchError>> {
        let dispatcher = self.dispatcher.clone();
        let caller = self.caller.clone();
        let request = self.route_request(command, correlation, entries, bytes);
        tokio::spawn(async move {
            dispatcher
                .route(caller, request)
                .await
                .map(|response| field(&response, "payload").clone())
        })
    }

    async fn cancel(&self, target: &str) -> String {
        let answer = self
            .route(
                "operation.cancel",
                &format!("cancel-{target}"),
                vec![("targetCorrelation", string(target))],
                None,
            )
            .await
            .expect("cancel answers");
        text(&answer, "state")
    }

    /// The radio has heard `peer_id` and resolves it; services scripted.
    async fn advertise(&self, peer_id: &str) {
        self.radio().set_services(peer_id, vec![hrm_service()]);
        self.radio().set_mtu(peer_id, 185);
        self.radio().push_event(advertisement(peer_id));
        for _ in 0..5000 {
            if self.central.peer_key_for(peer_id).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("peer {peer_id} never resolved");
    }

    async fn connect(&self, peer_id: &str) -> Link {
        self.advertise(peer_id).await;
        let response = self
            .execute(
                "connection.connect",
                vec![("peerId", string(peer_id))],
                None,
                OpControl::unbounded(),
            )
            .await
            .expect("connect");
        Link {
            handle: text(&response, "handle"),
            peer_id: peer_id.to_owned(),
            connection_id: text(&response, "connectionId"),
            owner_lease_id: text(&response, "ownerLeaseId"),
            generation: text(&response, "connectionGeneration"),
        }
    }

    fn link_entries(link: &Link) -> Vec<(&'static str, IpcValue)> {
        vec![
            ("connectionHandle", string(link.handle.clone())),
            ("peerId", string(link.peer_id.clone())),
            ("connectionId", string(link.connection_id.clone())),
            ("ownerLeaseId", string(link.owner_lease_id.clone())),
            ("connectionGeneration", string(link.generation.clone())),
        ]
    }

    async fn discover(&self, link: &Link) -> Database {
        let response = self
            .execute(
                "gatt.discover",
                Self::link_entries(link),
                None,
                OpControl::unbounded(),
            )
            .await
            .expect("discover");
        let IpcValue::Array(records) = field(&response, "characteristics") else {
            panic!("characteristics must be an array");
        };
        let characteristics = records
            .iter()
            .map(|record| (text(record, "characteristicUuid"), text(record, "handle")))
            .collect();
        Database {
            handle: text(&response, "handle"),
            id: text(&response, "databaseId"),
            generation: text(&response, "databaseGeneration"),
            characteristics,
        }
    }

    fn gatt_entries(link: &Link, database: &Database, uuid: &str) -> Vec<(&'static str, IpcValue)> {
        let mut entries = Self::link_entries(link);
        entries.extend([
            ("databaseHandle", string(database.handle.clone())),
            ("databaseId", string(database.id.clone())),
            ("databaseGeneration", string(database.generation.clone())),
            (
                "characteristicHandle",
                string(database.characteristics[uuid].clone()),
            ),
        ]);
        entries
    }

    async fn subscribe(
        &self,
        link: &Link,
        database: &Database,
        uuid: &str,
        mode: Option<&str>,
    ) -> Result<IpcValue, DispatchError> {
        let mut entries = Self::gatt_entries(link, database, uuid);
        if let Some(mode) = mode {
            entries.push(("deliveryMode", string(mode)));
        }
        self.execute("gatt.subscribe", entries, None, OpControl::unbounded())
            .await
    }

    async fn connection_events(&self, link: &Link, stream: &str) {
        let mut entries = Self::link_entries(link);
        entries.push(("connectionEventsHandle", string(stream)));
        self.execute(
            "connection.events.subscribe",
            entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("connection events subscribe");
    }

    async fn ready(&self, stream: &str) {
        self.execute(
            "connection.events.ready",
            vec![("connectionEventsHandle", string(stream))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("connection events ready");
    }

    /// Items delivered on one stream, in delivery order.
    fn items(&self, stream: &str) -> Vec<Value> {
        self.events
            .lock()
            .expect("event log")
            .iter()
            .filter(|event| event["streamId"] == stream)
            .map(|event| event["item"].clone())
            .collect()
    }

    async fn wait_items(&self, stream: &str, at_least: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let items = self.items(stream);
            if items.len() >= at_least {
                return items;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stream {stream} delivered {} of {at_least} items: {items:?}",
                items.len()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn wait_calls(&self, name: &str, at_least: usize) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while count(&self.radio().calls(), name) < at_least {
            assert!(
                tokio::time::Instant::now() < deadline,
                "radio never saw {at_least} {name} calls: {:?}",
                self.radio().calls()
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn wait_tracked(&self, correlation: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let tracked = self
                .dispatcher
                .inner
                .lock()
                .await
                .callers
                .get(&self.key())
                .is_some_and(|caller| caller.operations.contains_key(correlation));
            if tracked {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "correlation {correlation} never tracked"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Push one notification for the live enablement of `uuid` on `peer_id`.
    fn notify(&self, peer_id: &str, uuid: &str, value: Vec<u8>) {
        let epoch = self
            .radio()
            .enable_epochs()
            .iter()
            .rev()
            .find(|(key, _)| key.0 == peer_id && key.3 == uuid)
            .map(|(_, epoch)| *epoch)
            .expect("an enablement for this characteristic");
        self.radio().push_event(RadioEvent::Notification {
            peer_id: peer_id.to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: uuid.to_owned(),
            characteristic_occurrence: 0,
            epoch,
            value,
        });
    }

    async fn with_caller<T>(&self, read: impl FnOnce(&CallerState) -> T) -> T {
        let state = self.dispatcher.inner.lock().await;
        read(state.callers.get(&self.key()).expect("caller mapped"))
    }

    async fn replace_lease(&self) {
        let mut state = self.dispatcher.inner.lock().await;
        let caller = state.callers.get_mut(&self.key()).expect("caller mapped");
        caller.lease_id = "lease-2".to_owned();
        caller.lease_generation = "generation-2".to_owned();
    }
}

fn value_bytes(item: &Value) -> Vec<u8> {
    item["value"]["value"]["$__unifiedBleBytesV2"]
        .as_array()
        .expect("byte value")
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().expect("byte")).expect("u8"))
        .collect()
}

fn empty_scan_query() -> IpcValue {
    let canonical = "{\"anyOf\":null,\"exclude\":null}";
    let mut hash = 0xcbf29ce484222325_u64;
    for code_unit in canonical.encode_utf16() {
        hash ^= u64::from(code_unit);
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    object([
        ("anyOf", IpcValue::Null),
        ("exclude", IpcValue::Null),
        ("digest", string(format!("scan-query-v1:{hash:016x}"))),
    ])
}

fn wire_error(error: &DispatchError) -> Value {
    error.normalized_error().into_wire()
}

// Trusted continuation and offline recording use the same Tauri authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_continuation_replays_setup_and_retains_recording_without_offline_radio_open() {
    let module = module_path!().split_once("::").unwrap().1;
    let test = format!(
        "{module}::trusted_continuation_replays_setup_and_retains_recording_without_offline_radio_open"
    );
    let Some(directory) = recording_fixture::isolated_fixture_process(&test) else {
        return;
    };
    trusted_continuation_recording_scenario(&directory).await;
    recording_fixture::complete_fixture_process(&directory);
}

async fn trusted_continuation_recording_scenario(directory: &std::path::Path) {
    let harness = Harness::new().await;
    let peer = "native-recorded-peer";
    harness.advertise(peer).await;
    harness
        .dispatcher
        .continuation_configure_recording_directory(directory)
        .await
        .unwrap();
    let declaration = serde_json::json!({
        "onAppearance":"native", "peerId":peer,
        "resubscribe":[{"serviceUuid":HRM_SERVICE,"characteristicUuid":NOTIFY_ONLY}],
        "setup":[{"selector":{"serviceUuid":HRM_SERVICE,"characteristicUuid":CONTROL_POINT},
            "value":[2,0],"timeoutMs":4000,
            "response":{"subscriptionIndex":0,"prefix":[240,2,0],"minLength":4,"maxLength":4,
                "status":{"offset":3,"accepted":[0]}}}],
        "recording":{"id":"tauri-native","maxBytes":1048576,"maxRecords":1000}
    })
    .to_string();
    harness.radio().block_op(FaultOp::Write);
    let dispatcher = harness.dispatcher.clone();
    let mut first =
        tokio::spawn(async move { dispatcher.continuation_execute(peer, &declaration).await });
    tokio::select! {
        _ = harness.wait_calls("write_characteristic", 1) => {}
        result = &mut first => panic!("continuation ended before its setup write: {result:?}"),
    }
    let notify = |value, epoch| RadioEvent::Notification {
        peer_id: peer.into(),
        service_uuid: HRM_SERVICE.into(),
        service_occurrence: 0,
        characteristic_uuid: NOTIFY_ONLY.into(),
        characteristic_occurrence: 0,
        value,
        epoch,
    };
    let before_ack = harness
        .dispatcher
        .continuation_recording_status("tauri-native")
        .await
        .unwrap()["records"]
        .as_u64()
        .unwrap();
    harness.radio().push_event(notify(
        vec![240, 2, 0, 0],
        harness.central.routing_epoch(peer).await,
    ));
    tokio::time::timeout(WAIT, async {
        loop {
            if harness
                .dispatcher
                .continuation_recording_status("tauri-native")
                .await
                .unwrap()["records"]
                .as_u64()
                .unwrap()
                > before_ack
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let early = harness
        .dispatcher
        .continuation_recording_prepare("tauri-native", 256, 65536)
        .await
        .unwrap();
    assert!(
        early["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["record"]["valueB64"] == "8AIAAA=="),
        "prove actual ACK ingress before the negative completion assertion"
    );
    assert_eq!(
        harness
            .dispatcher
            .continuation_recording_acknowledge("tauri-native", early["token"].as_str().unwrap())
            .await
            .unwrap()["acknowledged"],
        true
    );
    assert!(
        !first.is_finished(),
        "early application ACK must not bypass held ATT"
    );
    harness.radio().unblock_op(FaultOp::Write);
    assert_eq!(
        tokio::time::timeout(WAIT, first)
            .await
            .unwrap()
            .unwrap()
            .unwrap()["event"],
        "continuation.completed"
    );
    for (generation, value) in [(1, 72), (2, 73)] {
        if generation == 2 {
            harness.radio().block_op(FaultOp::Write);
            harness.central.remote_peer_loss(peer).await.unwrap();
            harness.wait_calls("write_characteristic", 2).await;
            harness.radio().push_event(notify(
                vec![240, 2, 0, 0],
                harness.central.routing_epoch(peer).await,
            ));
            harness.radio().unblock_op(FaultOp::Write);
            tokio::time::timeout(WAIT, async {
                loop {
                    match harness.dispatcher.continuation_describe_backlog().await {
                        Ok(status)
                            if status["continuationOutcome"]["event"]
                                == "continuation.completed" =>
                        {
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => assert_eq!(error["code"], "lifecycle.invalid-state"),
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let before = harness
            .dispatcher
            .continuation_recording_status("tauri-native")
            .await
            .unwrap()["records"]
            .as_u64()
            .unwrap();
        harness.radio().push_event(notify(
            vec![0, value],
            harness.central.routing_epoch(peer).await,
        ));
        tokio::time::timeout(WAIT, async {
            loop {
                if harness
                    .dispatcher
                    .continuation_recording_status("tauri-native")
                    .await
                    .unwrap()["records"]
                    .as_u64()
                    .unwrap()
                    > before
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    harness
        .dispatcher
        .continuation_recording_stop("tauri-native")
        .await
        .unwrap();
    let claim = harness
        .dispatcher
        .continuation_prepare_claim(256, 65536)
        .await
        .unwrap();
    assert_eq!(claim["recording"]["id"], "tauri-native");
    assert_eq!(
        harness
            .dispatcher
            .continuation_acknowledge_claim(claim["claimToken"].as_str().unwrap())
            .await
            .unwrap()["disposed"],
        true
    );
    let batch = harness
        .dispatcher
        .continuation_recording_prepare("tauri-native", 256, 65536)
        .await
        .unwrap();
    let records = batch["records"].as_array().unwrap();
    for value in ["AEg=", "AEk="] {
        assert!(
            records
                .iter()
                .any(|entry| entry["record"]["valueB64"] == value),
            "missing durable positive value {value}"
        );
    }
    let generations: Vec<_> = ["AEg=", "AEk="]
        .into_iter()
        .map(|value| {
            let entry = records
                .iter()
                .find(|entry| entry["record"]["valueB64"] == value)
                .unwrap();
            assert_eq!(entry["metadata"]["session"]["peerId"], peer);
            entry["metadata"]["consumer"]["connectionGeneration"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_ne!(
        generations[0], generations[1],
        "recovery must retain a new generation identity"
    );
    let report = harness.dispatcher.authority_shutdown().await;
    assert!(report.core.is_some());
    assert!(report.orphan_failures.is_empty());
    assert!(!harness.radio().link_connected(peer));
    drop(harness);

    // A fresh trusted dispatcher must retrieve the same durable prefix without
    // initializing an adapter, even after the original radio owner is gone.
    let offline = BtleplugDispatcher::with_authority_opener(Arc::new(|| {
        panic!("offline recording opened radio")
    }));
    offline
        .continuation_configure_recording_directory(directory)
        .await
        .unwrap();
    assert_eq!(
        offline
            .continuation_recording_prepare("tauri-native", 256, 65536)
            .await
            .unwrap(),
        batch
    );
    let token = batch["token"].as_str().unwrap();
    let receipt = offline
        .continuation_recording_acknowledge("tauri-native", token)
        .await
        .unwrap();
    assert_eq!(receipt["acknowledged"], true);
    assert_eq!(receipt["records"].as_u64().unwrap(), records.len() as u64);
    assert_eq!(
        offline
            .continuation_recording_status("tauri-native")
            .await
            .unwrap()["records"],
        0
    );
    drop(offline);
}

// PR210-04 — one peer's hung discovery must not stall another peer's
// notifications, a cancel, or shutdown: the dispatcher shares the central
// and never holds a lock across radio work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_04_a_hung_discovery_never_stalls_another_peer_cancel_or_shutdown() {
    let harness = Harness::new().await;
    let a = harness.connect("peer-a").await;
    let b = harness.connect("peer-b").await;
    let b_database = harness.discover(&b).await;
    let subscription = harness
        .subscribe(&b, &b_database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe b");
    let b_stream = text(&subscription, "handle");

    harness.radio().block_op(FaultOp::Discover);
    let discovers = count(&harness.radio().calls(), "discover");
    let hung = harness.spawn_route(
        "gatt.discover",
        "discover-a",
        Harness::link_entries(&a),
        None,
    );
    harness.wait_calls("discover", discovers + 1).await;

    for index in 0..50_u8 {
        harness.notify("peer-b", NOTIFY_ONLY, vec![index]);
    }
    let items = harness.wait_items(&b_stream, 50).await;
    let delivered: Vec<u8> = items.iter().flat_map(value_bytes).collect();
    assert_eq!(delivered, (0..50).collect::<Vec<u8>>(), "all 50, in order");

    assert_eq!(harness.cancel("discover-a").await, "cancellation-requested");
    let error = tokio::time::timeout(WAIT, hung)
        .await
        .expect("the cancel settles while the radio call is still held")
        .expect("route joins")
        .expect_err("cancelled discovery fails");
    assert_eq!(error.code, BleErrorCode::OperationAborted);

    let held_again = harness.spawn_route(
        "gatt.discover",
        "discover-a-2",
        Harness::link_entries(&a),
        None,
    );
    harness.wait_calls("discover", discovers + 2).await;
    let shutdown = tokio::time::timeout(WAIT, harness.dispatcher.authority_shutdown())
        .await
        .expect("shutdown finishes while a discovery is held");
    assert!(shutdown.core.is_some());
    harness.radio().unblock_op(FaultOp::Discover);
    let _ = tokio::time::timeout(WAIT, held_again).await;
}

// PR210-05 — a cancel targets exactly its own core operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_05_a_cancel_targets_exactly_one_of_two_dispatched_writes() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let entries = || {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("mode", string("with-response")));
        entries
    };
    harness.radio().block_op(FaultOp::Write);
    let first = harness.spawn_route("gatt.write", "write-1", entries(), Some(vec![1]));
    let second = harness.spawn_route("gatt.write", "write-2", entries(), Some(vec![2]));
    harness.wait_calls("write_characteristic", 2).await;

    assert_eq!(harness.cancel("write-1").await, "cancellation-requested");
    let aborted = tokio::time::timeout(WAIT, first)
        .await
        .expect("the cancelled write settles")
        .expect("route joins")
        .expect_err("the cancelled write fails");
    assert_eq!(aborted.code, BleErrorCode::OperationAborted);
    assert_eq!(
        aborted.retryability,
        Retryability::Never,
        "a dispatched write may have committed: never caller-retryable (PR210-22)"
    );
    assert_eq!(aborted.commit, Some(CommitState::Unknown));
    let wire = wire_error(&aborted);
    assert_eq!(wire["retryability"], "never");
    assert_eq!(wire["commit"], "uncertain");

    harness.radio().unblock_op(FaultOp::Write);
    tokio::time::timeout(WAIT, second)
        .await
        .expect("the other write completes")
        .expect("route joins")
        .expect("the other write is unaffected");
    assert_eq!(
        count(&harness.radio().calls(), "write_characteristic"),
        2,
        "one radio write per operation, none repeated"
    );
    assert_eq!(
        harness.radio().writes().len(),
        1,
        "only the unaffected write completed at the radio"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_05_a_cancel_before_admission_never_reaches_the_radio() {
    let harness = Harness::new().await;
    harness.advertise("peer-a").await;
    // Hold the authority slot: the connect is tracked but cannot reach the
    // core, so the cancel arrives before any core id exists.
    let slot = harness.dispatcher.authority.lock().await;
    let connect = harness.spawn_route(
        "connection.connect",
        "connect-1",
        vec![("peerId", string("peer-a"))],
        None,
    );
    harness.wait_tracked("connect-1").await;
    assert_eq!(harness.cancel("connect-1").await, "cancellation-requested");
    drop(slot);

    let error = tokio::time::timeout(WAIT, connect)
        .await
        .expect("connect settles")
        .expect("route joins")
        .expect_err("cancelled connect fails");
    assert_eq!(error.code, BleErrorCode::OperationAborted);
    assert_eq!(error.retryability, Retryability::CallerDecides);
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(wire_error(&error)["commit"], "not-dispatched");
    assert_eq!(
        count(&harness.radio().calls(), "connect"),
        0,
        "no radio call"
    );
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_05_a_success_that_beat_the_cancel_is_returned_and_owned() {
    let harness = Harness::new().await;
    harness.advertise("peer-a").await;
    harness.radio().block_op(FaultOp::Connect);
    let connect = harness.spawn_route(
        "connection.connect",
        "connect-1",
        vec![("peerId", string("peer-a"))],
        None,
    );
    harness.wait_calls("connect", 1).await;
    harness.radio().unblock_op(FaultOp::Connect);
    let connected = tokio::time::timeout(WAIT, connect)
        .await
        .expect("connect settles")
        .expect("route joins")
        .expect("connect succeeds");
    assert_eq!(harness.cancel("connect-1").await, "already-terminal");
    let handle = text(&connected, "handle");
    assert!(
        harness
            .with_caller(|caller| caller.connections.contains_key(&handle))
            .await,
        "the returned handle is owned: no lease without an owner"
    );
}

// PR210-06 — the caller budget crosses as relative `budgetMs` and is
// admitted on the plugin clock when the route arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_budget_ms_is_validated_before_any_effect() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let reads = || count(&harness.radio().calls(), "read_characteristic");
    let before = reads();
    let malformed: Vec<IpcValue> = vec![
        number(-1),
        IpcValue::Number(serde_json::Number::from_f64(1.5).expect("finite")),
        string("50"),
        IpcValue::Number(serde_json::Number::from(9_007_199_254_740_992_u64)),
    ];
    for (index, budget) in malformed.into_iter().enumerate() {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("budgetMs", budget));
        let error = harness
            .route("gatt.read", &format!("bad-{index}"), entries, None)
            .await
            .expect_err("a malformed budget is refused");
        assert_eq!(
            error.identity(),
            ("protocol.malformed", "ipc", "tauri.route-budget".to_owned())
        );
    }
    let mut spent = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    spent.push(("budgetMs", number(0)));
    let error = harness
        .route("gatt.read", "spent", spent, None)
        .await
        .expect_err("a spent budget times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(error.retryability, Retryability::CallerDecides);
    assert_eq!(reads(), before, "no radio call for a spent budget");

    let mut absent = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    absent.push(("budgetMs", IpcValue::Null));
    harness
        .route("gatt.read", "unbounded", absent, None)
        .await
        .expect("null means no caller budget");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_a_short_budget_bounds_the_radio_call() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().block_op(FaultOp::Read);
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    // Room to reach the radio under parallel test load; the held read then
    // ends on the budget, far inside every liveness backstop.
    entries.push(("budgetMs", number(500)));
    let error = tokio::time::timeout(WAIT, harness.route("gatt.read", "short", entries, None))
        .await
        .expect("a 500 ms budget ends the read")
        .expect_err("the read times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(
        error.platform, None,
        "a caller-budget expiry is not a liveness backstop"
    );
    assert_eq!(count(&harness.radio().calls(), "read_characteristic"), 1);
    harness.radio().unblock_op(FaultOp::Read);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_queueing_behind_the_dispatcher_spends_the_budget() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let before = count(&harness.radio().calls(), "read_characteristic");
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    entries.push(("budgetMs", number(50)));
    let state = harness.dispatcher.inner.lock().await;
    let queued = harness.spawn_route("gatt.read", "queued", entries, None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(state);
    let error = tokio::time::timeout(WAIT, queued)
        .await
        .expect("the queued read settles")
        .expect("route joins")
        .expect_err("the budget ran out while queued");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(
        count(&harness.radio().calls(), "read_characteristic"),
        before,
        "the expired read never reached the radio"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_06_a_write_that_times_out_after_dispatch_is_never_retryable() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().block_op(FaultOp::Write);
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    entries.push(("mode", string("with-response")));
    entries.push(("budgetMs", number(500)));
    let error = tokio::time::timeout(
        WAIT,
        harness.route("gatt.write", "write", entries, Some(vec![7])),
    )
    .await
    .expect("the budget ends the write")
    .expect_err("the write times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(error.retryability, Retryability::Never);
    assert_eq!(wire_error(&error)["commit"], "uncertain");
    assert_eq!(count(&harness.radio().calls(), "write_characteristic"), 1);
    harness.radio().unblock_op(FaultOp::Write);
}

/// A 45 s budget outlives the old fixed 30 s backstop: the read that
/// answers at 40 s succeeds.
#[tokio::test(start_paused = true)]
async fn pr210_06_a_budget_longer_than_the_old_backstop_is_honoured() {
    let harness = Harness::on_current_runtime().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().block_op(FaultOp::Read);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(Harness::gatt_entries(&link, &database, CONTROL_POINT));
    let read = tokio::spawn(async move {
        dispatcher
            .execute(
                &caller,
                "gatt.read",
                payload,
                None,
                OpControl::new(ubm_desktop::Budget::from_ms(45_000), OpTicket::new()),
            )
            .await
    });
    while count(&harness.radio().calls(), "read_characteristic") == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(40)).await;
    harness.radio().unblock_op(FaultOp::Read);
    let value = read
        .await
        .expect("read joins")
        .expect("answered at 40 s of 45");
    assert_eq!(field(&value, "value"), &IpcValue::Bytes(vec![0x42]));
}

// A read answer carries the radio's own provenance verbatim: CoreBluetooth
// answering a read on a notifying characteristic says the value may be a
// notification, and the host never rewrites it into a read response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_carries_the_radio_provenance() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    for (provenance, wire) in [
        (ubm_desktop::ReadProvenance::ReadResponse, "read-response"),
        (
            ubm_desktop::ReadProvenance::ReadOrNotification,
            "read-or-notification",
        ),
    ] {
        harness.radio().script_read_provenance(provenance);
        let value = harness
            .execute(
                "gatt.read",
                Harness::gatt_entries(&link, &database, CONTROL_POINT),
                None,
                OpControl::unbounded(),
            )
            .await
            .expect("read");
        assert_eq!(field(&value, "value"), &IpcValue::Bytes(vec![0x42]));
        assert_eq!(field(&value, "provenance"), &string(wire));
    }
}

// PR210-07 — forwarders spawn only after their entry is published, so the
// first observation and the first notification are delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_07_scan_and_notification_forwarders_deliver_from_the_first_item() {
    let harness = Harness::new().await;
    // A sighting the OS reports while its scan start is still in flight
    // belongs to that scan (finding 121: before any scan it is no
    // observation at all). It is queued before the scan is published, so
    // the forwarder's very first poll finds it — which is when a forwarder
    // that raced its publication lost data.
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("start_scan", 1).await;
    harness.radio().push_event(advertisement("peer-early"));
    let deadline = tokio::time::Instant::now() + WAIT;
    while harness
        .central
        .resource_counters()
        .await
        .queued_advertisements
        < 1
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the in-flight scan never queued its sighting"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    harness.radio().unblock_op(FaultOp::StartScan);
    let scan = tokio::time::timeout(WAIT, start)
        .await
        .expect("start settles")
        .expect("joins")
        .expect("scan starts");
    let scan_stream = text(&scan, "handle");
    assert!(
        harness
            .with_caller(|caller| caller
                .scan
                .as_ref()
                .is_some_and(|scan| scan.handle == scan_stream && scan.task.is_some()))
            .await,
        "published with its forwarder"
    );
    let first = harness.wait_items(&scan_stream, 1).await;
    assert_eq!(first[0]["value"]["peerId"], "peer-early");
    harness.radio().push_event(advertisement("peer-late"));
    let later = harness.wait_items(&scan_stream, 2).await;
    assert_eq!(later[1]["value"]["peerId"], "peer-late");

    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let stream = text(&subscription, "handle");
    harness.notify("peer-a", NOTIFY_ONLY, vec![9]);
    let items = harness.wait_items(&stream, 1).await;
    assert_eq!(value_bytes(&items[0]), vec![9]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_07_a_lease_replaced_during_admission_stops_the_scan_it_admitted() {
    let harness = Harness::new().await;
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("start_scan", 1).await;
    harness.replace_lease().await;
    harness.radio().unblock_op(FaultOp::StartScan);
    let error = tokio::time::timeout(WAIT, start)
        .await
        .expect("start settles")
        .expect("joins")
        .expect_err("a stale lease cannot own the scan");
    assert_eq!(error.identity().2, "tauri.scan-stale-lease");
    assert_eq!(count(&harness.radio().calls(), "stop_scan"), 1);
    assert_eq!(
        harness.central.active_scan_id(),
        None,
        "the admitted scan stopped"
    );
    assert!(harness.with_caller(|caller| caller.scan.is_none()).await);
    assert!(harness.dispatcher.inner.lock().await.orphan_debt.is_empty());
}

// PR210-08 — a caller that disappears during admission gets a real,
// identity-scoped compensation; a failed compensation is retained debt.
// Paused clock: the automatic retries (finding 114) run only when this test
// lets time pass, so every radio call below is the one the test names.
#[tokio::test(start_paused = true)]
async fn pr210_08_a_caller_released_during_scan_admission_gets_a_real_stop() {
    let harness = Harness::on_current_runtime().await;
    let admitted = tokio::time::Instant::now();
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("start_scan", 1).await;
    harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .remove(&harness.key());
    harness.radio().fail_next(FaultOp::StopScan, "stop refused");
    harness
        .radio()
        .fail_next(FaultOp::StopScan, "stop refused again");
    harness.radio().unblock_op(FaultOp::StartScan);
    let error = tokio::time::timeout(WAIT, start)
        .await
        .expect("start settles")
        .expect("joins")
        .expect_err("nobody owns the scan");
    assert_eq!(error.code, BleErrorCode::OwnershipDenied);
    let scan_id = harness
        .central
        .active_scan_id()
        .expect("the stop failed: still owned");
    {
        let state = harness.dispatcher.inner.lock().await;
        assert_eq!(state.orphan_debt.len(), 1, "failed compensation is debt");
        assert!(
            matches!(&state.orphan_debt[0].resource, OrphanResource::Scan(id) if *id == scan_id)
        );
    }

    assert_no_automatic_retry_yet(admitted);
    let first = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(first.clone().into_wire()["state"], "release-failed");
    assert_eq!(first.into_wire()["failures"][0]["resourceKind"], "scan");
    let second = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(second.into_wire()["state"], "released");
    assert_eq!(count(&harness.radio().calls(), "stop_scan"), 3);
    assert_eq!(harness.central.active_scan_id(), None);
    assert!(harness.dispatcher.inner.lock().await.orphan_debt.is_empty());
    elapse(60_000).await;
    assert_eq!(
        count(&harness.radio().calls(), "stop_scan"),
        3,
        "a settled debt is never retried"
    );
}

/// The releases a test drives run before the first automatic retry is due,
/// so the radio calls it counts are exactly its own.
fn assert_no_automatic_retry_yet(since: tokio::time::Instant) {
    assert!(
        since.elapsed() < super::ORPHAN_RETRY_FIRST_DELAY,
        "{:?} of paused time passed; the first automatic retry is due at {:?}",
        since.elapsed(),
        super::ORPHAN_RETRY_FIRST_DELAY
    );
}

#[tokio::test(start_paused = true)]
async fn pr210_08_connect_and_subscribe_compensation_is_recorded_not_discarded() {
    let harness = Harness::on_current_runtime().await;
    harness.advertise("peer-a").await;
    let admitted = tokio::time::Instant::now();
    harness.radio().block_op(FaultOp::Connect);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("peerId", string("peer-a"))]);
    let connect = tokio::spawn(async move {
        dispatcher
            .connect(&caller, payload, OpControl::unbounded())
            .await
    });
    harness.wait_calls("connect", 1).await;
    harness.replace_lease().await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "release refused");
    harness.radio().unblock_op(FaultOp::Connect);
    let error = tokio::time::timeout(WAIT, connect)
        .await
        .expect("connect settles")
        .expect("joins")
        .expect_err("a stale lease cannot own the link");
    assert_eq!(error.identity().2, "tauri.connect-stale-lease");
    {
        let state = harness.dispatcher.inner.lock().await;
        assert_eq!(state.orphan_debt.len(), 1);
        assert!(matches!(
            &state.orphan_debt[0].resource,
            OrphanResource::Link { peer_id, .. } if peer_id == "peer-a"
        ));
    }
    assert_no_automatic_retry_yet(admitted);
    let released = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(
        released.into_wire()["state"],
        "released",
        "the retry releases the link"
    );
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(!harness.radio().link_connected("peer-a"));
    elapse(60_000).await;
    assert_eq!(
        count(&harness.radio().calls(), "disconnect"),
        2,
        "a settled debt is never retried"
    );
}

// PR210-09 — a failed release keeps the resource and its native identity;
// the retry calls native again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_scan_stop_keeps_the_scan_for_a_real_retry() {
    let harness = Harness::new().await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let scan_id = harness.central.active_scan_id().expect("scan owned");
    harness.radio().fail_next(FaultOp::StopScan, "stop refused");
    let stop = || {
        harness.execute(
            "scan.stop",
            vec![("scanHandle", string(handle.clone()))],
            None,
            OpControl::unbounded(),
        )
    };
    let error = stop().await.expect_err("the first stop fails truthfully");
    assert_eq!(error.code, BleErrorCode::ScanStopFailed);
    assert!(
        harness
            .with_caller(|caller| caller.scan.as_ref().is_some_and(|scan| {
                scan.core_operation_id == scan_id
                    && matches!(scan.phase, ReleasePhase::ReleaseFailed)
            }))
            .await,
        "the mapping and its core id survive the failure"
    );
    stop().await.expect("the retry stops");
    assert_eq!(count(&harness.radio().calls(), "stop_scan"), 2);
    assert!(harness.with_caller(|caller| caller.scan.is_none()).await);
    assert_eq!(harness.central.active_scan_id(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_unsubscribe_keeps_the_consumer_for_a_real_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let handle = text(&subscription, "handle");
    harness
        .radio()
        .fail_next(FaultOp::Unsubscribe, "disable refused");
    let unsubscribe = || {
        harness.execute(
            "gatt.unsubscribe",
            vec![("subscriptionHandle", string(handle.clone()))],
            None,
            OpControl::unbounded(),
        )
    };
    unsubscribe()
        .await
        .expect_err("the first disable fails truthfully");
    assert!(
        harness
            .with_caller(|caller| caller
                .subscriptions
                .get(&handle)
                .is_some_and(|subscription| {
                    matches!(subscription.phase, ReleasePhase::ReleaseFailed)
                }))
            .await,
        "the consumer mapping survives the failure"
    );
    assert_eq!(
        harness.radio().live_subscription_count(),
        1,
        "CCCD still live"
    );
    unsubscribe().await.expect("the retry disables");
    assert_eq!(harness.radio().live_subscription_count(), 0);
    assert_eq!(
        count(&harness.radio().calls(), "set_notifications"),
        3,
        "one enable, two disable attempts"
    );
    assert!(
        harness
            .with_caller(|caller| caller.subscriptions.is_empty())
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_disconnect_keeps_the_link_for_a_real_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "release refused");
    let disconnect = || {
        harness.execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
    };
    disconnect()
        .await
        .expect_err("the first release fails truthfully");
    assert!(
        harness
            .with_caller(|caller| caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| {
                    matches!(connection.phase, ReleasePhase::ReleaseFailed)
                        && connection.native_release.is_none()
                }))
            .await,
        "the link mapping survives the failure"
    );
    disconnect().await.expect("the retry releases");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(!harness.radio().link_connected("peer-a"));
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_09_a_failed_destroy_keeps_the_caller_for_a_real_retry() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "release refused");
    let first = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(first["state"], "release-failed");
    assert_eq!(first["failures"][0]["resourceKind"], "connection");
    assert_eq!(
        first["failures"][0]["error"]["retryability"], "never",
        "the wire vocabulary a TypeScript transport accepts"
    );
    assert!(
        harness
            .with_caller(|caller| caller.retired && caller.connections.contains_key(&link.handle))
            .await,
        "the retired caller keeps the link it could not release"
    );
    let second = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(second["state"], "released");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(harness.dispatcher.inner.lock().await.callers.is_empty());
}

// PR210-10 — a rejected disconnect changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_10_a_wrong_generation_disconnect_changes_nothing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    let shape = |caller: &CallerState| {
        (
            caller.connections.len(),
            caller.databases.len(),
            caller.subscriptions.len(),
            caller.connection_events.len(),
            caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| connection.phase.is_active()),
        )
    };
    let before = harness.with_caller(shape).await;
    let mut wrong = Harness::link_entries(&link);
    wrong.retain(|(key, _)| *key != "connectionGeneration");
    wrong.push(("connectionGeneration", string("generation-wrong")));
    let error = harness
        .execute("connection.disconnect", wrong, None, OpControl::unbounded())
        .await
        .expect_err("a wrong generation is refused");
    assert_eq!(error.code, BleErrorCode::ProtocolViolation);
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 0);
    assert_eq!(harness.with_caller(shape).await, before, "nothing changed");
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the valid disconnect succeeds");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 1);
}

// PR210-11 — typed core lifecycle events reach the connection-event
// stream by its own handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_idle_link_loss_reaches_the_stream_and_invalidates() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");

    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;

    // No operation follows: the OS alone reports the loss.
    harness
        .radio()
        .push_event(RadioEvent::Disconnected("peer-a".to_owned()));
    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[0]["value"]["current"], "connected");
    assert_eq!(items[1]["value"]["previous"], "connected");
    assert_eq!(items[1]["value"]["current"], "lost");
    assert_eq!(items[1]["value"]["cause"], "peer-link-loss");
    assert_eq!(
        items[1]["value"]["connectionGeneration"],
        link.generation.as_str()
    );
    assert_eq!(items[2]["kind"], "terminal");
    assert_eq!(items[2]["reason"], "connection-lost");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["kind"], "terminal");
    assert_eq!(ended[0]["reason"], "connection-lost");
    assert!(
        harness
            .with_caller(|caller| !caller.databases[&database.handle].valid)
            .await,
        "the database of the lost link is stale"
    );
    // The lost link releases without a radio call.
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("a lost link releases");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_requested_disconnect_reports_requested_disconnect() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("disconnect");
    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[1]["value"]["previous"], "disconnecting");
    assert_eq!(items[1]["value"]["current"], "disconnected");
    assert_eq!(items[1]["value"]["cause"], "requested-disconnect");
    assert_eq!(items[2]["reason"], "owner-released");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.items("connection-events-ipc-1").len(),
        3,
        "exactly once"
    );
}

// Finding 190a — after an app-requested `connection.disconnect`, a live
// notification subscription must end `owner-released` (the vocabulary's
// requested-disconnect word, as on RN iOS/Android/tvOS), never
// `stream.closed`: the supervisor then backs off and reconnects instead of
// stopping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_190a_a_requested_disconnect_ends_notifications_owner_released() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("disconnect");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["kind"], "terminal");
    assert_eq!(
        ended[0]["reason"], "owner-released",
        "a requested disconnect ends subscriptions owner-released on every host"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.items(&notifications).len(),
        1,
        "exactly one terminal"
    );
}

// The core can invalidate a notification poll before the requested native
// disconnect has answered. The in-flight poll must not win the transport
// terminal race against the owner-release path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_disconnect_pauses_an_in_flight_notification_terminal() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness.radio().block_op(FaultOp::Disconnect);
    let disconnect = harness.spawn_route(
        "connection.disconnect",
        "requested-disconnect-race",
        Harness::link_entries(&link),
        None,
    );
    harness.wait_calls("disconnect", 1).await;
    assert!(
        harness
            .with_caller(|caller| caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| matches!(connection.phase, ReleasePhase::Releasing(_))))
            .await
    );

    // Model the already-started core poll returning LinkEnded while the
    // disconnect owns the link. Its terminal send must be rejected atomically
    // with the release phase, not by a separate check before sending.
    let delivered = harness
        .dispatcher
        .notification_terminal(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &notifications,
            "connection-lost",
            None,
        )
        .await
        .expect("terminal admission check");
    assert!(!delivered);
    assert!(
        harness.items(&notifications).is_empty(),
        "the pending owner release must reserve the notification terminal"
    );

    harness.radio().unblock_op(FaultOp::Disconnect);
    disconnect
        .await
        .expect("disconnect task")
        .expect("disconnect");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["reason"], "owner-released");
    assert_eq!(harness.items(&notifications).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notification_terminal_sent_before_disconnect_is_not_sent_twice() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");

    assert!(harness
        .dispatcher
        .notification_terminal(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &notifications,
            "connection-lost",
            None,
        )
        .await
        .expect("first terminal is sent"));
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("disconnect");
    assert_eq!(
        harness.items(&notifications).len(),
        1,
        "a terminal physically sent before owner release is not duplicated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_release_emits_owner_release_terminal_before_retirement_and_rebootstraps() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");

    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert!(super::is_released(&cleanup), "{cleanup:?}");
    let terminal = harness.wait_items(&notifications, 1).await;
    assert_eq!(terminal[0]["kind"], "terminal");
    assert_eq!(terminal[0]["reason"], "owner-released");
    assert_eq!(harness.items(&notifications).len(), 1, "one terminal only");

    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), sink, version_offer())
        .await
        .expect("release leaves no stale caller state that blocks rebootstrap");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
/// Deterministic fresh-dispatcher/SQLite-reopen evidence; this does not spawn
/// a second OS process and therefore is not physical crash-recovery proof.
async fn fresh_dispatcher_reopens_terminal_evidence_releases_failed_reservations_and_preserves_quota(
) {
    let harness = Harness::new().await;
    let directory = std::env::temp_dir().join(format!(
        "ubm-tauri-terminal-rebootstrap-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir(&directory).expect("fixture directory");
    let ledger = Arc::new(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            1024,
        )
        .expect("ledger"),
    );
    *harness
        .dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    {
        let mut state = harness.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .versions = object([(
            "ipcProtocol",
            object([("selected", object([("value", number(11))]))]),
        )]);
    }
    let scope = object([
        ("managerId", string("manager")),
        ("operationId", string("subscription")),
        ("incarnation", string("incarnation")),
    ]);
    let reservation = harness
        .dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.reserve",
            &BTreeMap::from([
                ("version".to_owned(), number(1)),
                ("producer".to_owned(), string("subscription")),
                ("maximumBytes".to_owned(), number(512)),
                ("formatVersion".to_owned(), number(1)),
                ("scope".to_owned(), scope.clone()),
            ]),
            None,
        )
        .await
        .expect("reserve terminal evidence");
    let _reservation_id = text(&reservation, "reservationId");

    let failed_scope = object([
        ("managerId", string("manager")),
        ("operationId", string("failed-commit")),
        ("incarnation", string("incarnation")),
    ]);
    let failed_reservation = harness
        .dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.reserve",
            &BTreeMap::from([
                ("version".to_owned(), number(1)),
                ("producer".to_owned(), string("subscription")),
                ("maximumBytes".to_owned(), number(512)),
                ("formatVersion".to_owned(), number(1)),
                ("scope".to_owned(), failed_scope),
            ]),
            None,
        )
        .await
        .expect("reserve failed-commit evidence");
    let failed_reservation_id = text(&failed_reservation, "reservationId");
    assert!(
        harness
            .dispatcher
            .terminal_evidence_route(
                &harness.caller,
                "terminalEvidence.commit",
                &BTreeMap::from([
                    ("version".to_owned(), number(1)),
                    ("reservationId".to_owned(), string(failed_reservation_id)),
                    ("digest".to_owned(), string("00".repeat(32))),
                ]),
                Some(vec![1]),
            )
            .await
            .is_err(),
        "a bad digest must not commit"
    );

    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("terminalEvidenceScope", scope));
    let subscription = harness
        .execute("gatt.subscribe", entries, None, OpControl::unbounded())
        .await
        .expect("subscribe with reserved terminal evidence");
    let notifications = text(&subscription, "handle");

    assert!(
        harness
            .dispatcher
            .terminal_evidence_route(
                &harness.caller,
                "terminalEvidence.recovery.list",
                &BTreeMap::from([("version".to_owned(), number(1))]),
                None
            )
            .await
            .expect("live owner may list")
            .into_wire()["records"]
            .as_array()
            .expect("records")
            .is_empty(),
        "a live terminal receipt is not recovery-visible"
    );
    assert!(harness
        .dispatcher
        .notification_terminal_with_counters(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &notifications,
            "overflow",
            None,
            super::terminal_evidence::TerminalCounters::new(1, 23, 0),
        )
        .await
        .expect("terminal delivery"));
    let overflow = harness.wait_items(&notifications, 1).await;
    assert_eq!(overflow[0]["droppedItems"], 1);
    assert_eq!(overflow[0]["droppedBytes"], 23);
    assert_eq!(overflow[0]["replacedItems"], 0);
    let overflow_reference = overflow[0]["evidence"]["ref"]
        .as_str()
        .expect("receipt ref");
    let (overflow_bytes, _) = ledger
        .read(overflow_reference, 1024)
        .expect("receipt readable");
    let overflow_canonical = String::from_utf8(overflow_bytes).expect("canonical receipt");
    assert!(overflow_canonical.contains("\"droppedItems\":1"));
    assert!(overflow_canonical.contains("\"droppedBytes\":23"));

    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert!(super::is_released(&cleanup), "{cleanup:?}");
    harness
        .dispatcher
        .bootstrap(
            harness.caller.clone(),
            IpcEventSink::new(Channel::new(|_| Ok(()))),
            version_offer(),
        )
        .await
        .expect("same dispatcher rebootstrap releases runtime evidence authority");
    let same_dispatcher_recovery = harness
        .dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.list",
            &BTreeMap::from([("version".to_owned(), number(1))]),
            None,
        )
        .await
        .expect("same dispatcher lists committed evidence after release");
    assert_eq!(
        same_dispatcher_recovery.into_wire()["records"]
            .as_array()
            .expect("same dispatcher records")
            .len(),
        1,
        "runtime teardown retains the committed bytes but does not retain its lease"
    );
    let competing_dispatcher = BtleplugDispatcher::with_core_authority(Arc::new(
        harness.central.clone(),
    )
        as Arc<dyn CoreAuthority>);
    *competing_dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    competing_dispatcher
        .bootstrap(
            harness.caller.clone(),
            IpcEventSink::new(Channel::new(|_| Ok(()))),
            version_offer(),
        )
        .await
        .expect("a busy row must not hide independent recovery availability");
    let legacy_busy = competing_dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.list",
            &BTreeMap::from([("version".to_owned(), number(1))]),
            None,
        )
        .await
        .expect_err("legacy list must not silently return only cold rows");
    assert_eq!(legacy_busy.code, BleErrorCode::PlatformFailure);
    assert_eq!(legacy_busy.retryability, Retryability::CallerDecides);
    assert_eq!(
        legacy_busy.operation, "tauri.terminal-evidence.recovery.list",
        "legacy callers retain the pre-status retry signal"
    );
    let busy_status = competing_dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.listWithStatus",
            &BTreeMap::from([("version".to_owned(), number(1))]),
            None,
        )
        .await
        .expect("status list reports contention without failing");
    let busy_wire = busy_status.into_wire();
    assert_eq!(
        busy_wire["records"].as_array().expect("busy records").len(),
        0,
        "the winning controller alone exposes its claimed opaque alias"
    );
    assert_eq!(
        busy_wire["busyProducers"],
        serde_json::json!(["subscription"]),
        "contention names only the producer that must be retried"
    );
    let release_recovery_controller = harness.dispatcher.release(&harness.key()).await;
    assert!(
        super::is_released(&release_recovery_controller),
        "releasing the winning controller must relinquish its private recovery lease: {release_recovery_controller:?}"
    );
    let recovered_dispatcher = competing_dispatcher;
    let terminal = harness.wait_items(&notifications, 1).await;
    let reference = terminal[0]["evidence"]["ref"]
        .as_str()
        .expect("committed evidence reference");
    let (raw, _) = ledger
        .read(reference, 1024)
        .expect("committed raw evidence remains readable");
    let canonical = String::from_utf8(raw).expect("canonical JSON");
    assert!(canonical.contains("\"reason\":\"overflow\""));

    let recovery = recovered_dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.listWithStatus",
            &BTreeMap::from([("version".to_owned(), number(1))]),
            None,
        )
        .await
        .expect("a status refresh claims evidence once its former controller releases");
    let recovery_wire = recovery.into_wire();
    assert!(
        recovery_wire["busyProducers"]
            .as_array()
            .expect("released busy producers")
            .is_empty(),
        "contention clears on the next status refresh"
    );
    let records = recovery_wire["records"]
        .as_array()
        .cloned()
        .expect("records");
    assert_eq!(records.len(), 1);
    let handle = records[0]["handle"].as_str().expect("handle").to_owned();
    let recovered = recovered_dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.read",
            &BTreeMap::from([
                ("version".to_owned(), number(1)),
                ("handle".to_owned(), string(handle.clone())),
                ("maximumBytes".to_owned(), number(1024)),
            ]),
            None,
        )
        .await
        .expect("fresh lease reads receipt");
    assert_eq!(recovered.into_wire()["kind"], "committed");
    let quota_remains_charged = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "subscription".to_owned(),
        manager_id: "manager".to_owned(),
        operation_id: "must-wait-for-recovery-ack".to_owned(),
        incarnation: "incarnation".to_owned(),
        scope_utf16le: None,
    };
    assert!(
        matches!(
            // A committed receipt is charged at its exact canonical byte
            // length, not at the original reservation ceiling.  Request the
            // whole configured quota so this assertion proves that *any*
            // unacknowledged committed receipt survives stream drain and
            // manager teardown without depending on incidental JSON size.
            ledger.reserve(&quota_remains_charged, 1024),
            Err(ubm_desktop::terminal_evidence_ledger::TerminalEvidenceError::Quota)
        ),
        "draining the live subscription and releasing its runtime must not reclaim committed evidence capacity"
    );
    let old_lease_error = recovered_dispatcher
        .execute(
            &harness.caller,
            "terminalEvidence.recovery.read",
            BTreeMap::from([
                ("version".to_owned(), number(1)),
                ("handle".to_owned(), string(handle.clone())),
                ("maximumBytes".to_owned(), number(1024)),
                ("__expectedLeaseId".to_owned(), string(LEASE_ID)),
                (
                    "__expectedLeaseGeneration".to_owned(),
                    string(LEASE_GENERATION),
                ),
            ]),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("old lease is denied");
    assert_eq!(old_lease_error.code, BleErrorCode::OwnershipDenied);
    let other = Harness::over_dispatcher(
        harness.central.clone(),
        recovered_dispatcher.clone(),
        AuthenticatedCaller::new("test-app".to_owned(), "other-window".to_owned()),
    )
    .await;
    {
        let mut state = other.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&other.key())
            .expect("other caller")
            .versions = object([(
            "ipcProtocol",
            object([("selected", object([("value", number(11))]))]),
        )]);
    }
    assert_eq!(
        other
            .dispatcher
            .terminal_evidence_route(
                &other.caller,
                "terminalEvidence.recovery.read",
                &BTreeMap::from([
                    ("version".to_owned(), number(1)),
                    ("handle".to_owned(), string(handle.clone())),
                    ("maximumBytes".to_owned(), number(1024))
                ]),
                None
            )
            .await
            .expect_err("other caller denied")
            .code,
        BleErrorCode::OwnershipDenied
    );
    recovered_dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.ack",
            &BTreeMap::from([
                ("version".to_owned(), number(1)),
                ("handle".to_owned(), string(handle)),
            ]),
            None,
        )
        .await
        .expect("ack");
    assert!(
        ledger.read(reference, 1024).is_err(),
        "ack releases committed receipt"
    );

    let post_rebootstrap = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "subscription".to_owned(),
        manager_id: "manager".to_owned(),
        operation_id: "post-rebootstrap".to_owned(),
        incarnation: "incarnation".to_owned(),
        scope_utf16le: None,
    };
    ledger
        .reserve(&post_rebootstrap, 512)
        .expect("failed uncommitted reservation was released and quota is reusable");
    drop(ledger);
    std::fs::remove_dir_all(directory).expect("remove fixture directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_evidence_recovery_refresh_and_release_share_the_caller_lease_fence() {
    let harness = Harness::new().await;
    let directory = std::env::temp_dir().join(format!(
        "ubm-tauri-terminal-refresh-release-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir(&directory).expect("fixture directory");
    let ledger = Arc::new(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            1024,
        )
        .expect("ledger"),
    );
    *harness
        .dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "subscription".to_owned(),
        manager_id: "manager".to_owned(),
        operation_id: "refresh-release".to_owned(),
        incarnation: "incarnation".to_owned(),
        scope_utf16le: None,
    };
    let reservation = ledger.reserve(&scope, 64).expect("reserve");
    let bytes = b"terminal";
    ledger
        .stage(&reservation.reservation_id, &harness.key(), bytes)
        .expect("stage");
    use sha2::Digest;
    ledger
        .commit(
            &reservation.reservation_id,
            bytes,
            sha2::Sha256::digest(bytes).into(),
        )
        .expect("commit");
    ledger
        .abandon(&reservation.reservation_id)
        .expect("release source authority");

    let (claimed_sender, claimed_receiver) = std::sync::mpsc::sync_channel(1);
    let (resume_sender, resume_receiver) = std::sync::mpsc::sync_channel(1);
    *harness
        .dispatcher
        .terminal_evidence_recovery_pause
        .lock()
        .expect("terminal evidence recovery pause mutex poisoned") =
        Some(Arc::new(super::TerminalEvidenceRecoveryPause {
            claimed: claimed_sender,
            resume: std::sync::Mutex::new(resume_receiver),
        }));
    let refreshing_dispatcher = harness.dispatcher.clone();
    let refreshing_caller = harness.caller.clone();
    let refresh = tokio::spawn(async move {
        refreshing_dispatcher
            .refresh_terminal_evidence_recovery(&refreshing_caller)
            .await
    });
    claimed_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("refresh claimed durable evidence before owner insertion");
    let releasing_dispatcher = harness.dispatcher.clone();
    let releasing_key = harness.key();
    let mut release =
        tokio::spawn(async move { releasing_dispatcher.release(&releasing_key).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut release)
            .await
            .is_err(),
        "release must wait for the refresh lease fence after claim"
    );
    resume_sender.send(()).expect("resume refresh");
    refresh
        .await
        .expect("refresh task")
        .expect("refresh succeeds before release");
    let release_result = release.await.expect("release task");
    assert!(
        super::is_released(&release_result),
        "teardown observes and releases the alias inserted by refresh: {release_result:?}"
    );
    *harness
        .dispatcher
        .terminal_evidence_recovery_pause
        .lock()
        .expect("terminal evidence recovery pause mutex poisoned") = None;
    harness
        .dispatcher
        .bootstrap(
            harness.caller.clone(),
            IpcEventSink::new(Channel::new(|_| Ok(()))),
            version_offer(),
        )
        .await
        .expect("fresh lease bootstraps");
    let recovered = harness
        .dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.list",
            &BTreeMap::from([("version".to_owned(), number(1))]),
            None,
        )
        .await
        .expect("fresh lease recovers after release");
    assert_eq!(
        recovered.into_wire()["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
    drop(ledger);
    std::fs::remove_dir_all(directory).expect("remove fixture directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_evidence_recovery_refresh_refuses_a_lease_retired_before_claim() {
    let harness = Harness::new().await;
    let directory = std::env::temp_dir().join(format!(
        "ubm-tauri-terminal-refresh-retired-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir(&directory).expect("fixture directory");
    let ledger = Arc::new(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            1024,
        )
        .expect("ledger"),
    );
    *harness
        .dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "subscription".to_owned(),
        manager_id: "manager".to_owned(),
        operation_id: "retired-before-claim".to_owned(),
        incarnation: "incarnation".to_owned(),
        scope_utf16le: None,
    };
    let reservation = ledger.reserve(&scope, 64).expect("reserve");
    let bytes = b"terminal";
    ledger
        .stage(&reservation.reservation_id, &harness.key(), bytes)
        .expect("stage");
    use sha2::Digest;
    ledger
        .commit(
            &reservation.reservation_id,
            bytes,
            sha2::Sha256::digest(bytes).into(),
        )
        .expect("commit");
    ledger
        .abandon(&reservation.reservation_id)
        .expect("release source authority");
    assert!(super::is_released(
        &harness.dispatcher.release(&harness.key()).await
    ));
    let denied = harness
        .dispatcher
        .refresh_terminal_evidence_recovery(&harness.caller)
        .await
        .expect_err("retired lease cannot claim recovery evidence");
    assert_eq!(denied.code, BleErrorCode::OwnershipDenied);
    harness
        .dispatcher
        .bootstrap(
            harness.caller.clone(),
            IpcEventSink::new(Channel::new(|_| Ok(()))),
            version_offer(),
        )
        .await
        .expect("fresh lease bootstraps");
    let recovered = harness
        .dispatcher
        .terminal_evidence_route(
            &harness.caller,
            "terminalEvidence.recovery.list",
            &BTreeMap::from([("version".to_owned(), number(1))]),
            None,
        )
        .await
        .expect("fresh lease recovers retained evidence");
    assert_eq!(
        recovered.into_wire()["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
    drop(ledger);
    std::fs::remove_dir_all(directory).expect("remove fixture directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_owner_terminal_send_keeps_the_disconnect_retryable() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    let attempts = Arc::new(AtomicUsize::new(0));
    let send_attempts = Arc::clone(&attempts);
    let events = Arc::clone(&harness.events);
    let sink = IpcEventSink::new(Channel::new(move |body| {
        if send_attempts.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            return Err(tauri::Error::Io(std::io::Error::other(
                "scripted terminal send refusal",
            )));
        }
        if let InvokeResponseBody::Json(json) = body {
            events
                .lock()
                .expect("event log")
                .push(serde_json::from_str(&json).expect("event json"));
        }
        Ok(())
    }));
    {
        let mut state = harness.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .event_sink = sink;
    }

    let first = harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("a refused terminal send cannot report complete cleanup");
    assert_eq!(first.code, BleErrorCode::PlatformTransport);
    assert!(
        harness
            .with_caller(|caller| caller.connections.contains_key(&link.handle)
                && caller.subscriptions.contains_key(&notifications))
            .await
    );
    assert!(harness.items(&notifications).is_empty());
    assert_eq!(
        harness
            .with_caller(|caller| caller.connections[&link.handle].native_release)
            .await,
        Some(true)
    );

    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("retry delivers the terminal and releases the mapping");
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["reason"], "owner-released");
    assert_eq!(ended.len(), 1);
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_stale_generation_matches_nothing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;
    harness
        .dispatcher
        .apply_lifecycle_event(&LifecycleEvent {
            sequence: 99,
            peer_id: "peer-a".to_owned(),
            peer_key: "stale".to_owned(),
            connection_generation: Some("generation-from-an-older-link".to_owned()),
            database_generation: None,
            kind: LifecycleKind::LinkLost,
            platform: None,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        harness.items("connection-events-ipc-1").len(),
        1,
        "only the initial event"
    );
    assert!(
        harness
            .with_caller(
                |caller| caller.connection_events["connection-events-ipc-1"].end == StreamEnd::Open
            )
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_reset_reports_backend_restart_with_observation_cause() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness.connection_events(&link, "host-reset-events").await;
    harness.ready("host-reset-events").await;
    let generation = harness
        .with_caller(|caller| caller.connections[&link.handle].core_generation.clone())
        .await;
    harness
        .dispatcher
        .apply_lifecycle_event(&LifecycleEvent {
            sequence: 99,
            peer_id: "peer-a".into(),
            peer_key: "peer-a".into(),
            connection_generation: Some(generation),
            database_generation: None,
            kind: LifecycleKind::HostReset,
            platform: Some(PlatformDetail::new("ubm", "source-observation-overflow")),
        })
        .await;
    let items = harness.wait_items("host-reset-events", 3).await;
    assert_eq!(items[1]["value"]["cause"], "backend-restart");
    assert_eq!(
        items[1]["value"]["platform"]["code"],
        "source-observation-overflow"
    );
    assert_eq!(items[2]["reason"], "source-failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_platform_fact_survives_active_and_early_stream_delivery() {
    for ready_first in [true, false] {
        let harness = Harness::new().await;
        let link = harness.connect("peer-a").await;
        harness.connection_events(&link, "platform-events").await;
        if ready_first {
            harness.ready("platform-events").await;
        }
        let generation = harness
            .with_caller(|caller| caller.connections[&link.handle].core_generation.clone())
            .await;
        harness
            .dispatcher
            .apply_lifecycle_event(&LifecycleEvent {
                sequence: 99,
                peer_id: "peer-a".into(),
                peer_key: "peer-a".into(),
                connection_generation: Some(generation),
                database_generation: None,
                kind: LifecycleKind::LinkLost,
                platform: Some(
                    PlatformDetail::new("bluez-mgmt", "8")
                        .with_metadata("disconnectReason", PlatformValue::Int(8))
                        .with_metadata("wide", PlatformValue::Int(i64::MAX)),
                ),
            })
            .await;
        if !ready_first {
            harness.ready("platform-events").await;
        }
        let items = harness.wait_items("platform-events", 3).await;
        assert_eq!(items[1]["value"]["cause"], "peer-link-loss");
        assert_eq!(
            items[1]["value"]["connectionGeneration"],
            link.generation.as_str()
        );
        assert_eq!(items[1]["value"]["platform"]["domain"], "bluez-mgmt");
        assert_eq!(
            items[1]["value"]["platform"]["metadata"]["disconnectReason"],
            8
        );
        assert_eq!(
            items[1]["value"]["platform"]["metadata"]["wide"],
            i64::MAX.to_string()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_lagged_pump_ends_every_stream_with_overflow() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;
    harness.dispatcher.overflow_connection_event_streams().await;
    let items = harness.wait_items("connection-events-ipc-1", 2).await;
    assert_eq!(items[1]["kind"], "terminal");
    assert_eq!(items[1]["reason"], "overflow");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notification_source_failure_preserves_native_error_after_buffered_value() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .unwrap();
    let stream = text(&subscription, "handle");
    harness.notify("peer-a", NOTIFY_ONLY, vec![7]);
    let epoch = harness
        .radio()
        .enable_epochs()
        .iter()
        .rev()
        .find(|(key, _)| key.0 == "peer-a" && key.3 == NOTIFY_ONLY)
        .map(|(_, epoch)| *epoch)
        .unwrap();
    harness
        .radio()
        .push_event(RadioEvent::NotificationSourceFailed {
            peer_id: "peer-a".to_owned(),
            service_uuid: HRM_SERVICE.to_owned(),
            service_occurrence: 0,
            characteristic_uuid: NOTIFY_ONLY.to_owned(),
            characteristic_occurrence: 0,
            epoch,
            error: DesktopError::new(
                BleErrorCode::PlatformFailure,
                ubm_core::contracts::BleErrorDomain::Gatt,
                "gatt.notification",
            )
            .with_platform(PlatformDetail::new("test-radio", "radio-failed")),
        });
    let items = harness.wait_items(&stream, 2).await;
    assert_eq!(value_bytes(&items[0]), vec![7]);
    assert_eq!(items[1]["reason"], "source-failed");
    assert_eq!(items[1]["error"]["code"], "platform.failure");
    assert_eq!(items[1]["error"]["operation"], "gatt.notification");
    assert_eq!(items[1]["error"]["platform"]["code"], "radio-failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_service_change_invalidates_and_ends_notifications() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .radio()
        .push_event(RadioEvent::ServicesChanged("peer-a".to_owned()));
    let ended = harness.wait_items(&notifications, 1).await;
    assert_eq!(ended[0]["reason"], "service-changed");
    let deadline = tokio::time::Instant::now() + WAIT;
    while harness
        .with_caller(|caller| caller.databases[&database.handle].valid)
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "database never invalidated"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_11_a_loss_before_ready_is_delivered_at_ready() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness
        .radio()
        .push_event(RadioEvent::Disconnected("peer-a".to_owned()));
    let deadline = tokio::time::Instant::now() + WAIT;
    while harness
        .with_caller(|caller| {
            caller.connection_events["connection-events-ipc-1"].end == StreamEnd::Open
        })
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "loss never recorded"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    harness.ready("connection-events-ipc-1").await;
    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[0]["value"]["current"], "connected");
    assert_eq!(items[1]["value"]["current"], "lost");
    assert_eq!(items[2]["reason"], "connection-lost");
}

// PR210-13T / FIX-PLAN decision 3 — the 4.x property check, then the
// requirement travels to the core.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_13t_a_requirement_is_refused_only_where_the_property_is_missing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let enables = || count(&harness.radio().calls(), "set_notifications");

    let error = harness
        .subscribe(
            &link,
            &database,
            INDICATE_ONLY,
            Some("require-notification"),
        )
        .await
        .expect_err("an indicate-only characteristic cannot notify");
    assert_eq!(
        error.identity(),
        (
            "gatt.property-not-supported",
            "gatt",
            "tauri.subscribe.notification".to_owned()
        )
    );
    let error = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("require-indication"))
        .await
        .expect_err("a notify-only characteristic cannot indicate");
    assert_eq!(error.identity().2, "tauri.subscribe.indication");
    let error = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("require-telepathy"))
        .await
        .expect_err("an unknown mode is refused");
    assert_eq!(error.code, BleErrorCode::ArgumentInvalid);
    assert_eq!(enables(), 0, "no refusal reached the radio");

    // The 4.x-accepted case: require-notification on a notify-capable
    // characteristic succeeds end to end and reports what the radio saw.
    harness
        .radio()
        .script_observed_delivery(ObservedDelivery::Notification);
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("require-notification"))
        .await
        .expect("a notify-capable characteristic accepts require-notification");
    assert_eq!(
        field(&subscription, "observedDelivery"),
        &string("notification")
    );
    assert_eq!(
        harness.radio().delivery_requests(),
        vec![Some(DeliveryMode::Notification)],
        "the requirement travels to the core and the radio"
    );
    let stream = text(&subscription, "handle");
    harness.notify("peer-a", NOTIFY_ONLY, vec![1, 2]);
    let items = harness.wait_items(&stream, 1).await;
    assert_eq!(items[0]["value"]["delivery"], "notification");
}

/// A hard indication requirement is carried through. The scripted radio
/// records that request; it does not apply the platform CCCD rule. The
/// real backend refuses it on macOS and Linux in `plan_delivery` before
/// any effect when the characteristic also notifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_13t_a_dual_property_indication_requirement_is_not_dropped() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let mut service = hrm_service();
    service.characteristics.push(characteristic(
        "00002a5d-0000-1000-8000-00805f9b34fb",
        flags(false, false, true, true),
    ));
    harness.radio().set_services(&link.peer_id, vec![service]);
    let database = harness.discover(&link).await;
    harness
        .subscribe(
            &link,
            &database,
            "00002a5d-0000-1000-8000-00805f9b34fb",
            Some("require-indication"),
        )
        .await
        .expect("the scripted radio accepts the carried requirement");
    assert_eq!(
        harness.radio().delivery_requests(),
        vec![Some(DeliveryMode::Indication)],
        "require-indication is not replaced with the platform default"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_13t_a_preference_reports_unknown_delivery_when_the_radio_says_nothing() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, Some("prefer-indication"))
        .await
        .expect("a preference rides through");
    assert_eq!(field(&subscription, "observedDelivery"), &string("unknown"));
    assert_eq!(harness.radio().delivery_requests(), vec![None]);
    assert_eq!(
        harness.radio().delivery_preferences(),
        vec![Some(DeliveryMode::Indication)]
    );
}

// PR210-22 — retryability is the core's answer, never the code's.
#[test]
fn pr210_22_a_dispatcher_error_is_never_retryable_by_its_code() {
    for code in [
        BleErrorCode::OperationAborted,
        BleErrorCode::OperationTimedOut,
    ] {
        let error = DispatchError::new(code, "ipc", "tauri.test");
        assert_eq!(error.retryability, Retryability::Never);
        let wire = error.normalized_error().into_wire();
        assert_eq!(wire["retryability"], "never");
        assert_eq!(wire["commit"], Value::Null);
    }
}

// PR210-32 — racing first calls open exactly one radio.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pr210_32_racing_first_calls_open_exactly_one_authority() {
    let opens = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let opener: AuthorityOpener = {
        let opens = Arc::clone(&opens);
        let gate = Arc::clone(&gate);
        Arc::new(move || {
            let opens = Arc::clone(&opens);
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                opens.fetch_add(1, AtomicOrdering::SeqCst);
                gate.notified().await;
                let central = ubm_desktop::executor::desktop_runtime()
                    .spawn(DesktopCentral::open(FakeRadio::new(), "tauri-race"))
                    .await
                    .expect("open joins")
                    .map_err(|error| DispatchError::from_core(&error))?;
                let authority: Arc<dyn CoreAuthority> = Arc::new(central);
                Ok(authority)
            })
        })
    };
    let dispatcher = BtleplugDispatcher::with_authority_opener(opener);
    let first = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.ensure_authority().await }
    });
    let second = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.ensure_authority().await }
    });
    while opens.load(AtomicOrdering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_waiters();
    let first = first
        .await
        .expect("joins")
        .map_err(|error| error.describe());
    let second = second
        .await
        .expect("joins")
        .map_err(|error| error.describe());
    let (first, second) = (first.expect("first opens"), second.expect("second shares"));
    assert_eq!(opens.load(AtomicOrdering::SeqCst), 1, "exactly one radio");
    assert!(Arc::ptr_eq(&first, &second), "one shared central");
}

// The effective ATT MTU the OS observed crosses the dispatcher. An
// unobserved link is null, not a synthesized length and not a link failure.
// Windows reads `GattSession.MaxPduSize`. Linux reads the BlueZ
// characteristic MTU. A scripted measurement still crosses as a number.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_effective_mtu_reads_the_live_link_through_the_core() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let unobserved = harness
        .execute(
            "connection.effective-mtu",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("an unobserved MTU is null");
    assert_eq!(field(&unobserved, "mtu"), &IpcValue::Null);
    harness.radio().set_effective_mtu("peer-a", 515);
    let mtu = harness
        .execute(
            "connection.effective-mtu",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the OS measurement crosses");
    assert_eq!(field(&mtu, "mtu"), &number(515));
}

// PARITY §4 — connected RSSI works again (4.x read it through btleplug).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_rssi_reads_the_live_link_through_the_core() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let error = harness
        .execute(
            "connection.rssi",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("an unmeasured RSSI is not synthesized");
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    harness.radio().set_rssi("peer-a", -55);
    let rssi = harness
        .execute(
            "connection.rssi",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the OS measurement crosses");
    assert_eq!(field(&rssi, "rssi"), &number(-55));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_control_watches_do_not_consume_live_watch_capacity() {
    async fn acknowledge_initial(harness: &Harness, handle: &str) {
        harness.wait_items(handle, 1).await;
        let event_id = harness
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|event| event["streamId"] == handle)
            .unwrap()["eventId"]
            .as_str()
            .unwrap()
            .to_owned();
        let request = BTreeMap::from([
            ("eventId".to_owned(), string(event_id)),
            (
                "rendererLease".to_owned(),
                object([
                    ("leaseId", string(LEASE_ID)),
                    ("generation", string(LEASE_GENERATION)),
                ]),
            ),
        ]);
        harness
            .dispatcher
            .acknowledge(harness.caller.clone(), request)
            .await
            .expect("event ACK");
    }
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    harness.radio().set_write_readiness("peer-a", true);
    harness.radio().set_connection_parameters(
        "peer-a",
        ubm_desktop::boundary::ObservedConnectionParameters {
            interval_us: 30_000,
            latency: 0,
            supervision_timeout_us: 4_000_000,
        },
    );
    for (subscribe, unsubscribe, handle_key, prefix) in [
        (
            "connection.write-readiness.subscribe",
            "connection.write-readiness.unsubscribe",
            "writeReadinessHandle",
            "ready",
        ),
        (
            "connection.parameters.subscribe",
            "connection.parameters.unsubscribe",
            "parameterEventsHandle",
            "parameters",
        ),
    ] {
        for ordinal in 0..600 {
            let handle = format!("{prefix}-{ordinal}");
            let mut entries = Harness::link_entries(&link);
            entries.push((handle_key, string(&handle)));
            harness
                .execute(subscribe, entries, None, OpControl::unbounded())
                .await
                .expect("live capacity is reusable");
            // Acknowledge the actual event delivered on this renderer channel.
            acknowledge_initial(&harness, &handle).await;
            for _ in 0..2 {
                harness
                    .execute(
                        unsubscribe,
                        vec![(handle_key, string(&handle))],
                        None,
                        OpControl::unbounded(),
                    )
                    .await
                    .expect("recent release replay is idempotent");
            }
            let state = harness.dispatcher.inner.lock().await;
            let owner = state.callers.get(&harness.key()).unwrap();
            assert!(owner.write_readiness_watches.is_empty() && owner.parameter_watches.is_empty());
            assert!(
                owner.write_readiness_releases.len() <= 256
                    && owner.parameter_releases.len() <= 256
            );
        }
        // Repeated closes must not weaken the actual concurrent-resource bound.
        for ordinal in 0..256 {
            let handle = format!("{prefix}-active-{ordinal}");
            let mut entries = Harness::link_entries(&link);
            entries.push((handle_key, string(&handle)));
            harness
                .execute(subscribe, entries, None, OpControl::unbounded())
                .await
                .expect("live watch");
            acknowledge_initial(&harness, &handle).await;
        }
        let mut overflow = Harness::link_entries(&link);
        overflow.push((handle_key, string(format!("{prefix}-overflow"))));
        let error = harness
            .execute(subscribe, overflow, None, OpControl::unbounded())
            .await
            .unwrap_err();
        assert_eq!(error.code, BleErrorCode::StreamQuota);
        for ordinal in 0..256 {
            harness
                .execute(
                    unsubscribe,
                    vec![(handle_key, string(format!("{prefix}-active-{ordinal}")))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .expect("release live watch");
        }
        let old = harness
            .execute(
                unsubscribe,
                vec![(handle_key, string(format!("{prefix}-0")))],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap_err();
        assert_eq!(old.code, BleErrorCode::OwnershipDenied);
    }
}

/// U01: capability truth is checked at the actual bootstrap boundary, beside
/// routed operations and delivered service-change events, not serialization only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_capability_bootstrap_agrees_with_routed_rssi_and_service_change() {
    fn mac(core: &mut ubm_core::central::Central) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::MacOs),
            false,
        )
    }
    fn windows(
        core: &mut ubm_core::central::Central,
    ) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::Windows),
            false,
        )
    }
    fn linux(core: &mut ubm_core::central::Central) -> Result<(), ubm_core::contracts::CoreError> {
        ubm_desktop::register_desktop_capabilities_for(
            core,
            Some(ubm_desktop::DesktopOs::Linux),
            false,
        )
    }
    for (os, registration) in [
        (
            ubm_desktop::DesktopOs::MacOs,
            mac as fn(
                &mut ubm_core::central::Central,
            ) -> Result<(), ubm_core::contracts::CoreError>,
        ),
        (ubm_desktop::DesktopOs::Windows, windows),
        (ubm_desktop::DesktopOs::Linux, linux),
    ] {
        let mut profile = ubm_desktop::CentralProfile::desktop("native-capability-bootstrap");
        profile.register_capabilities = registration;
        let harness = Harness::over(
            DesktopCentral::open_with(FakeRadio::new(), profile)
                .await
                .unwrap(),
        )
        .await;
        let response = harness
            .dispatcher
            .bootstrap(
                AuthenticatedCaller::new("test-app".to_owned(), "capability-probe".to_owned()),
                IpcEventSink::new(Channel::new(|_| Ok(()))),
                version_offer(),
            )
            .await
            .unwrap()
            .into_wire();
        let descriptors = response["bootstrap"]["capabilities"]["descriptors"]
            .as_array()
            .unwrap();
        let native = harness.central.capability_descriptors().await;
        for id in [
            "connection:rssi",
            "gatt:service-changed",
            "background:desktop-maintain-connection",
        ] {
            let row = descriptors.iter().find(|row| row["id"] == id).unwrap();
            let native = native.iter().find(|row| row.id() == id).unwrap();
            assert_eq!(
                row["state"],
                native.state().as_str(),
                "{} {id}",
                os.as_str()
            );
            assert_eq!(row["evidence"]["receiptId"], native.receipt_id());
        }
        let link = harness.connect("peer-a").await;
        if os == ubm_desktop::DesktopOs::MacOs {
            harness.radio().set_rssi("peer-a", -55);
        }
        let rssi = harness
            .execute(
                "connection.rssi",
                Harness::link_entries(&link),
                None,
                OpControl::default(),
            )
            .await;
        if os == ubm_desktop::DesktopOs::MacOs {
            assert_eq!(field(&rssi.unwrap(), "rssi"), &number(-55));
        } else {
            assert_eq!(rssi.unwrap_err().code, BleErrorCode::CapabilityUnsupported);
        }
        let database = harness.discover(&link).await;
        let subscription = harness
            .subscribe(&link, &database, NOTIFY_ONLY, None)
            .await
            .unwrap();
        let notifications = text(&subscription, "handle");
        harness
            .radio()
            .push_event(RadioEvent::ServicesChanged("peer-a".to_owned()));
        let ended = harness.wait_items(&notifications, 1).await;
        assert_eq!(ended[0]["reason"], "service-changed", "{}", os.as_str());
        assert!(super::is_released(
            &harness.dispatcher.release(&harness.key()).await
        ));
    }
}

#[tokio::test]
async fn transport_only_restrictions_match_absent_routes_and_rebind_ownership() {
    let harness = Harness::new().await;
    for (id, command) in [
        ("discovery:advertisement-watch", "advertisement.watch"),
        ("gatt:reliable-write", "gatt.reliable-write"),
    ] {
        assert!(crate::capabilities::transport_restriction(id).is_some());
        let error = harness
            .execute(command, vec![], None, OpControl::default())
            .await
            .unwrap_err();
        assert_eq!(error.operation, "tauri.route-command");
    }
    assert!(crate::capabilities::transport_restriction("gatt:high-throughput-acquire").is_none());
    assert!(
        crate::capabilities::transport_restriction("gatt:write-without-response-readiness")
            .is_none()
    );
    let readiness = harness
        .execute(
            "connection.write-readiness.subscribe",
            vec![],
            None,
            OpControl::default(),
        )
        .await
        .unwrap_err();
    assert_ne!(readiness.operation, "tauri.route-command");
    assert!(crate::capabilities::transport_restriction("peer:origin-authorized").is_some());
    let error = harness
        .execute(
            "peers.authorized",
            vec![("query", object([]))],
            None,
            OpControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    assert!(crate::capabilities::transport_restriction("lifecycle:page-persistence").is_some());
    let _link = harness.connect("peer-a").await;
    harness
        .dispatcher
        .bootstrap(
            harness.caller.clone(),
            IpcEventSink::new(Channel::new(|_| Ok(()))),
            version_offer(),
        )
        .await
        .unwrap();
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

/// A route's attachment identity is compared field by field against the
/// record the renderer was given — without building (and so without
/// sampling platform authorization for) a new record on every route.
#[test]
fn a_route_attachment_matches_its_record_by_identity_alone() {
    let attachment = test_attachment();
    let IpcValue::Object(record) = attachment_record(&attachment) else {
        panic!("the attachment record is an object");
    };
    assert!(attachment_identity_matches(&record, &attachment));
    for (key, adapter_level) in [
        ("attachmentId", false),
        ("backendInstanceId", false),
        ("backendGeneration", false),
        ("adapterId", true),
        ("adapterGeneration", true),
    ] {
        let mut forged = record.clone();
        let target = if adapter_level {
            match forged.get_mut("adapter") {
                Some(IpcValue::Object(adapter)) => adapter,
                _ => panic!("adapter record"),
            }
        } else {
            &mut forged
        };
        target.insert(key.to_owned(), string("forged"));
        assert!(
            !attachment_identity_matches(&forged, &attachment),
            "a forged {key} is refused"
        );
    }
}

/// A compensation that hangs is bounded by the cleanup liveness backstop
/// and becomes debt instead of stalling the refused start (PR210-08).
#[tokio::test(start_paused = true)]
async fn pr210_08_a_hung_compensation_is_bounded_and_becomes_debt() {
    let harness = Harness::on_current_runtime().await;
    harness.radio().block_op(FaultOp::StartScan);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(vec![("query", empty_scan_query())]);
    let start = tokio::spawn(async move {
        dispatcher
            .start_scan(&caller, payload, OpControl::unbounded())
            .await
    });
    while count(&harness.radio().calls(), "start_scan") == 0 {
        tokio::task::yield_now().await;
    }
    harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .remove(&harness.key());
    harness.radio().block_op(FaultOp::StopScan);
    harness.radio().unblock_op(FaultOp::StartScan);
    let started = tokio::time::Instant::now();
    let error = start
        .await
        .expect("joins")
        .expect_err("nobody owns the scan");
    assert_eq!(error.code, BleErrorCode::OwnershipDenied);
    assert!(
        started.elapsed() <= ubm_desktop::LIVENESS_CLEANUP + Duration::from_secs(1),
        "the hung stop is bounded by the cleanup backstop"
    );
    assert_eq!(harness.dispatcher.inner.lock().await.orphan_debt.len(), 1);
    assert!(harness.central.active_scan_id().is_some(), "still owned");
    harness.radio().unblock_op(FaultOp::StopScan);
}

/// A release that hangs is bounded and keeps the link for a real retry
/// (PR210-09).
#[tokio::test(start_paused = true)]
async fn pr210_09_a_hung_disconnect_is_bounded_and_keeps_the_link() {
    let harness = Harness::on_current_runtime().await;
    let link = harness.connect("peer-a").await;
    harness.radio().block_op(FaultOp::Disconnect);
    let started = tokio::time::Instant::now();
    let error = harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("the hung release times out");
    assert_eq!(error.code, BleErrorCode::OperationTimedOut);
    assert_eq!(
        error.platform.as_deref(),
        Some(ubm_desktop::LIVENESS_BACKSTOP_DETAIL),
        "no caller budget: the liveness backstop decided, and says so"
    );
    assert!(started.elapsed() <= ubm_desktop::LIVENESS_CLEANUP + Duration::from_secs(1));
    assert!(
        harness
            .with_caller(|caller| caller
                .connections
                .get(&link.handle)
                .is_some_and(|connection| {
                    matches!(connection.phase, ReleasePhase::ReleaseFailed)
                }))
            .await
    );
    harness.radio().unblock_op(FaultOp::Disconnect);
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("the retry releases");
    assert_eq!(count(&harness.radio().calls(), "disconnect"), 2);
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
}

// ---------------------------------------------------------------------------
// PR #210 findings 43, 57, 58, 60 — the Tauri half.
// ---------------------------------------------------------------------------

/// A radio scripted as a desktop OS radio: `policy` gate, teardown on loss.
fn os_radio(policy: AdmissionPolicy) -> FakeRadio {
    let radio = FakeRadio::new();
    radio.set_os_policy(policy, true);
    radio
}

impl Harness {
    /// The terminal item of one stream, once delivered.
    async fn wait_terminal(&self, stream: &str) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(terminal) = self
                .items(stream)
                .into_iter()
                .find(|item| item["kind"] == "terminal")
            {
                return terminal;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stream {stream} never ended: {:?}",
                self.items(stream)
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Drop the adapter out from under live work and wait for the core's
    /// reset report.
    async fn lose_adapter(&self) -> AdapterResetEvent {
        let mut resets = self.central.adapter_reset_events();
        self.radio()
            .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOff));
        tokio::time::timeout(WAIT, resets.recv())
            .await
            .expect("the core reports the reset")
            .expect("reset event")
    }

    async fn adapter_state(&self) -> Value {
        let response = self
            .execute("adapter.state", Vec::new(), None, OpControl::unbounded())
            .await
            .expect("adapter.state answers");
        field(&response, "state").clone().into_wire()
    }
}

fn version_offer() -> BTreeMap<String, IpcValue> {
    let range = |axis: &str, value: i64| {
        let version = object([("axis", string(axis)), ("value", number(value))]);
        object([
            ("axis", string(axis)),
            ("minimum", version.clone()),
            ("maximum", version),
        ])
    };
    let IpcValue::Object(offer) = object([
        ("backendContract", range("backend-contract", 1)),
        ("capabilitySchema", range("capability-schema", 1)),
        ("eventSchema", range("event-schema", 1)),
        ("traceFormat", range("trace-format", 1)),
        (
            "ipcProtocol",
            range("ipc-protocol", super::IPC_PROTOCOL_VERSION),
        ),
    ]) else {
        panic!("the version offer is an object");
    };
    offer
}

// Finding 43 — the attachment identity is the shared central's own: no
// second btleplug Manager (a second CBCentralManager on macOS) is opened
// for it, and the adapter it names is the one the central runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_43_the_attachment_is_the_shared_central_s_own() {
    let harness = Harness::new().await;
    let core = harness.central.attachment();
    let attachment = &harness.attachment;
    assert_eq!(attachment.attachment_id, core.attachment_id().as_str());
    assert_eq!(
        attachment.backend_instance_id,
        core.backend_instance_id().as_str()
    );
    assert_eq!(
        attachment.backend_generation,
        core.backend_generation().as_str()
    );
    assert_eq!(attachment.adapter_id, core.adapter_id().as_str());
    assert_eq!(
        attachment.adapter_generation,
        core.adapter_generation().as_str()
    );
    let state = harness.adapter_state().await;
    assert_eq!(
        state["backendGeneration"],
        core.backend_generation().as_str()
    );
}

// Finding 57 — an adapter loss ends everything live on it in the legacy
// vocabulary: links `connected → lost` with cause `adapter-loss` (legacy
// `connection-state-changed` reason `adapter`), notification streams and
// the scan `source-failed`, databases stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_57_an_adapter_loss_ends_every_stream_in_the_legacy_vocabulary() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let scan_stream = text(&scan, "handle");
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .expect("subscribe");
    let notifications = text(&subscription, "handle");
    harness
        .connection_events(&link, "connection-events-ipc-1")
        .await;
    harness.ready("connection-events-ipc-1").await;

    let reset = harness.lose_adapter().await;
    assert_eq!(reset.released_links, vec!["peer-a".to_owned()]);

    let items = harness.wait_items("connection-events-ipc-1", 3).await;
    assert_eq!(items[1]["value"]["previous"], "connected");
    assert_eq!(items[1]["value"]["current"], "lost");
    assert_eq!(items[1]["value"]["cause"], "adapter-loss");
    assert_eq!(
        items[1]["value"]["attachmentId"],
        harness.attachment.attachment_id.as_str(),
        "the transition reports the attachment the link lived on"
    );
    assert_eq!(items[2]["kind"], "terminal");
    assert_eq!(items[2]["reason"], "connection-lost");

    let ended = harness.wait_terminal(&notifications).await;
    assert_eq!(ended["reason"], "source-failed");
    assert_eq!(ended["error"]["code"], "operation.reset");

    let scan_end = harness.wait_terminal(&scan_stream).await;
    assert_eq!(scan_end["reason"], "source-failed");
    assert!(
        scan_end["error"]["platform"]["safeMessage"]
            .as_str()
            .is_some_and(|detail| detail.contains("powered-off")),
        "the core's own words ride along: {scan_end:?}"
    );
    assert!(
        harness.with_caller(|caller| caller.scan.is_none()).await,
        "the core released the scan owner; nothing is left to stop"
    );
    assert!(
        harness
            .with_caller(|caller| !caller.databases[&database.handle].valid)
            .await,
        "the database of the lost link is stale"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_terminal_evidence_stage_failure_is_authenticated_and_keeps_the_scan_binding() {
    let harness = Harness::new().await;
    let directory = scan_recovery_test_directory("scan-terminal-stage-failure");
    std::fs::create_dir(&directory).expect("fixture directory");
    let ledger = Arc::new(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            1024,
        )
        .expect("ledger"),
    );
    let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "scan".into(),
        manager_id: "manager".into(),
        operation_id: "stage-failure".into(),
        incarnation: "incarnation".into(),
        scope_utf16le: None,
    };
    let reservation = ledger
        .reserve(&scope, 1)
        .expect("reserve tiny evidence budget");
    *harness
        .dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let operation_id = {
        let mut state = harness.dispatcher.inner.lock().await;
        let resource = state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .scan
            .as_mut()
            .expect("scan");
        resource.terminal_evidence = Some(super::TerminalEvidenceBinding {
            reservation_id: reservation.reservation_id.clone(),
            scope,
        });
        resource.core_operation_id.clone()
    };
    harness
        .dispatcher
        .apply_scan_terminal(&ubm_desktop::ScanTerminalEvent {
            sequence: 1,
            operation_id: operation_id.clone(),
            aborted: true,
            detail: "test scan end".into(),
        })
        .await;
    let terminal = harness.wait_terminal(&handle).await;
    assert_eq!(terminal["reason"], "source-failed");
    assert_eq!(terminal["terminalEvidenceStageFailed"], true);
    assert!(
        terminal.get("evidence").is_none(),
        "stage failure must not forge a receipt"
    );
    assert_eq!(
        terminal["error"]["operation"],
        "tauri.terminal-evidence.stage-before-emit"
    );
    assert!(
        harness.with_caller(|caller| caller.scan.is_some()).await,
        "binding survives failed stage for retry/recovery"
    );
    harness
        .dispatcher
        .apply_scan_terminal(&ubm_desktop::ScanTerminalEvent {
            sequence: 2,
            operation_id: operation_id.clone(),
            aborted: true,
            detail: "duplicate scan end".into(),
        })
        .await;
    assert_eq!(
        harness
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event["streamId"] == handle && event["item"]["kind"] == "terminal")
            .count(),
        1,
        "a duplicate core end cannot resend the delivered stage-failure terminal"
    );
    let late_forwarder_error = super::DispatchError::new(
        BleErrorCode::PlatformFailure,
        "scan",
        "tauri.test.late-forwarder-error",
    );
    harness
        .dispatcher
        .terminal(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &handle,
            "source-failed",
            Some(&late_forwarder_error),
        )
        .await
        .expect("terminalized scan ignores late forwarder terminal");
    assert_eq!(
        harness
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event["streamId"] == handle && event["item"]["kind"] == "terminal")
            .count(),
        1,
        "a post-terminal forwarder error cannot stage or emit a second terminal"
    );
    harness
        .execute(
            "scan.stop",
            vec![("scanHandle", string(handle.clone()))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("terminalized scan stop cleans up physical mapping");
    assert_eq!(
        harness
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event["streamId"] == handle && event["item"]["kind"] == "terminal")
            .count(),
        1,
        "scan.stop after terminal delivery cannot emit owner-released"
    );
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_terminal_stage_failure_delivery_quota_is_retained_as_retryable_cleanup_debt() {
    let harness = Harness::new().await;
    let directory = scan_recovery_test_directory("scan-terminal-stage-delivery-debt");
    std::fs::create_dir(&directory).expect("fixture directory");
    let ledger = Arc::new(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            1024,
        )
        .expect("ledger"),
    );
    let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "scan".into(),
        manager_id: "manager".into(),
        operation_id: "stage-delivery-debt".into(),
        incarnation: "incarnation".into(),
        scope_utf16le: None,
    };
    let reservation = ledger
        .reserve(&scope, 1)
        .expect("reserve tiny evidence budget");
    *harness
        .dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let operation_id = {
        let mut state = harness.dispatcher.inner.lock().await;
        let caller = state.callers.get_mut(&harness.key()).expect("caller");
        caller
            .pending_events
            .extend((0..super::MAX_PENDING_EVENTS).map(|index| format!("queued-{index}")));
        let resource = caller.scan.as_mut().expect("scan");
        resource.terminal_evidence = Some(super::TerminalEvidenceBinding {
            reservation_id: reservation.reservation_id.clone(),
            scope,
        });
        resource.core_operation_id.clone()
    };
    harness
        .dispatcher
        .apply_scan_terminal(&ubm_desktop::ScanTerminalEvent {
            sequence: 1,
            operation_id,
            aborted: true,
            detail: "test scan end".into(),
        })
        .await;
    assert!(
        harness
            .with_caller(|caller| {
                caller.scan.as_ref().is_some_and(|scan| {
                    scan.terminal_stage_failure.is_some()
                        && scan
                            .terminal_delivery_failure
                            .as_ref()
                            .is_some_and(|error| {
                                error.operation == "tauri.scan-terminal-stage-event"
                            })
                })
            })
            .await,
        "the exact queue refusal remains attached to the live scan mapping"
    );
    let stop = harness
        .execute(
            "scan.stop",
            vec![("scanHandle", string(handle.clone()))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("scan.stop must not discard an undelivered terminal debt");
    assert_eq!(stop.operation, "tauri.scan-terminal-stage-event");
    harness
        .dispatcher
        .acknowledge(
            harness.caller.clone(),
            BTreeMap::from([
                ("eventId".to_owned(), string("queued-0")),
                (
                    "rendererLease".to_owned(),
                    object([
                        ("leaseId", string(LEASE_ID)),
                        ("generation", string(LEASE_GENERATION)),
                    ]),
                ),
            ]),
        )
        .await
        .expect("one acknowledged event drains capacity");
    // No user stop/release follows this acknowledgement. The drain itself
    // retries the retained terminal delivery and gives a terminal-only
    // consumer a deterministic source-failed outcome.
    let terminal = harness.wait_terminal(&handle).await;
    assert_eq!(terminal["terminalEvidenceStageFailed"], true);
    assert_eq!(
        terminal["error"]["operation"],
        "tauri.terminal-evidence.stage-before-emit"
    );
    assert!(
        harness
            .with_caller(|caller| {
                caller.scan.as_ref().is_some_and(|scan| {
                    scan.terminal_stage_failure.is_none()
                        && scan.terminal_delivery_failure.is_none()
                })
            })
            .await,
        "a successful retry retires only the delivery debt, not the scan binding"
    );
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_release_joins_a_held_scan_terminal_delivery_retry() {
    let harness = Harness::new().await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let held_error = super::DispatchError::new(
        BleErrorCode::PlatformTransport,
        "stream",
        "tauri.scan-terminal-stage-event",
    );
    let (sender, receiver) = tokio::sync::watch::channel(None);
    {
        let mut state = harness.dispatcher.inner.lock().await;
        let resource = state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .scan
            .as_mut()
            .expect("scan");
        resource.terminal_stage_failure = Some(held_error.clone());
        resource.terminal_delivery_failure = Some(held_error.clone());
        resource.terminal_delivery_retry = Some(receiver);
    }
    let dispatcher = harness.dispatcher.clone();
    let key = harness.key();
    let mut release = tokio::spawn(async move { dispatcher.release(&key).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut release)
            .await
            .is_err(),
        "release must join the in-flight retry rather than emit owner-released or abandon evidence"
    );
    sender
        .send(Some(Err(held_error)))
        .expect("wake the retained release joiner");
    let cleanup = release.await.expect("joined release task").into_wire();
    assert_eq!(cleanup["state"], "release-failed");
    assert!(cleanup
        .to_string()
        .contains("tauri.scan-terminal-stage-event"));
    // The scan still exists and no terminal was sent while the retry was held.
    assert!(harness.with_caller(|caller| caller.scan.is_some()).await);
    assert!(
        harness
            .events
            .lock()
            .expect("events")
            .iter()
            .all(|event| event["streamId"] != handle),
        "a joined release never emits a competing owner-release terminal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_release_after_successful_held_terminal_retry_emits_no_owner_terminal() {
    let harness = Harness::new().await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let stage_failure = super::DispatchError::new(
        BleErrorCode::PlatformFailure,
        "stream",
        "tauri.terminal-evidence.stage-before-emit",
    );
    let (sender, receiver) = tokio::sync::watch::channel(None);
    {
        let mut state = harness.dispatcher.inner.lock().await;
        let resource = state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .scan
            .as_mut()
            .expect("scan");
        resource.terminal_stage_failure = Some(stage_failure);
        resource.terminal_delivery_retry = Some(receiver);
    }
    let dispatcher = harness.dispatcher.clone();
    let key = harness.key();
    let mut release = tokio::spawn(async move { dispatcher.release(&key).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut release)
            .await
            .is_err(),
        "release joins the held terminal retry"
    );
    sender.send(Some(Ok(()))).expect("complete held retry");
    assert_eq!(
        release.await.expect("release task").into_wire()["state"],
        "released"
    );
    assert!(
        !harness
            .dispatcher
            .inner
            .lock()
            .await
            .callers
            .contains_key(&harness.key()),
        "the joined successful cleanup retires the caller without a second terminal"
    );
    assert!(
        harness
            .events
            .lock()
            .expect("events")
            .iter()
            .all(|event| event["streamId"] != handle),
        "a joined successful terminal retry suppresses a contradictory owner-released terminal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_scan_stop_does_not_retry_another_scans_terminal_delivery_debt() {
    let harness = Harness::new().await;
    harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    {
        let mut state = harness.dispatcher.inner.lock().await;
        let resource = state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .scan
            .as_mut()
            .expect("scan");
        resource.terminal_stage_failure = Some(super::DispatchError::new(
            BleErrorCode::PlatformFailure,
            "stream",
            "tauri.terminal-evidence.stage-before-emit",
        ));
        resource.terminal_delivery_failure = Some(super::DispatchError::new(
            BleErrorCode::StreamQuota,
            "stream",
            "tauri.scan-terminal-stage-event",
        ));
    }
    harness
        .execute(
            "scan.stop",
            vec![("scanHandle", string("already-released"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("unknown scan handle stays idempotently released");
    assert!(
        harness
            .with_caller(|caller| {
                caller
                    .scan
                    .as_ref()
                    .is_some_and(|scan| scan.terminal_stage_failure.is_some())
            })
            .await,
        "the stale stop must not advance a different scan's terminal debt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_terminal_claim_fences_a_forwarder_terminal_before_staging() {
    let harness = Harness::new().await;
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    {
        let mut state = harness.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .scan
            .as_mut()
            .expect("scan")
            .terminal_failure_claimed = true;
    }
    let late = super::DispatchError::new(
        BleErrorCode::PlatformFailure,
        "scan",
        "tauri.test.forwarder-raced-lifecycle-stage",
    );
    harness
        .dispatcher
        .terminal(
            &harness.key(),
            (LEASE_ID, LEASE_GENERATION),
            &handle,
            "source-failed",
            Some(&late),
        )
        .await
        .expect("claimed scan suppresses the racing forwarder terminal");
    assert!(
        harness.events.lock().expect("events").is_empty(),
        "no terminal can overtake the lifecycle claim while its stage is fallible"
    );
    assert!(
        matches!(
            harness
                .dispatcher
                .scan_delivery(&harness.key(), &handle)
                .await,
            super::Delivery::Paused
        ),
        "the forwarder is fenced before the lifecycle sends its marker"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_terminal_send_failure_is_retained_and_retried_without_stage_failure_branding() {
    let harness = Harness::new().await;
    let directory = scan_recovery_test_directory("scan-terminal-send-retry");
    create_private_fixture_directory(&directory);
    let ledger = Arc::new(
        ubm_desktop::terminal_evidence_ledger::TerminalEvidenceLedger::open(
            &directory.join("terminal-evidence.sqlite"),
            4096,
        )
        .expect("ledger"),
    );
    let scope = ubm_desktop::terminal_evidence_ledger::TerminalEvidenceScope {
        producer: "scan".into(),
        manager_id: "manager".into(),
        operation_id: "terminal-send-retry".into(),
        incarnation: "incarnation".into(),
        scope_utf16le: None,
    };
    let reservation = ledger.reserve(&scope, 4096).expect("reserve evidence");
    *harness
        .dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::clone(&ledger));
    let scan = harness
        .execute(
            "scan.start",
            vec![("query", empty_scan_query())],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("scan starts");
    let handle = text(&scan, "handle");
    let operation_id = {
        let mut state = harness.dispatcher.inner.lock().await;
        let caller = state.callers.get_mut(&harness.key()).expect("caller");
        let accepted = caller.event_sink.clone();
        caller.event_sink = IpcEventSink::new(Channel::new(|_| {
            Err(tauri::Error::Io(std::io::Error::other(
                "scripted terminal transport refusal",
            )))
        }));
        let resource = caller.scan.as_mut().expect("scan");
        resource.terminal_evidence = Some(super::TerminalEvidenceBinding {
            reservation_id: reservation.reservation_id.clone(),
            scope,
        });
        let operation_id = resource.core_operation_id.clone();
        (accepted, operation_id)
    };
    harness
        .dispatcher
        .apply_scan_terminal(&ubm_desktop::ScanTerminalEvent {
            sequence: 1,
            operation_id: operation_id.1.clone(),
            aborted: true,
            detail: "test terminal send failure".into(),
        })
        .await;
    let committed = ledger
        .recoverable("scan", &harness.key())
        .expect("committed evidence remains recoverable after channel refusal");
    assert_eq!(committed.len(), 1);
    let committed_reference = committed[0].reference.clone().expect("committed reference");
    let committed_digest = committed[0].digest.expect("committed digest");
    assert!(
        harness
            .with_caller(|caller| {
                caller.scan.as_ref().is_some_and(|scan| {
                    scan.terminal_failure_claimed
                        && scan.terminal_stage_failure.is_none()
                        && scan
                            .terminal_delivery_failure
                            .as_ref()
                            .is_some_and(|error| error.operation == "tauri.terminal-send")
                        && matches!(
                            scan.terminal_delivery_retry_item,
                            Some(super::ScanTerminalRetry::Terminal { .. })
                        )
                })
            })
            .await,
        "the exact normal terminal is retained; it is not mislabeled as a stage failure"
    );
    {
        let mut state = harness.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&harness.key())
            .expect("caller")
            .event_sink = operation_id.0;
    }
    harness
        .dispatcher
        .retry_scan_terminal_delivery_failure(&harness.key())
        .await
        .expect("the exact retained terminal retries after transport recovery");
    let terminal = harness.wait_terminal(&handle).await;
    assert_eq!(terminal["reason"], "source-failed");
    assert_eq!(terminal["evidence"]["ref"], committed_reference);
    assert_eq!(
        terminal["evidence"]["digest"],
        committed_digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert!(
        terminal.get("terminalEvidenceStageFailed").is_none(),
        "a transport failure must not use the authenticated evidence-stage marker"
    );
    assert!(
        harness
            .with_caller(|caller| {
                caller.scan.as_ref().is_some_and(|scan| {
                    scan.terminal_failure_sent
                        && scan.terminal_delivery_failure.is_none()
                        && scan.terminal_delivery_retry_item.is_none()
                })
            })
            .await,
        "successful retry terminalizes the retained scan and prevents an owner-release duplicate"
    );
    std::fs::remove_dir_all(directory).expect("fixture cleanup");
}

// Finding 57 — the new attachment generation reaches the renderer the way
// the IPC represents a backend restart: routes on the previous attachment
// fail `backend.reset` (recreate the manager) before any native I/O, the
// releases a renderer owes still run, and a fresh attach binds the new
// generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_57_a_route_validated_before_rebind_stays_on_its_original_attachment() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let request = harness.route_request(
        "gatt.read",
        "gatt.read-validated-before-rebind",
        Harness::gatt_entries(&link, &database, CONTROL_POINT),
        None,
    );
    let envelope = into_object(
        required_value(&request, "envelope", "test.route-envelope")
            .expect("route envelope")
            .clone(),
        "test.route-envelope",
    )
    .expect("route envelope object");
    let validated_attachment = harness
        .dispatcher
        .validate_envelope(&harness.caller, "gatt.read", &envelope)
        .await
        .expect("the original attachment validates before reset");

    let reset = harness.lose_adapter().await;
    harness
        .dispatcher
        .rebind_callers(reset.previous.attachment_id().as_str(), &reset.current)
        .await;
    assert_eq!(
        harness
            .dispatcher
            .bound_attachment(&harness.caller, "test.route-attachment")
            .await
            .expect("caller remains bound")
            .attachment_id,
        reset.current.attachment_id().as_str(),
        "the caller is rebound after the old envelope was validated"
    );
    let reads = count(&harness.radio().calls(), "read");
    let error = harness
        .dispatcher
        .execute_for_attachment(
            &harness.caller,
            "gatt.read",
            &validated_attachment,
            Harness::lease_payload(Harness::gatt_entries(&link, &database, CONTROL_POINT)),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect_err("the route remains bound to the attachment it validated");

    assert_eq!(
        error.identity(),
        (
            "backend.reset",
            "adapter",
            "tauri.route-attachment".to_owned()
        )
    );
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(
        count(&harness.radio().calls(), "read"),
        reads,
        "no native I/O"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_57_the_previous_attachment_fails_backend_reset_before_native_io() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let reset = harness.lose_adapter().await;
    assert_eq!(
        reset.previous.backend_generation().as_str(),
        harness.attachment.backend_generation
    );
    harness
        .radio()
        .push_event(RadioEvent::AdapterState(AdapterPowerState::PoweredOn));

    let reads = count(&harness.radio().calls(), "read");
    for (command, entries) in [
        (
            "gatt.read",
            Harness::gatt_entries(&link, &database, CONTROL_POINT),
        ),
        ("adapter.state", Vec::new()),
        ("scan.start", vec![("query", empty_scan_query())]),
    ] {
        let error = harness
            .route(command, &format!("{command}-stale"), entries, None)
            .await
            .expect_err("a route on the previous attachment is refused");
        assert_eq!(
            error.identity(),
            (
                "backend.reset",
                "adapter",
                "tauri.route-attachment".to_owned()
            ),
            "{command}"
        );
        assert_eq!(error.commit, Some(CommitState::NotDispatched));
    }
    assert_eq!(
        count(&harness.radio().calls(), "read"),
        reads,
        "no native I/O"
    );
    assert!(!harness.radio().scan_active());

    // The releases the renderer owes for the old attachment still run.
    harness
        .route(
            "connection.disconnect",
            "disconnect-after-reset",
            Harness::link_entries(&link),
            None,
        )
        .await
        .expect("a link the adapter took releases");
    assert!(
        harness
            .with_caller(|caller| caller.connections.is_empty())
            .await
    );
    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert!(super::is_released(&cleanup), "{cleanup:?}");

    // A fresh attach binds the central's new generation.
    let second = AuthenticatedCaller::new("test-app".to_owned(), "second".to_owned());
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    let response = harness
        .dispatcher
        .bootstrap(second, sink, version_offer())
        .await
        .expect("a fresh attach succeeds");
    let bootstrap = field(&response, "bootstrap");
    let attachment = field(bootstrap, "attachment");
    assert_eq!(
        text(attachment, "backendGeneration"),
        reset.current.backend_generation().as_str()
    );
    assert_eq!(
        text(attachment, "attachmentId"),
        reset.current.attachment_id().as_str()
    );
    assert_ne!(
        text(attachment, "backendGeneration"),
        harness.attachment.backend_generation
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gatt_resolution_reset_takes_precedence_over_stale_target_error() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let attachment = harness.attachment.clone();
    harness
        .dispatcher
        .refuse_stale_attachment(&attachment, "gatt.read")
        .await
        .expect("the initial attachment check passes");
    let resolving = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dispatcher = harness.dispatcher.clone();
    let resolving_task = Arc::clone(&resolving);
    let release_task = Arc::clone(&release);
    let resolution = tokio::spawn(async move {
        dispatcher
            .resolve_after_attachment_check(&attachment, "gatt.read", || async move {
                resolving_task.notify_one();
                release_task.notified().await;
                Err::<(), _>(DispatchError::new(
                    BleErrorCode::GattStaleHandle,
                    "gatt",
                    "tauri.characteristic-database-generation",
                ))
            })
            .await
    });
    resolving.notified().await;
    let reset = harness.lose_adapter().await;
    harness
        .dispatcher
        .rebind_callers(reset.previous.attachment_id().as_str(), &reset.current)
        .await;
    release.notify_one();
    let error = resolution
        .await
        .expect("resolution task joins")
        .expect_err("reset takes precedence over stale target");
    assert_eq!(
        error.identity(),
        (
            "backend.reset",
            "adapter",
            "tauri.route-attachment".to_owned()
        )
    );
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gatt_resolution_preserves_error_when_attachment_is_unchanged() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let error = harness
        .dispatcher
        .resolve_after_attachment_check(&harness.attachment, "gatt.read", || async {
            Err::<(), _>(DispatchError::new(
                BleErrorCode::GattNotFound,
                "gatt",
                "tauri.characteristic-handle",
            ))
        })
        .await
        .expect_err("resolution error propagates");
    assert_eq!(
        error.identity(),
        (
            "gatt.not-found",
            "gatt",
            "tauri.characteristic-handle".to_owned()
        )
    );
    assert_eq!(error.commit, None);
}

// Finding 58 — admission errors from the core cross unchanged: code,
// domain, operation, detail, retryability and commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adapter_reset_refuses_route_before_gatt_reservation_without_pump_rebind() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    // Deterministically deliver link invalidation before attachment rebinding,
    // the two independent reset/lifecycle receivers selected by the pump.
    let pump = harness
        .dispatcher
        .lifecycle_pump
        .lock()
        .unwrap()
        .take()
        .unwrap();
    pump.abort();
    assert!(
        pump.await.is_err(),
        "the test owns a stopped lifecycle pump"
    );
    let mut lifecycle = harness.central.lifecycle_events();
    harness.lose_adapter().await;
    let event = tokio::time::timeout(WAIT, lifecycle.recv())
        .await
        .expect("adapter loss lifecycle event")
        .expect("lifecycle event");
    assert!(matches!(event.kind, LifecycleKind::AdapterLost));
    harness.dispatcher.apply_lifecycle_event(&event).await;
    let reads = count(&harness.radio().calls(), "read");

    let error = harness
        .route(
            "gatt.read",
            "reset-before-reservation",
            Harness::gatt_entries(&link, &database, CONTROL_POINT),
            None,
        )
        .await
        .expect_err("old attachment cannot reserve a GATT operation");

    assert_eq!(
        error.identity(),
        (
            "backend.reset",
            "adapter",
            "tauri.route-attachment".to_owned()
        )
    );
    assert_eq!(error.commit, Some(CommitState::NotDispatched));
    assert_eq!(
        count(&harness.radio().calls(), "read"),
        reads,
        "no native I/O"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_58_admission_refusals_cross_verbatim() {
    for (power, authorization, expected) in [
        (
            AdapterPowerState::PoweredOff,
            AdapterAuthorization::Granted,
            "adapter.powered-off",
        ),
        (
            AdapterPowerState::Resetting,
            AdapterAuthorization::Granted,
            "adapter.resetting",
        ),
        (
            AdapterPowerState::PoweredOn,
            AdapterAuthorization::Denied,
            "permission.denied",
        ),
        (
            AdapterPowerState::PoweredOn,
            AdapterAuthorization::Restricted,
            "permission.restricted",
        ),
        (
            AdapterPowerState::PoweredOn,
            AdapterAuthorization::NotDetermined,
            "permission.not-determined",
        ),
    ] {
        let radio = FakeRadio::new();
        radio.set_os_policy(AdmissionPolicy::CoreBluetooth, false);
        radio.set_adapter_state(power);
        radio.set_adapter_authorization(authorization);
        let harness = Harness::with_radio(radio).await;
        harness.advertise("peer-a").await;
        let refused = harness
            .execute(
                "connection.connect",
                vec![("peerId", string("peer-a"))],
                None,
                OpControl::unbounded(),
            )
            .await
            .expect_err("admission refuses before any effect");
        let core = harness
            .central
            .connect("peer-a", "lease-direct", OpControl::unbounded())
            .await
            .expect_err("the core refuses the same way");
        assert_eq!(refused.code.as_str(), expected);
        assert_eq!(
            wire_error(&refused),
            wire_error(&DispatchError::from_core(&core)),
            "{expected} crosses unchanged"
        );
        assert_eq!(count(&harness.radio().calls(), "connect"), 0);
    }
}

// Finding 94 — an adapter.state read admitted before an adapter reset and
// still in flight when it lands answers the post-transition snapshot, as
// every legacy host did: never `operation.reset` or `backend.reset`. The
// radio read is held (a barrier, not timing) until the reset is published.
// A request that arrives after the reset still fails `backend.reset`
// (finding 57).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_94_an_adapter_state_read_racing_a_reset_answers_the_new_state() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    harness
        .radio()
        .set_adapter_authorization(AdapterAuthorization::Granted);
    harness
        .radio()
        .set_adapter_state(AdapterPowerState::PoweredOn);
    harness.radio().block_op(FaultOp::AdapterState);
    let read = harness.execute("adapter.state", Vec::new(), None, OpControl::unbounded());
    let reset = async {
        let deadline = tokio::time::Instant::now() + WAIT;
        while count(&harness.radio().calls(), "adapter_state") == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the read reaches the radio"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        harness
            .radio()
            .set_adapter_state(AdapterPowerState::PoweredOff);
        let reset = harness.lose_adapter().await;
        harness.radio().unblock_op(FaultOp::AdapterState);
        reset
    };
    let (read, reset) = tokio::join!(read, reset);
    let state = field(&read.expect("the racing read answers"), "state")
        .clone()
        .into_wire();
    assert_eq!(state["power"], "off");
    assert_eq!(
        state["backendGeneration"],
        reset.current.backend_generation().as_str(),
        "the snapshot carries the generation it was read under"
    );
}

// Finding 60 — adapter.state reports the central's facts in the IPC
// vocabulary: resetting / unsupported power, authorization as the OS
// reported it, and a stated reason where the platform cannot answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_60_adapter_state_reports_the_central_s_facts() {
    let harness = Harness::new().await;
    let radio = harness.radio();
    radio.set_adapter_authorization(AdapterAuthorization::Granted);
    for (power, availability, wire_power) in [
        (AdapterPowerState::PoweredOn, "available", "on"),
        (AdapterPowerState::PoweredOff, "available", "off"),
        (AdapterPowerState::Resetting, "available", "resetting"),
        (AdapterPowerState::Unknown, "available", "unknown"),
    ] {
        radio.set_adapter_state(power);
        let state = harness.adapter_state().await;
        assert_eq!(state["availability"], availability, "{power:?}");
        assert_eq!(state["power"], wire_power, "{power:?}");
        assert_eq!(state["authorization"], "granted", "{power:?}");
    }

    radio.set_adapter_state(AdapterPowerState::Unsupported);
    let state = harness.adapter_state().await;
    assert_eq!(state["availability"], "unsupported");
    assert_eq!(state["power"], "unsupported");
    assert_eq!(state["authorization"], "unavailable");
    assert!(state["safeReason"].is_string());

    // CoreBluetooth's Unauthorized is the legacy `authorization: denied`.
    radio.set_adapter_state(AdapterPowerState::Unauthorized);
    let state = harness.adapter_state().await;
    assert_eq!(state["power"], "unknown");
    assert_eq!(state["authorization"], "denied");

    radio.set_adapter_state(AdapterPowerState::PoweredOn);
    for (authorization, wire) in [
        (AdapterAuthorization::Denied, "denied"),
        (AdapterAuthorization::Restricted, "restricted"),
        (AdapterAuthorization::NotDetermined, "not-determined"),
    ] {
        radio.set_adapter_authorization(authorization);
        assert_eq!(harness.adapter_state().await["authorization"], wire);
    }

    // `heard` is the boundary's own peer list (the scripted radio lists
    // none), never a dispatcher count.
    harness.advertise("peer-a").await;
    assert_eq!(harness.adapter_state().await["heard"], 0);
    assert!(harness.radio().calls().iter().any(|call| call == "peers"));
}

// Finding 60 — a radio that cannot read authorization says so: `unknown`
// with the core's reason, never a denial and never silently absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_60_an_unreadable_authorization_is_unknown_with_a_reason() {
    let harness = Harness::new().await;
    harness
        .radio()
        .set_adapter_state(AdapterPowerState::PoweredOn);
    let state = harness.adapter_state().await;
    assert_eq!(state["authorization"], "unknown");
    assert!(
        state["safeReason"]
            .as_str()
            .is_some_and(|reason| reason.contains("authorization")),
        "{state:?}"
    );
    assert_eq!(state["power"], "on");
}

// Finding 60 — after a reset the attachment the renderer re-binds reports
// the new backend generation in adapter.state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_60_adapter_state_follows_the_reset_generation() {
    let harness = Harness::with_radio(os_radio(AdmissionPolicy::LifecycleOnly)).await;
    let reset = harness.lose_adapter().await;
    let rebound = harness
        .dispatcher
        .ensure_adapter()
        .await
        .expect("the new attachment");
    assert_eq!(
        rebound.backend_generation,
        reset.current.backend_generation().as_str()
    );
    assert_eq!(rebound.adapter_id, harness.attachment.adapter_id);
}

// Finding 90 — the maximum write length is the core's answer per write
// mode, and equals what a write of that mode actually accepts: with an OS
// long write (Windows/Linux) a with-response write takes up to 512 bytes
// while a write without response stays one ATT payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_90_the_maximum_write_length_is_what_a_write_accepts_per_mode() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness
        .radio()
        .set_write_limits("peer-a", WriteLimits::os_long_write(185).expect("limits"));
    let maximum = |mode: &'static str| {
        let mut entries = Harness::link_entries(&link);
        entries.push(("mode", string(mode)));
        harness.execute(
            "connection.maximum-write-length",
            entries,
            None,
            OpControl::unbounded(),
        )
    };
    let with_response = maximum("with-response").await.expect("with response");
    assert_eq!(field(&with_response, "bytes"), &number(512));
    let without_response = maximum("without-response").await.expect("without response");
    assert_eq!(field(&without_response, "bytes"), &number(182));
    let error = maximum("sometimes").await.expect_err("an unknown mode");
    assert_eq!(error.code, BleErrorCode::ArgumentInvalid);

    let write = |correlation: &'static str, length: usize| {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("mode", string("with-response")));
        harness.route("gatt.write", correlation, entries, Some(vec![0x5a; length]))
    };
    write("write-at-maximum", 512)
        .await
        .expect("the reported maximum is accepted");
    let refused = write("write-past-maximum", 513)
        .await
        .expect_err("one byte past the maximum is refused");
    assert_ne!(refused.code, BleErrorCode::OperationTimedOut);
}

// Finding 95 — a discovery the core refuses whole crosses the IPC with its
// typed identity unchanged (a malformed platform UUID is
// `protocol.malformed`, a database past the ATT handle space
// `capability.limited`); nothing is skipped, no database handle is minted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_95_a_refused_discovery_crosses_the_ipc_whole_and_typed() {
    let malformed = {
        let mut service = hrm_service();
        service.characteristics[1].uuid = "not-a-uuid".to_owned();
        vec![service]
    };
    let oversized = {
        let mut service = hrm_service();
        let template = service.characteristics[0].clone();
        service.characteristics = (0..65_535u32)
            .map(|index| CharacteristicSnapshot {
                uuid: format!("{:08x}-0000-1000-8000-00805f9b34fb", 0x10_0000 + index),
                ..template.clone()
            })
            .collect();
        vec![service]
    };
    for (services, code, operation) in [
        (malformed, "protocol.malformed", "discovery.snapshot.uuid"),
        (oversized, "capability.limited", "discovery.database-bound"),
    ] {
        let harness = Harness::new().await;
        let link = harness.connect("peer-r").await;
        harness.radio().set_services("peer-r", services);
        let refused = harness
            .execute(
                "gatt.discover",
                Harness::link_entries(&link),
                None,
                OpControl::unbounded(),
            )
            .await
            .expect_err("the discovery is refused whole");
        assert_eq!(
            refused.identity(),
            (code, "gatt", operation.to_owned()),
            "{code}"
        );
        assert_eq!(
            harness.with_caller(|caller| caller.databases.len()).await,
            0,
            "no database handle for a refused discovery"
        );
    }
}

// Finding 114 — the release of a resource nobody could own (a cancelled or
// refused scan, connect or subscribe that the core still admitted) is
// retried automatically on the Tauri 4.x schedule: 8 attempts in all, the
// retries 100 ms apart doubling to 5 s. The debt stays observable while it
// retries; exhaustion is reported as `tauri.quarantine.exhausted`; a window
// released meanwhile takes the debt over and the retries still end it.
impl Harness {
    /// A scan the core admitted for a lease that was replaced while it
    /// started: nobody can own it, so its compensation runs. `refusals`
    /// scan stops fail first (the compensation's own attempt included).
    async fn orphan_a_scan(&self, refusals: usize) -> OperationId {
        self.radio().block_op(FaultOp::StartScan);
        let dispatcher = self.dispatcher.clone();
        let caller = self.caller.clone();
        let payload = Self::lease_payload(vec![("query", empty_scan_query())]);
        let start = tokio::spawn(async move {
            dispatcher
                .start_scan(&caller, payload, OpControl::unbounded())
                .await
        });
        while count(&self.radio().calls(), "start_scan") == 0 {
            tokio::task::yield_now().await;
        }
        self.replace_lease().await;
        for attempt in 0..refusals {
            self.radio()
                .fail_next(FaultOp::StopScan, &format!("stop refused {attempt}"));
        }
        self.radio().unblock_op(FaultOp::StartScan);
        let error = start
            .await
            .expect("joins")
            .expect_err("a replaced lease cannot own the scan");
        assert_eq!(error.identity().2, "tauri.scan-stale-lease");
        self.central
            .active_scan_id()
            .expect("the compensation failed")
    }

    async fn debt(&self) -> Vec<(u32, bool)> {
        self.dispatcher
            .inner
            .lock()
            .await
            .orphan_debt
            .iter()
            .map(|debt| (debt.attempts, debt.exhausted))
            .collect()
    }

    fn stops(&self) -> usize {
        count(&self.radio().calls(), "stop_scan")
    }
}

/// Let the paused clock run `millis` forward and every task settle.
async fn elapse(millis: u64) {
    tokio::time::sleep(Duration::from_millis(millis)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn finding_114_a_failed_compensation_retries_until_the_release_lands() {
    let harness = Harness::on_current_runtime().await;
    // The compensation and the first two retries are refused; the third
    // retry (attempt 4) releases the scan.
    harness.orphan_a_scan(3).await;
    assert_eq!(harness.stops(), 1);
    assert_eq!(harness.debt().await, vec![(1, false)], "observable debt");

    elapse(100).await;
    assert_eq!(harness.stops(), 2, "first retry after 100 ms");
    assert_eq!(harness.debt().await, vec![(2, false)]);
    elapse(200).await;
    assert_eq!(harness.stops(), 3, "second retry 200 ms later");
    elapse(400).await;
    assert_eq!(harness.stops(), 4, "third retry 400 ms later");
    assert!(harness.debt().await.is_empty(), "the release landed");
    assert_eq!(harness.central.active_scan_id(), None);

    elapse(60_000).await;
    assert_eq!(harness.stops(), 4, "nothing retries a settled debt");
    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(cleanup.into_wire()["state"], "released");
}

#[tokio::test(start_paused = true)]
async fn orphan_retry_survives_transient_authority_lock_contention() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(1).await;
    assert_eq!(harness.stops(), 1);
    let slot = harness.dispatcher.authority.lock().await;
    elapse(100).await;
    assert_eq!(
        harness.stops(),
        1,
        "authority inspection is momentarily held"
    );
    drop(slot);
    elapse(100).await;
    assert_eq!(
        harness.stops(),
        2,
        "retry resumes when authority is readable"
    );
    assert!(
        harness.debt().await.is_empty(),
        "native cleanup settles exact debt"
    );
}

#[tokio::test(start_paused = true)]
async fn finding_114_eight_refused_attempts_exhaust_and_are_reported() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(9).await;
    let mut expected_stops = 1;
    for delay in [100, 200, 400, 800, 1_600, 3_200, 5_000] {
        elapse(delay).await;
        expected_stops += 1;
        assert_eq!(harness.stops(), expected_stops, "retry after {delay} ms");
    }
    assert_eq!(
        harness.debt().await,
        vec![(8, true)],
        "exhausted, still owed"
    );
    elapse(60_000).await;
    assert_eq!(harness.stops(), 8, "no retry after exhaustion");
    assert!(harness.central.active_scan_id().is_some(), "still owned");

    // The window's release makes one more attempt (the ninth refusal) and
    // reports the exhausted debt.
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "release-failed");
    assert_eq!(cleanup["failures"][0]["resourceKind"], "scan");
    assert_eq!(
        cleanup["failures"][0]["error"]["operation"],
        "tauri.quarantine.exhausted"
    );
    assert_eq!(harness.stops(), 9);

    // Nothing refuses any more: the next release lands.
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "released");
    assert_eq!(harness.central.active_scan_id(), None);
}

#[tokio::test(start_paused = true)]
async fn finding_114_a_window_released_during_retries_still_ends_the_orphan() {
    let harness = Harness::on_current_runtime().await;
    // The compensation and the window's release are refused; the automatic
    // retry that follows releases the scan after the window is gone.
    harness.orphan_a_scan(2).await;
    elapse(50).await;
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "release-failed");
    assert_eq!(
        cleanup["failures"][0]["error"]["operation"],
        "tauri.release.orphan"
    );
    assert_eq!(harness.stops(), 2);
    assert_eq!(
        harness.debt().await.len(),
        1,
        "the debt outlives the release"
    );

    elapse(50).await;
    assert_eq!(harness.stops(), 3, "the retry schedule continues");
    assert!(harness.debt().await.is_empty());
    assert_eq!(harness.central.active_scan_id(), None, "no orphan stays up");
    elapse(60_000).await;
    assert_eq!(harness.stops(), 3);
}

#[tokio::test(start_paused = true)]
async fn finding_114_a_release_that_lands_first_ends_the_retries() {
    let harness = Harness::on_current_runtime().await;
    harness.orphan_a_scan(1).await;
    elapse(50).await;
    let cleanup = harness.dispatcher.release(&harness.key()).await.into_wire();
    assert_eq!(cleanup["state"], "released");
    assert_eq!(harness.stops(), 2);
    elapse(60_000).await;
    assert_eq!(harness.stops(), 2, "no retry of a released orphan");
    assert!(harness.debt().await.is_empty());
}

// Finding 116 — the OS's own answer behind a failure crosses as the same
// per-OS platform identity the Node desktop path reports; a failure without
// one keeps the Tauri 4.x `btleplug` / `native-error` shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_116_the_platform_identity_crosses_per_os() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let read = || {
        harness.execute(
            "gatt.read",
            Harness::gatt_entries(&link, &database, CONTROL_POINT),
            None,
            OpControl::unbounded(),
        )
    };
    for (platform, expected) in [
        (
            PlatformDetail::new("corebluetooth", "10")
                .with_message("The connection has timed out."),
            serde_json::json!({
                "domain": "corebluetooth",
                "code": "10",
                "safeMessage": "The connection has timed out.",
                "metadata": {}
            }),
        ),
        (
            PlatformDetail::new("winrt", "gatt-protocol-error")
                .with_message("The attribute requires authentication.")
                .with_metadata("hresult", PlatformValue::Int(-2_140_864_509))
                .with_metadata("gattStatus", PlatformValue::Int(5)),
            serde_json::json!({
                "domain": "winrt",
                "code": "gatt-protocol-error",
                "safeMessage": "The attribute requires authentication.",
                "metadata": { "hresult": -2_140_864_509_i64, "gattStatus": 5 }
            }),
        ),
        (
            // No platform message: the core's detail is the safe message,
            // as on the Node desktop path.
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotPermitted"),
            serde_json::json!({
                "domain": "bluez-dbus",
                "code": "org.bluez.Error.NotPermitted",
                "safeMessage": "read refused",
                "metadata": {}
            }),
        ),
        (
            // An integer JavaScript cannot hold exactly crosses as its text.
            PlatformDetail::new("winrt", "unreachable")
                .with_message("m")
                .with_metadata("handle", PlatformValue::Int(i64::MAX))
                .with_metadata("retried", PlatformValue::Bool(true))
                .with_metadata("device", PlatformValue::Text("BluetoothLE#1".to_owned())),
            serde_json::json!({
                "domain": "winrt",
                "code": "unreachable",
                "safeMessage": "m",
                "metadata": {
                    "handle": i64::MAX.to_string(),
                    "retried": true,
                    "device": "BluetoothLE#1"
                }
            }),
        ),
    ] {
        harness
            .radio()
            .fail_next_with_platform(FaultOp::Read, "read refused", platform);
        let error = read().await.expect_err("the scripted OS failure");
        assert_eq!(wire_error(&error)["platform"], expected);
    }

    harness.radio().fail_next(FaultOp::Read, "peer went away");
    let error = read()
        .await
        .expect_err("an OS failure without platform detail");
    assert_eq!(
        wire_error(&error)["platform"],
        serde_json::json!({
            "domain": "btleplug",
            "code": "native-error",
            "safeMessage": "peer went away",
            "metadata": {}
        }),
        "the Tauri 4.x shape"
    );
}

// Finding 182 — the Polar H10 database through the Tauri dispatcher: a
// characteristic that carries descriptors must register exactly once.
// Descriptor-level core paths share their characteristic's path, and the
// desktop N-API path renders one record per characteristic (its grouping
// merges descriptor rows into the characteristic node). The dispatcher
// fanned one characteristic record out per descriptor row, so every H10
// characteristic with a CCCD or user-description descriptor (heart-rate
// measurement, battery level, device-info strings, PMD ECG) rejected the
// snapshot downstream with public-gatt.duplicate-characteristic-path.
fn h10_characteristic(
    uuid: &str,
    properties: PropertyFlags,
    descriptors: Vec<DescriptorSnapshot>,
) -> CharacteristicSnapshot {
    CharacteristicSnapshot {
        uuid: uuid.to_owned(),
        occurrence: 0,
        properties,
        descriptors,
    }
}

fn cccd() -> DescriptorSnapshot {
    DescriptorSnapshot {
        uuid: "00002902-0000-1000-8000-00805f9b34fb".to_owned(),
        occurrence: 0,
    }
}

/// The Polar H10 E9B93D29 service set from the iOS/Android discovered
/// events: 1800, 1801, 180D, 180A, 180F, the Polar custom service, PMD,
/// and FEEE — with the descriptors a real strap reports.
fn h10_services() -> Vec<ServiceSnapshot> {
    vec![
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "00001800-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![
                h10_characteristic(
                    "00002a00-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
                h10_characteristic(
                    "00002a01-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
            ],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "00001801-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "00002a05-0000-1000-8000-00805f9b34fb",
                flags(false, false, false, true),
                Vec::new(),
            )],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "0000180d-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![
                h10_characteristic(
                    "00002a37-0000-1000-8000-00805f9b34fb",
                    flags(false, false, true, false),
                    vec![cccd()],
                ),
                h10_characteristic(
                    "00002a38-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
                h10_characteristic(
                    "00002a39-0000-1000-8000-00805f9b34fb",
                    flags(false, true, false, false),
                    Vec::new(),
                ),
            ],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "0000180a-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![
                h10_characteristic(
                    "00002a29-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    vec![DescriptorSnapshot {
                        uuid: "00002901-0000-1000-8000-00805f9b34fb".to_owned(),
                        occurrence: 0,
                    }],
                ),
                h10_characteristic(
                    "00002a24-0000-1000-8000-00805f9b34fb",
                    flags(true, false, false, false),
                    Vec::new(),
                ),
            ],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "0000180f-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "00002a19-0000-1000-8000-00805f9b34fb",
                flags(true, false, true, false),
                vec![cccd()],
            )],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "6217ff4b-fb31-1140-ad5a-a45545d7ecf3".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "6217ff4c-fb31-1140-ad5a-a45545d7ecf3",
                flags(true, true, false, false),
                Vec::new(),
            )],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "fb005c80-02e7-f387-1cad-8acd2d8df0c8".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "fb005c81-02e7-f387-1cad-8acd2d8df0c8",
                flags(false, false, true, false),
                vec![cccd()],
            )],

            access: std::default::Default::default(),
        },
        ServiceSnapshot {
            primary: None,
            included_services: None,
            uuid: "0000feee-0000-1000-8000-00805f9b34fb".to_owned(),
            occurrence: 0,
            characteristics: vec![h10_characteristic(
                "0000feef-0000-1000-8000-00805f9b34fb",
                flags(true, false, false, false),
                Vec::new(),
            )],

            access: std::default::Default::default(),
        },
    ]
}

fn characteristic_keys(records: &[IpcValue]) -> Vec<String> {
    records
        .iter()
        .map(|record| {
            format!(
                "{}|{}|{}|{}",
                text(record, "serviceUuid"),
                text(record, "serviceOccurrence"),
                text(record, "characteristicUuid"),
                text(record, "characteristicOccurrence"),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_182_a_characteristic_with_descriptors_registers_once() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-h10").await;
    harness.radio().set_services("peer-h10", h10_services());
    let response = harness
        .execute(
            "gatt.discover",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("discover");
    let IpcValue::Array(characteristics) = field(&response, "characteristics") else {
        panic!("characteristics must be an array");
    };
    let IpcValue::Array(descriptors) = field(&response, "descriptors") else {
        panic!("descriptors must be an array");
    };
    // Twelve scripted characteristics, four scripted descriptors: one
    // record each, so the portable snapshot validates downstream.
    assert_eq!(
        characteristics.len(),
        12,
        "one characteristic record per characteristic: {characteristics:?}"
    );
    assert_eq!(
        descriptors.len(),
        4,
        "one descriptor record per descriptor: {descriptors:?}"
    );
    let mut keys = characteristic_keys(characteristics);
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        12,
        "no duplicated characteristic path reaches validateTopology"
    );
    let descriptor_uuids: Vec<String> = descriptors
        .iter()
        .map(|record| text(record, "uuid"))
        .collect();
    for expected in [
        "00002902-0000-1000-8000-00805f9b34fb",
        "00002901-0000-1000-8000-00805f9b34fb",
    ] {
        assert!(
            descriptor_uuids.iter().any(|uuid| uuid == expected),
            "descriptor {expected} survives the render: {descriptor_uuids:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finding_182_a_second_discover_replaces_the_snapshot() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-h10").await;
    harness.radio().set_services("peer-h10", h10_services());
    let first = harness.discover(&link).await;
    let second_response = harness
        .execute(
            "gatt.discover",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("rediscover");
    let second_handle = text(&second_response, "handle");
    assert_ne!(
        first.handle, second_handle,
        "rediscovery mints a fresh database handle"
    );
    let IpcValue::Array(second_characteristics) = field(&second_response, "characteristics") else {
        panic!("characteristics must be an array");
    };
    assert_eq!(
        second_characteristics.len(),
        12,
        "the replaced snapshot still carries every characteristic once: {second_characteristics:?}"
    );
    // The first database is stale: a read through it fails with the
    // generation identity, never with the replaced tree's data.
    let mut entries = Harness::link_entries(&link);
    entries.extend([
        ("databaseHandle", string(first.handle.clone())),
        ("databaseId", string(first.id.clone())),
        ("databaseGeneration", string(first.generation.clone())),
        (
            "characteristicHandle",
            string(
                first
                    .characteristics
                    .get("00002a37-0000-1000-8000-00805f9b34fb")
                    .expect("heart-rate measurement handle")
                    .clone(),
            ),
        ),
    ]);
    let error = harness
        .execute("gatt.read", entries, None, OpControl::unbounded())
        .await
        .expect_err("a read on the replaced database fails");
    assert_eq!(
        error.identity(),
        (
            "gatt.stale-handle",
            "gatt",
            "tauri.characteristic-database-generation".to_owned()
        ),
        "stale generation, not appended state"
    );
}

// Findings F1/F2 — one vocabulary across hosts: a successful
// without-response write reports the contract word `unknown` (the same word
// Electron/Node report for the same physical event; the contract
// `WriteReceipt` allows only `confirmed`/`unknown`), and a with-response
// write reports `confirmed`. The shared renderer decoder rejects anything
// else as `protocol.malformed`, so a Tauri-only word would surface every
// successful without-response write as a protocol error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn findings_f1_f2_write_receipt_commit_state_matches_the_contract_on_both_modes() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    let write = |correlation: &'static str, mode: &'static str| {
        let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        entries.push(("mode", string(mode)));
        harness.route("gatt.write", correlation, entries, Some(vec![0x5a; 3]))
    };
    let without_response = write("write-without-response", "without-response")
        .await
        .expect("a without-response write succeeds");
    assert_eq!(text(&without_response, "mode"), "without-response");
    assert_eq!(
        text(&without_response, "commitState"),
        "unknown",
        "the contract word for an unconfirmed write, on every host"
    );
    assert_eq!(field(&without_response, "bytesSubmitted"), &number(3));
    let with_response = write("write-with-response", "with-response")
        .await
        .expect("a with-response write succeeds");
    assert_eq!(text(&with_response, "mode"), "with-response");
    assert_eq!(text(&with_response, "commitState"), "confirmed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readiness_write_uses_the_owned_native_route_and_reports_unconfirmed_acceptance() {
    let harness = Harness::new().await;
    let link = harness.connect("peer-a").await;
    let database = harness.discover(&link).await;
    harness.radio().set_write_readiness("peer-a", true);
    let mut entries = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    entries.push(("mode", string("without-response")));
    let result = harness
        .route(
            "gatt.write-when-ready",
            "ready-write",
            entries,
            Some(vec![42]),
        )
        .await
        .expect("ready write");
    assert_eq!(field(&result, "commitState"), &string("unknown"));
    assert_eq!(field(&result, "bytesSubmitted"), &number(1));
    let mut invalid = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    invalid.push(("mode", string("with-response")));
    let error = harness
        .route(
            "gatt.write-when-ready",
            "ready-invalid",
            invalid,
            Some(vec![99]),
        )
        .await
        .expect_err("wrong mode");
    assert_eq!(error.code, BleErrorCode::ArgumentInvalid);
}

#[tokio::test]
async fn phy_snapshot_dispatch_preserves_distinct_native_directions() {
    let harness = Harness::new().await;
    let link = harness.connect("observed-phy-peer").await;
    harness.radio().set_connection_phy(
        "observed-phy-peer",
        ubm_desktop::boundary::ObservedConnectionPhy {
            tx_phy: ubm_desktop::boundary::BlePhy::Le2M,
            rx_phy: ubm_desktop::boundary::BlePhy::LeCoded,
        },
    );
    let value = harness
        .execute(
            "connection.phy",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&value, "txPhy"), "le-2m");
    assert_eq!(text(&value, "rxPhy"), "le-coded");
    assert_eq!(text(&value, "connectionId"), link.connection_id);
    assert_eq!(count(&harness.radio().calls(), "connection_phy"), 1);
}

#[tokio::test]
async fn phy_snapshot_preserves_native_refusal_and_stale_owner_admission() {
    let harness = Harness::new().await;
    let link = harness.connect("phy-unavailable-peer").await;
    let error = harness
        .execute(
            "connection.phy",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::CapabilityUnsupported);
    assert_eq!(count(&harness.radio().calls(), "connection_phy"), 1);
    harness
        .execute(
            "connection.disconnect",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert!(harness
        .execute(
            "connection.phy",
            Harness::link_entries(&link),
            None,
            OpControl::unbounded()
        )
        .await
        .is_err());
    assert_eq!(count(&harness.radio().calls(), "connection_phy"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_group_dispatcher_replays_until_ack_and_retains_cleanup() {
    let harness = Harness::new().await;
    let link = harness.connect("ordered-peer").await;
    let database = harness.discover(&link).await;
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("orderedHandle", string("ordered-test")));
    entries.push((
        "members",
        IpcValue::Array(vec![
            object([
                ("memberId", string("a")),
                (
                    "characteristicHandle",
                    string(database.characteristics[NOTIFY_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-notification")),
            ]),
            object([
                ("memberId", string("b")),
                (
                    "characteristicHandle",
                    string(database.characteristics[INDICATE_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-indication")),
            ]),
        ]),
    ));
    let opened = harness
        .execute("gatt.ordered.open", entries, None, OpControl::unbounded())
        .await
        .expect("open ordered group");
    assert_eq!(text(&opened, "state"), "native");
    let status: Value = serde_json::from_str(&text(&opened, "nativeStatus")).unwrap();
    assert!(status["setupError"].is_null(), "{status:?}");
    {
        let mut state = harness.dispatcher.inner.lock().await;
        let owner = state.callers.get_mut(&harness.key()).unwrap();
        for index in 0..255 {
            owner.ordered_groups.insert(
                format!("occupied-{index}"),
                Arc::new(tokio::sync::Mutex::new(
                    super::ordered_groups::OrderedGroup {
                        peer_id: "fixture".into(),
                        collection_id: index,
                        connection_handle: "fixture".into(),
                        open_response: None,
                        delivery: super::ordered_delivery::OrderedDelivery::default(),
                        pending_native_id: None,
                        source_closed: false,
                        released: false,
                        core_acknowledged: false,
                        core_forgotten: false,
                        last_status: None,
                        terminal_status: None,
                        terminal_status_pending: false,
                        member_ids: Vec::new(),
                        public_connection: "fixture".into(),
                        public_database: "fixture".into(),
                        orphan: super::ordered_groups::OrphanProgress::default(),
                    },
                )),
            );
        }
    }
    let mut quota_entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    quota_entries.push(("orderedHandle", string("quota-test")));
    quota_entries.push((
        "members",
        IpcValue::Array(vec![object([
            ("memberId", string("q")),
            (
                "characteristicHandle",
                string(database.characteristics[NOTIFY_ONLY].clone()),
            ),
            ("deliveryMode", string("prefer-notification")),
        ])]),
    ));
    let before = count(&harness.radio().calls(), "set_notifications");
    let error = harness
        .execute(
            "gatt.ordered.open",
            quota_entries.clone(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, BleErrorCode::StreamQuota);
    assert_eq!(count(&harness.radio().calls(), "set_notifications"), before);
    let recovered = harness
        .execute(
            "gatt.ordered.status",
            vec![("orderedHandle", string("ordered-test"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&recovered, "state"), "result");
    harness.notify("ordered-peer", NOTIFY_ONLY, vec![1]);
    harness.notify("ordered-peer", INDICATE_ONLY, vec![2]);
    let poll = |ack: Option<String>| {
        let mut entries = vec![("orderedHandle", string("ordered-test"))];
        if let Some(ack) = ack {
            entries.push(("ackDeliveryId", string(ack)));
        }
        harness.execute("gatt.ordered.next", entries, None, OpControl::unbounded())
    };
    let first = tokio::time::timeout(WAIT, async {
        loop {
            let item = poll(None).await.unwrap();
            if text(&item, "state") == "item" {
                break item;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("first value");
    let id = text(&first, "deliveryId");
    assert_eq!(
        poll(None).await.unwrap(),
        first,
        "lost response is replayed"
    );
    assert!(poll(Some("wrong".into())).await.is_err());
    let second = tokio::time::timeout(WAIT, async {
        loop {
            let item = poll(Some(id.clone())).await.unwrap();
            if text(&item, "state") == "item" {
                break item;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("second value");
    let first_wire = field(&first, "nativeWire").clone().into_wire();
    let second_wire = field(&second, "nativeWire").clone().into_wire();
    assert_eq!(first_wire["memberId"], "0");
    assert_eq!(second_wire["memberId"], "1");
    let closed = harness
        .execute(
            "gatt.ordered.close",
            vec![("orderedHandle", string("ordered-test"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_ne!(
        text(&closed, "cleanupState"),
        "released",
        "unacknowledged value stays owned"
    );
    let mut ack = Some(text(&second, "deliveryId"));
    tokio::time::timeout(WAIT, async {
        loop {
            let item = poll(ack.clone()).await.unwrap();
            if text(&item, "state") == "item" {
                ack = Some(text(&item, "deliveryId"));
            }
            let close = harness
                .execute(
                    "gatt.ordered.close",
                    vec![("orderedHandle", string("ordered-test"))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            if text(&item, "state") == "closed" && text(&close, "cleanupState") == "released" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("drain and cleanup");
    let foreign = harness.other_caller().await;
    assert!(
        foreign
            .execute(
                "gatt.ordered.forget",
                vec![("orderedHandle", string("ordered-test"))],
                None,
                OpControl::unbounded()
            )
            .await
            .is_err(),
        "foreign caller cannot claim a group receipt"
    );
    assert!(
        harness
            .execute(
                "gatt.ordered.forget",
                vec![("orderedHandle", string("never-issued"))],
                None,
                OpControl::unbounded()
            )
            .await
            .is_err(),
        "never-issued handles are refused"
    );
    let original = harness
        .with_caller(|owner| owner.ordered_groups["ordered-test"].clone())
        .await;
    let (peer, collection) = {
        let group = original.lock().await;
        (group.peer_id.clone(), group.collection_id)
    };
    harness
        .central
        .acknowledge_ordered_batch(&peer, collection)
        .await
        .unwrap();
    assert!(
        harness
            .central
            .ordered_batch_member_status(&peer, collection)
            .await
            .is_ok(),
        "lost acknowledgement response retains native receipt"
    );
    let forget = || {
        harness.execute(
            "gatt.ordered.forget",
            vec![("orderedHandle", string("ordered-test"))],
            None,
            OpControl::unbounded(),
        )
    };
    forget().await.unwrap();
    forget().await.unwrap();
    assert!(
        harness
            .central
            .ordered_batch_member_status(&peer, collection)
            .await
            .is_err(),
        "native forget reclaimed receipt"
    );
    assert_eq!(
        harness
            .with_caller(|owner| owner.ordered_groups.len())
            .await,
        256,
        "lost forget response retains caller slot"
    );
    let retire = || {
        harness.execute(
            "gatt.ordered.retire",
            vec![("orderedHandle", string("ordered-test"))],
            None,
            OpControl::unbounded(),
        )
    };
    retire().await.unwrap();
    retire().await.unwrap();
    assert_eq!(
        harness
            .with_caller(|owner| owner.ordered_groups.len())
            .await,
        255
    );
    harness
        .execute(
            "gatt.ordered.open",
            quota_entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("reclaimed slot admits another group");
    let mut quota_ack = None;
    tokio::time::timeout(WAIT, async {
        loop {
            let close = harness
                .execute(
                    "gatt.ordered.close",
                    vec![("orderedHandle", string("quota-test"))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            let mut entries = vec![("orderedHandle", string("quota-test"))];
            if let Some(ack) = quota_ack.clone() {
                entries.push(("ackDeliveryId", string(ack)));
            }
            let next = harness
                .execute("gatt.ordered.next", entries, None, OpControl::unbounded())
                .await
                .unwrap();
            if text(&next, "state") == "item" {
                quota_ack = Some(text(&next, "deliveryId"));
            }
            if text(&next, "state") == "closed" && text(&close, "cleanupState") == "released" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("second group drains");
    let second_group = harness
        .with_caller(|owner| owner.ordered_groups["quota-test"].clone())
        .await;
    let (peer, collection) = {
        let mut group = second_group.lock().await;
        harness
            .central
            .acknowledge_ordered_batch(&group.peer_id, group.collection_id)
            .await
            .unwrap();
        group.core_acknowledged = true;
        (group.peer_id.clone(), group.collection_id)
    };
    harness
        .central
        .forget_ordered_batch(&peer, collection)
        .await
        .unwrap();
    harness
        .execute(
            "gatt.ordered.forget",
            vec![("orderedHandle", string("quota-test"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .expect("retry after lost native forget response");
    assert!(
        harness
            .with_caller(|owner| owner.ordered_groups.contains_key("quota-test"))
            .await
    );
    {
        let mut state = harness.dispatcher.inner.lock().await;
        state
            .callers
            .get_mut(&harness.key())
            .unwrap()
            .ordered_groups
            .retain(|handle, _| !handle.starts_with("occupied-"));
    }
    use crate::IpcDispatcher;
    harness.dispatcher.release_caller(harness.caller.clone());
    harness
        .dispatcher
        .settle_revoked_ordered(&harness.key())
        .await;
    assert!(!harness
        .dispatcher
        .inner
        .lock()
        .await
        .callers
        .get(&harness.key())
        .is_some_and(|owner| owner.ordered_groups.contains_key("quota-test")));
    assert!(
        harness
            .dispatcher
            .inspect_ordered_orphans(&harness.caller)
            .await
            .is_empty(),
        "already acknowledged receipt is not reclassified as abandoned"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_group_cancel_before_reservation_never_enables_source() {
    let harness = Harness::new().await;
    let link = harness.connect("ordered-cancel-peer").await;
    let database = harness.discover(&link).await;
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("orderedHandle", string("ordered-cancel")));
    entries.push((
        "members",
        IpcValue::Array(vec![object([
            ("memberId", string("a")),
            (
                "characteristicHandle",
                string(database.characteristics[NOTIFY_ONLY].clone()),
            ),
            ("deliveryMode", string("prefer-notification")),
        ])]),
    ));
    let ctl = OpControl::unbounded();
    ctl.ticket.request_cancel();
    assert!(harness
        .execute("gatt.ordered.open", entries, None, ctl)
        .await
        .is_err());
    let status = harness
        .execute(
            "gatt.ordered.status",
            vec![("orderedHandle", string("ordered-cancel"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&status, "state"), "unknown");
    assert_eq!(count(&harness.radio().calls(), "set_notifications"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_group_failed_setup_preserves_shared_subscription_and_retry_owner() {
    let harness = Harness::new().await;
    let link = harness.connect("ordered-failure-peer").await;
    let database = harness.discover(&link).await;
    let subscription = harness
        .subscribe(&link, &database, NOTIFY_ONLY, None)
        .await
        .unwrap();
    let subscription_handle = text(&subscription, "handle");
    harness
        .radio()
        .fail_next(FaultOp::Subscribe, "second enable refused");
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("orderedHandle", string("ordered-failed")));
    entries.push((
        "members",
        IpcValue::Array(vec![
            object([
                ("memberId", string("a")),
                (
                    "characteristicHandle",
                    string(database.characteristics[NOTIFY_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-notification")),
            ]),
            object([
                ("memberId", string("b")),
                (
                    "characteristicHandle",
                    string(database.characteristics[INDICATE_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-indication")),
            ]),
        ]),
    ));
    let open = harness
        .execute("gatt.ordered.open", entries, None, OpControl::unbounded())
        .await
        .unwrap();
    let status: Value = serde_json::from_str(&text(&open, "nativeStatus")).unwrap();
    assert!(!status["setupError"].is_null(), "{status:?}");
    assert_eq!(status["members"][0]["acquisition"], "acquired");
    assert_ne!(status["members"][1]["acquisition"], "acquired");
    let recovered = harness
        .execute(
            "gatt.ordered.status",
            vec![("orderedHandle", string("ordered-failed"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&recovered, "state"), "result");
    let _cleanup = harness
        .execute(
            "gatt.ordered.close",
            vec![("orderedHandle", string("ordered-failed"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.notify("ordered-failure-peer", NOTIFY_ONLY, vec![42]);
    let received = harness.wait_items(&subscription_handle, 1).await;
    assert_eq!(
        value_bytes(&received[0]),
        vec![42],
        "independent subscriber still receives values"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_group_owner_release_reports_and_retains_undrained_value() {
    let harness = Harness::new().await;
    let link = harness.connect("ordered-release-peer").await;
    let database = harness.discover(&link).await;
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("orderedHandle", string("ordered-release")));
    entries.push((
        "members",
        IpcValue::Array(vec![object([
            ("memberId", string("a")),
            (
                "characteristicHandle",
                string(database.characteristics[NOTIFY_ONLY].clone()),
            ),
            ("deliveryMode", string("prefer-notification")),
        ])]),
    ));
    harness
        .execute("gatt.ordered.open", entries, None, OpControl::unbounded())
        .await
        .unwrap();
    harness.notify("ordered-release-peer", NOTIFY_ONLY, vec![9]);
    let poll = |ack: Option<String>| {
        let mut entries = vec![("orderedHandle", string("ordered-release"))];
        if let Some(ack) = ack {
            entries.push(("ackDeliveryId", string(ack)));
        }
        harness.execute("gatt.ordered.next", entries, None, OpControl::unbounded())
    };
    let item = tokio::time::timeout(WAIT, async {
        loop {
            let item = poll(None).await.unwrap();
            if text(&item, "state") == "item" {
                break item;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let cleanup = harness.dispatcher.release(&harness.key()).await;
    assert_eq!(text(&cleanup, "state"), "release-failed");
    assert_eq!(
        poll(None).await.unwrap(),
        item,
        "retired owner can recover the retained item"
    );
    let mut ack = Some(text(&item, "deliveryId"));
    tokio::time::timeout(WAIT, async {
        loop {
            let item = poll(ack.clone()).await.unwrap();
            if text(&item, "state") == "item" {
                ack = Some(text(&item, "deliveryId"));
            }
            let close = harness
                .execute(
                    "gatt.ordered.close",
                    vec![("orderedHandle", string("ordered-release"))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            if text(&item, "state") == "closed" && text(&close, "cleanupState") == "released" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("retired owner drains and releases");
    harness
        .execute(
            "gatt.ordered.forget",
            vec![("orderedHandle", string("ordered-release"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness
        .execute(
            "gatt.ordered.retire",
            vec![("orderedHandle", string("ordered-release"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        text(&harness.dispatcher.release(&harness.key()).await, "state"),
        "released"
    );
}

/// Test-only JSON-lines bridge used by the public TypeScript Tauri journey.
/// It runs the production dispatcher over FakeRadio; no OS radio is opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn ordered_public_tauri_bridge() {
    use crate::IpcDispatcher;
    use std::io::BufRead;
    assert_eq!(std::env::var("UBM_TAURI_BRIDGE").as_deref(), Ok("1"));
    fn send(value: Value) {
        println!("UBM_BRIDGE {}", value);
    }
    let radio = FakeRadio::new();
    radio.set_services("bridge-peer", vec![hrm_service()]);
    radio.set_gatt_snapshot_identity(
        "bridge-peer",
        ubm_desktop::boundary::GattSnapshotIdentity {
            owner: "fake".into(),
            attachment: 1,
            revision: 1,
        },
    );
    radio.set_mtu("bridge-peer", 185);
    let central = DesktopCentral::open(radio, "tauri-public-bridge")
        .await
        .unwrap();
    central.boundary().push_event(advertisement("bridge-peer"));
    tokio::time::timeout(WAIT, async {
        while central.peer_key_for("bridge-peer").await.is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let authority: Arc<dyn CoreAuthority> = Arc::new(central.clone());
    let dispatcher = BtleplugDispatcher::with_core_authority(authority);
    let directory = std::env::temp_dir().join(format!(
        "ubm-tauri-ordered-bridge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    dispatcher
        .continuation_configure_recording_directory(&directory)
        .await
        .unwrap();
    // The bridge is a renderer-visible IPC journey, but its durable owner is
    // deliberately installed by trusted host code, exactly as production
    // does.  No renderer path receives the directory or ledger capability.
    let terminal_ledger = dispatcher
        .recordings
        .terminal_evidence_ledger(8192)
        .unwrap();
    *dispatcher
        .terminal_evidence_ledger
        .lock()
        .expect("terminal evidence ledger mutex poisoned") = Some(Arc::new(terminal_ledger));
    let caller = AuthenticatedCaller::new("bridge-app".into(), "main".into());
    let sink = IpcEventSink::new(Channel::new(|body| {
        if let InvokeResponseBody::Json(json) = body {
            if let Ok(event) = serde_json::from_str::<Value>(&json) {
                send(serde_json::json!({"event":event}));
            }
        }
        Ok(())
    }));
    send(serde_json::json!({"ready":true}));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(command) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = command["id"].clone();
        let result = match command["kind"].as_str() {
            Some("invoke") => {
                let request = IpcValue::from_wire(command["request"].clone()).unwrap();
                let attach = command["request"]["kind"] == "bootstrap";
                dispatcher
                    .dispatch(caller.clone(), request, attach.then(|| sink.clone()))
                    .await
                    .into_wire()
            }
            Some("notify") => {
                let uuid = command["uuid"].as_str().unwrap();
                let bytes = command["bytes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| u8::try_from(value.as_u64().unwrap()).unwrap())
                    .collect();
                let epoch = central
                    .boundary()
                    .enable_epochs()
                    .into_iter()
                    .rev()
                    .find(|(key, _)| key.0 == "bridge-peer" && key.3 == uuid)
                    .unwrap()
                    .1;
                central.boundary().push_event(RadioEvent::Notification {
                    peer_id: "bridge-peer".into(),
                    service_uuid: HRM_SERVICE.into(),
                    service_occurrence: 0,
                    characteristic_uuid: uuid.into(),
                    characteristic_occurrence: 0,
                    epoch,
                    value: bytes,
                });
                serde_json::json!({"ok":true})
            }
            Some("loss") => {
                let count = command["count"].as_u64();
                match central
                    .boundary()
                    .stage_ordered_source_loss("bridge-peer", count)
                {
                    Ok(()) => serde_json::json!({"ok":true}),
                    Err(error) => serde_json::json!({"error":error.to_string()}),
                }
            }
            Some("fail-subscribe") => {
                central
                    .boundary()
                    .fail_next(FaultOp::Subscribe, "bridge enable failure");
                serde_json::json!({"ok":true})
            }
            Some("fail-notification-source") => {
                let uuid = command["uuid"].as_str().unwrap();
                let epoch = central
                    .boundary()
                    .enable_epochs()
                    .into_iter()
                    .rev()
                    .find(|(key, _)| key.0 == "bridge-peer" && key.3 == uuid)
                    .unwrap()
                    .1;
                central
                    .boundary()
                    .push_event(RadioEvent::NotificationSourceFailed {
                        peer_id: "bridge-peer".into(),
                        service_uuid: HRM_SERVICE.into(),
                        service_occurrence: 0,
                        characteristic_uuid: uuid.into(),
                        characteristic_occurrence: 0,
                        epoch,
                        error: DesktopError::new(
                            BleErrorCode::PlatformFailure,
                            ubm_core::contracts::BleErrorDomain::Gatt,
                            "gatt.notification",
                        )
                        .with_platform(PlatformDetail::new("test-radio", "terminal-source-failed")),
                    })
                    .unwrap();
                serde_json::json!({"ok":true})
            }
            Some("recording-stop") => dispatcher
                .continuation_recording_stop_ordered(command["recordingId"].as_str().unwrap())
                .await
                .unwrap(),
            Some("recording-status") => dispatcher
                .continuation_recording_status(command["recordingId"].as_str().unwrap())
                .await
                .unwrap(),
            Some("recording-prepare") => dispatcher
                .continuation_recording_prepare(
                    command["recordingId"].as_str().unwrap(),
                    128,
                    65536,
                )
                .await
                .unwrap(),
            Some("settle-marker") => {
                central.boundary().settle_marker_write(
                    command["collectionId"].as_str().unwrap().parse().unwrap(),
                    command["markerId"].as_str().unwrap().parse().unwrap(),
                );
                serde_json::json!({"ok":true})
            }
            Some("finish") => {
                send(serde_json::json!({"id":id,"result":{"ok":true}}));
                break;
            }
            _ => serde_json::json!({"error":"unknown bridge command"}),
        };
        send(serde_json::json!({"id":id,"result":result}));
    }
    drop(dispatcher);
    drop(central);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_group_revoked_caller_is_recoverable_by_trusted_host() {
    use crate::IpcDispatcher;
    let harness = Harness::new().await;
    let link = harness.connect("ordered-revoked-peer").await;
    let database = harness.discover(&link).await;
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("orderedHandle", string("ordered-revoked")));
    entries.push((
        "members",
        IpcValue::Array(vec![object([
            ("memberId", string("a")),
            (
                "characteristicHandle",
                string(database.characteristics[NOTIFY_ONLY].clone()),
            ),
            ("deliveryMode", string("prefer-notification")),
        ])]),
    ));
    harness
        .execute("gatt.ordered.open", entries, None, OpControl::unbounded())
        .await
        .unwrap();
    harness.notify("ordered-revoked-peer", NOTIFY_ONLY, vec![11]);
    let _item = tokio::time::timeout(WAIT, async {
        loop {
            let item = harness
                .execute(
                    "gatt.ordered.next",
                    vec![("orderedHandle", string("ordered-revoked"))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            if text(&item, "state") == "item" {
                break item;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let other = AuthenticatedCaller::new("other-app".into(), "main".into());
    {
        let mut state = harness.dispatcher.inner.lock().await;
        for index in 0..super::MAX_PENDING_EVENTS {
            state
                .ordered_orphans
                .push(super::ordered_groups::OrphanSlot {
                    owner: caller_key(&other),
                    id: format!("other-{index}"),
                    receipt: Some(object([("receiptId", string(format!("other-{index}")))])),
                    page_start: 0,
                    page_end: None,
                    page_done: false,
                });
        }
    }
    harness
        .radio()
        .fail_next(FaultOp::Unsubscribe, "first native cleanup refused");
    harness.dispatcher.release_caller(harness.caller.clone());
    harness
        .dispatcher
        .settle_revoked_ordered(&harness.key())
        .await;
    assert!(harness
        .dispatcher
        .inspect_ordered_orphans(&harness.caller)
        .await
        .is_empty());
    assert!(
        !harness
            .dispatcher
            .revoked_ordered_handles(&harness.caller)
            .await
            .is_empty(),
        "full receipt quota retains source debt"
    );
    harness
        .dispatcher
        .acknowledge_ordered_orphan(&other, "other-0")
        .await;
    let receipt = tokio::time::timeout(WAIT, async {
        loop {
            let receipts = harness
                .dispatcher
                .inspect_ordered_orphans(&harness.caller)
                .await;
            if let Some(receipt) = receipts.into_iter().next() {
                break receipt;
            }
            harness
                .dispatcher
                .settle_revoked_ordered(&harness.key())
                .await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(error) => {
            let group = harness
                .with_caller(|owner| owner.ordered_groups["ordered-revoked"].clone())
                .await;
            let group = group.lock().await;
            panic!(
                "sealed orphan receipt: {error:?}; values={} controls={} terminal={} closed={} pending={:?} released={} error={:?}",
                group.orphan.values,
                group.orphan.controls,
                group.orphan.terminal.is_some(),
                group.orphan.source_closed,
                group.orphan.pending_receipt_ack,
                group.released,
                group.orphan.error
            );
        }
    };
    let mut oversized = receipt.clone().into_wire();
    oversized["terminal"]["reason"] = serde_json::json!({"kind":"source-failed", "error":{
        "code":"platform.failure", "domain":"gatt", "operation":"bridge", "detail":"é".repeat(5001),
        "platformDomain":null, "platformCode":null, "platformMessage":null, "platformMetadata":{}
    }});
    let receipt = IpcValue::from_wire(oversized).unwrap();
    {
        let mut state = harness.dispatcher.inner.lock().await;
        let slot = state
            .ordered_orphans
            .iter_mut()
            .find(|slot| slot.owner == harness.key() && slot.receipt.is_some())
            .unwrap();
        slot.receipt = Some(receipt.clone());
    }
    assert!(
        count(&harness.radio().calls(), "set_notifications") >= 3,
        "failed cleanup was retried"
    );
    assert_eq!(text(&receipt, "values"), "1");
    assert_eq!(text(&receipt, "bytes"), "1");
    let wire = receipt.clone().into_wire();
    assert_eq!(wire["trustedValues"][0]["memberId"], "0");
    assert!(wire["trustedValues"][0]["sourceMembership"]["epoch"].is_string());
    assert_eq!(field(&receipt, "uncertainTail"), &IpcValue::Bool(true));
    assert!(
        !receipt.clone().into_wire().to_string().contains("[11]"),
        "receipt contains no value payload"
    );
    assert_eq!(
        harness
            .dispatcher
            .inspect_ordered_orphans(&other)
            .await
            .len(),
        super::MAX_PENDING_EVENTS - 1
    );
    let id = text(&receipt, "receiptId");
    harness
        .dispatcher
        .acknowledge_ordered_orphan(&other, &id)
        .await;
    assert_eq!(
        harness
            .dispatcher
            .inspect_ordered_orphans(&harness.caller)
            .await
            .len(),
        1
    );
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), sink, version_offer())
        .await
        .expect("replacement attachment after sealed cleanup");
    let summaries = harness
        .dispatcher
        .ordered_orphan_receipts(&harness.caller)
        .await
        .unwrap();
    let IpcValue::Array(summaries) = field(&summaries, "receipts") else {
        panic!("receipt summaries");
    };
    assert_eq!(summaries.len(), 1);
    assert_eq!(text(&summaries[0], "receiptId"), id);
    let ack_payload = || {
        [("receiptId".to_owned(), string(id.clone()))]
            .into_iter()
            .collect()
    };
    assert!(
        harness
            .dispatcher
            .ack_ordered_orphan(&harness.caller, ack_payload())
            .await
            .is_err(),
        "unread receipt cannot be acknowledged"
    );
    let mut offset = 0i64;
    let mut pages = 0usize;
    let mut assembled = String::new();
    loop {
        let payload = || {
            [
                ("receiptId".to_owned(), string(id.clone())),
                ("offset".to_owned(), number(offset)),
                ("limit".to_owned(), number(1000)),
            ]
            .into_iter()
            .collect()
        };
        let page = harness
            .dispatcher
            .ordered_orphan_page(&harness.caller, payload())
            .await
            .unwrap();
        assert_eq!(
            harness
                .dispatcher
                .ordered_orphan_page(&harness.caller, payload())
                .await
                .unwrap(),
            page,
            "lost page response replays"
        );
        assembled.push_str(&text(&page, "chunk"));
        pages += 1;
        offset = match field(&page, "nextOffset") {
            IpcValue::Number(value) => value.as_i64().unwrap(),
            _ => panic!("offset"),
        };
        if field(&page, "done") == &IpcValue::Bool(true) {
            break;
        }
    }
    assert!(pages > 5, "oversized single control spans multiple pages");
    assert_eq!(
        serde_json::from_str::<Value>(&assembled).unwrap(),
        receipt.clone().into_wire()
    );
    harness
        .dispatcher
        .ack_ordered_orphan(&harness.caller, ack_payload())
        .await
        .unwrap();
    assert!(harness
        .dispatcher
        .inspect_ordered_orphans(&harness.caller)
        .await
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_group_repeated_lifecycle_reclaims_each_slot() {
    let harness = Harness::new().await;
    let link = harness.connect("ordered-repeat-peer").await;
    let database = harness.discover(&link).await;
    for index in 0..3 {
        let handle = format!("ordered-repeat-{index}");
        let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
        entries.push(("orderedHandle", string(handle.clone())));
        entries.push((
            "members",
            IpcValue::Array(vec![object([
                ("memberId", string("a")),
                (
                    "characteristicHandle",
                    string(database.characteristics[NOTIFY_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-notification")),
            ])]),
        ));
        harness
            .execute("gatt.ordered.open", entries, None, OpControl::unbounded())
            .await
            .unwrap();
        let mut ack = None;
        tokio::time::timeout(WAIT, async {
            loop {
                let close = harness
                    .execute(
                        "gatt.ordered.close",
                        vec![("orderedHandle", string(handle.clone()))],
                        None,
                        OpControl::unbounded(),
                    )
                    .await
                    .unwrap();
                let mut entries = vec![("orderedHandle", string(handle.clone()))];
                if let Some(id) = ack.clone() {
                    entries.push(("ackDeliveryId", string(id)));
                }
                let next = harness
                    .execute("gatt.ordered.next", entries, None, OpControl::unbounded())
                    .await
                    .unwrap();
                if text(&next, "state") == "item" {
                    ack = Some(text(&next, "deliveryId"));
                }
                if text(&next, "state") == "closed" && text(&close, "cleanupState") == "released" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("drain repeated group");
        harness
            .execute(
                "gatt.ordered.forget",
                vec![("orderedHandle", string(handle.clone()))],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        harness
            .execute(
                "gatt.ordered.retire",
                vec![("orderedHandle", string(handle))],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        assert_eq!(
            harness
                .with_caller(|owner| owner.ordered_groups.len())
                .await,
            0
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_recording_stop_reports_capacity_then_finishes_after_ack() {
    use ubm_desktop::continuation_journal::JournalQuota;
    use ubm_desktop::ordered_recording::{
        OrderedRecording, OrderedRecordingDeclaration, OrderedRecordingMember,
        RecordingDriverStatus,
    };
    let module = module_path!().split_once("::").unwrap().1;
    let test = format!("{module}::ordered_recording_stop_reports_capacity_then_finishes_after_ack");
    let Some(directory) = recording_fixture::isolated_fixture_process(&test) else {
        return;
    };
    let harness = Harness::new().await;
    let link = harness.connect("ordered-recording-peer").await;
    let _database = harness.discover(&link).await;
    harness
        .dispatcher
        .continuation_configure_recording_directory(&directory)
        .await
        .unwrap();
    let peer = harness
        .central
        .peer_records()
        .await
        .into_iter()
        .find(|peer| peer.peer_id == link.peer_id)
        .unwrap();
    let lease = harness
        .with_caller(|owner| owner.connections[&link.handle].lease.clone())
        .await;
    let selector = DesktopCentral::<FakeRadio>::selector(
        HRM_SERVICE,
        Some(0),
        Some(NOTIFY_ONLY),
        Some(0),
        None,
        None,
    )
    .unwrap();
    let declaration = OrderedRecordingDeclaration {
        peer_id: link.peer_id.clone(),
        connection_generation: peer.connection_generation.unwrap(),
        database_generation: peer.database_generation.unwrap(),
        members: vec![OrderedRecordingMember {
            member_id: 1,
            public_member_id: "a".into(),
            selector,
            delivery: None,
            preference: None,
        }],
        max_items: 8,
        max_bytes: 128,
        max_controls: 8,
    };
    let recording = OrderedRecording::prepare(
        Arc::new(harness.central.clone()),
        harness.dispatcher.recordings.clone(),
        "ordered-stop".into(),
        JournalQuota {
            max_bytes: 1 << 20,
            max_records: 1,
        },
        declaration,
    )
    .await
    .unwrap();
    recording
        .start_receipt(OpControl::budget_ms(5000).with_connection_lease(lease))
        .await
        .unwrap();
    harness.notify(&link.peer_id, NOTIFY_ONLY, vec![1]);
    harness.notify(&link.peer_id, NOTIFY_ONLY, vec![2]);
    tokio::time::timeout(WAIT, async {
        while !matches!(
            recording.driver_status(),
            RecordingDriverStatus::WaitingCapacity
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recording reaches capacity");
    let legacy = harness
        .dispatcher
        .continuation_recording_stop("ordered-stop")
        .await;
    assert!(
        legacy.is_err(),
        "legacy journal stop must not claim ordered radio release"
    );
    harness.radio().fail_next(
        FaultOp::Unsubscribe,
        "scripted ordered source cleanup failure",
    );
    let pending = harness
        .dispatcher
        .continuation_recording_stop_ordered("ordered-stop")
        .await
        .unwrap();
    // Ordered stop receipts are revision 2: the linked producer persists the
    // individual failure-observation records rather than collapsing them into
    // the legacy terminal aggregate.
    assert_eq!(pending["recording"]["version"], 2);
    assert_eq!(pending["recording"]["state"], "waiting-capacity");
    assert!(pending["recording"]["error"].is_null());
    let finished = tokio::time::timeout(WAIT, async {
        loop {
            let page = harness
                .dispatcher
                .continuation_recording_prepare("ordered-stop", 16, 4096)
                .await
                .unwrap();
            if let Some(token) = page["token"].as_str() {
                harness
                    .dispatcher
                    .continuation_recording_acknowledge("ordered-stop", token)
                    .await
                    .unwrap();
            }
            let status = harness
                .dispatcher
                .continuation_recording_stop_ordered("ordered-stop")
                .await
                .unwrap();
            if status["recording"]["state"] == "finished" {
                break status;
            }
            if status["recording"]["state"] == "failed" {
                assert_eq!(status["recording"]["error"]["kind"], "source");
                assert!(status["recording"]["error"]["source"]["code"].is_string());
            } else {
                assert!(
                    matches!(
                        status["recording"]["state"].as_str(),
                        Some("waiting-capacity" | "waiting-source" | "waiting-drain")
                    ),
                    "unexpected stop: {status}"
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordered stop releases after reader acknowledgement");
    assert!(
        count(&harness.radio().calls(), "set_notifications") >= 3,
        "failed native cleanup was retried before completion"
    );
    assert!(finished["recording"]["error"].is_null());
    assert_eq!(
        harness
            .dispatcher
            .continuation_recording_stop_ordered("ordered-stop")
            .await
            .unwrap()["recording"]["state"],
        "finished"
    );
    recording_fixture::complete_fixture_process(&directory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_recording_dispatcher_retains_lease_across_caller_release() {
    let module = module_path!().split_once("::").unwrap().1;
    let test =
        format!("{module}::ordered_recording_dispatcher_retains_lease_across_caller_release");
    let Some(directory) = recording_fixture::isolated_fixture_process(&test) else {
        return;
    };
    let harness = Harness::new().await;
    harness.radio().set_gatt_snapshot_identity(
        "ordered-recording-dispatch-peer",
        ubm_desktop::boundary::GattSnapshotIdentity {
            owner: "fake".into(),
            attachment: 1,
            revision: 1,
        },
    );
    let link = harness.connect("ordered-recording-dispatch-peer").await;
    let database = harness.discover(&link).await;
    harness
        .dispatcher
        .continuation_configure_recording_directory(&directory)
        .await
        .unwrap();
    let earlier = harness
        .execute(
            "gatt.ordered-recording.reserve",
            vec![("reservationToken", string("earlier-local-token"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let earlier_handle = text(&earlier, "startHandle");
    let reserve = || vec![("reservationToken", string("recording-local-token"))];
    let reserved = harness
        .execute(
            "gatt.ordered-recording.reserve",
            reserve(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        harness
            .execute(
                "gatt.ordered-recording.reserve",
                reserve(),
                None,
                OpControl::unbounded()
            )
            .await
            .unwrap(),
        reserved
    );
    let start_handle = text(&reserved, "startHandle");
    let entries = || {
        let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
        entries.push(("startHandle", string(start_handle.clone())));
        entries.push((
            "recording",
            object([
                ("id", string("recording-dispatch")),
                ("maxBytes", number(1 << 20)),
                ("maxRecords", number(128)),
            ]),
        ));
        entries.push((
            "members",
            IpcValue::Array(vec![object([
                ("memberId", string("a")),
                (
                    "characteristicHandle",
                    string(database.characteristics[NOTIFY_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-notification")),
            ])]),
        ));
        entries
    };
    let failed_reservation = harness
        .execute(
            "gatt.ordered-recording.reserve",
            vec![("reservationToken", string("invalid-setup-token"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let failed_handle = text(&failed_reservation, "startHandle");
    let mut failed_entries = entries();
    for (key, value) in &mut failed_entries {
        if *key == "startHandle" {
            *value = string(failed_handle.clone());
        }
        if *key == "recording" {
            *value = object([
                ("id", string("recording-invalid")),
                ("maxBytes", number(1 << 20)),
                ("maxRecords", number(128)),
            ]);
        }
    }
    failed_entries.push(("streamItemCapacity", number(65537)));
    let failed = harness
        .execute(
            "gatt.ordered-recording.start",
            failed_entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(failed["kind"], "prepare-failed");
    assert!(failed.get("leaseCleanup").is_none());
    harness
        .execute(
            "gatt.ordered-recording.retire",
            vec![("startHandle", string(failed_handle))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.radio().block_op(FaultOp::OrderedOpen);
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(entries());
    let start_waiter = tokio::spawn(async move {
        dispatcher
            .execute(
                &caller,
                "gatt.ordered-recording.start",
                payload,
                None,
                OpControl::unbounded(),
            )
            .await
    });
    harness.wait_calls("ordered_open", 1).await;
    start_waiter.abort();
    let pending = harness
        .execute(
            "gatt.ordered-recording.status",
            vec![("startHandle", string(start_handle.clone()))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&pending, "state"), "pending");
    harness.radio().unblock_op(FaultOp::OrderedOpen);
    let first = tokio::time::timeout(WAIT, async {
        loop {
            let status = harness
                .execute(
                    "gatt.ordered-recording.status",
                    vec![("startHandle", string(start_handle.clone()))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            if text(&status, "state") == "result" {
                break field(&status, "result").clone();
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("lost start response is replayable");
    assert_eq!(text(&first, "kind"), "started");
    assert_eq!(first.clone().into_wire()["receipt"]["state"], "active");
    let mut earlier_entries = entries();
    for (key, value) in &mut earlier_entries {
        if *key == "startHandle" {
            *value = string(earlier_handle.clone());
        }
        if *key == "recording" {
            *value = object([
                ("id", string("recording-earlier")),
                ("maxBytes", number(1 << 20)),
                ("maxRecords", number(128)),
            ]);
        }
    }
    let earlier_start = harness
        .execute(
            "gatt.ordered-recording.start",
            earlier_entries,
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        earlier_start.into_wire()["receipt"]["state"],
        "active",
        "earlier reservation starts after later reservation"
    );
    tokio::time::timeout(WAIT, async {
        loop {
            let receipt = harness
                .dispatcher
                .continuation_recording_stop_ordered("recording-earlier")
                .await
                .unwrap();
            if receipt["recording"]["state"] == "finished"
                && receipt["hostCleanup"]["state"] == "released"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("shared-link lease releases without disconnecting the other owner");
    harness
        .execute(
            "gatt.ordered-recording.retire",
            vec![("startHandle", string(earlier_handle.clone()))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let mut stale_entries = entries();
    for (key, value) in &mut stale_entries {
        if *key == "startHandle" {
            *value = string(earlier_handle.clone());
        }
    }
    assert!(
        harness
            .execute(
                "gatt.ordered-recording.start",
                stale_entries,
                None,
                OpControl::unbounded()
            )
            .await
            .is_err(),
        "retired start handle cannot be reopened"
    );
    assert_eq!(
        harness
            .execute(
                "gatt.ordered-recording.start",
                entries(),
                None,
                OpControl::unbounded()
            )
            .await
            .unwrap(),
        first
    );
    let mut changed = entries();
    changed.push(("streamItemCapacity", number(3)));
    assert!(harness
        .execute(
            "gatt.ordered-recording.start",
            changed,
            None,
            OpControl::unbounded()
        )
        .await
        .is_err());
    let marker = harness
        .execute(
            "gatt.ordered-recording.marker.reserve",
            vec![
                ("startHandle", string(start_handle.clone())),
                ("reservationToken", string("recording-marker")),
            ],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let marker_handle = text(&marker, "markerHandle");
    let mut marker_start = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    marker_start.push(("startHandle", string(start_handle.clone())));
    marker_start.push(("markerHandle", string(marker_handle.clone())));
    marker_start.push(("withResponse", IpcValue::Bool(true)));
    harness
        .execute(
            "gatt.ordered-recording.marker.start",
            marker_start,
            Some(vec![8]),
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.wait_calls("ordered_marker_start", 1).await;
    harness.notify(&link.peer_id, NOTIFY_ONLY, vec![7]);
    tokio::time::timeout(WAIT, async {
        loop {
            let status = harness
                .dispatcher
                .continuation_recording_status("recording-dispatch")
                .await
                .unwrap();
            if status["records"].as_u64().unwrap_or(0) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("notification recorded by supervised dispatcher start");
    let recovery = harness
        .execute(
            "gatt.ordered-recording.recovery",
            vec![("startHandle", string(start_handle.clone()))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let recovery_token = text(&recovery, "recoveryToken");
    let collection: u64 = first.clone().into_wire()["receipt"]["collectionId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let original_marker = || {
        vec![
            ("startHandle", string(start_handle.clone())),
            ("markerHandle", string(marker_handle.clone())),
        ]
    };
    tokio::time::timeout(WAIT, async {
        loop {
            let status = harness
                .execute(
                    "gatt.ordered-recording.marker.status",
                    original_marker(),
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap()
                .into_wire();
            if status["status"]["state"] == "admitted" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("original marker admitted");
    harness.radio().settle_marker_write(collection, 0);
    tokio::time::timeout(WAIT, async {
        loop {
            let _status = harness
                .execute(
                    "gatt.ordered-recording.marker.status",
                    original_marker(),
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            if harness
                .execute(
                    "gatt.ordered-recording.marker.forget",
                    original_marker(),
                    None,
                    OpControl::unbounded(),
                )
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("original marker settled before handoff");
    crate::IpcDispatcher::release_caller(&harness.dispatcher, harness.caller.clone());
    tokio::time::timeout(WAIT, async {
        loop {
            if !harness
                .dispatcher
                .inner
                .lock()
                .await
                .callers
                .contains_key(&harness.key())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("caller released while recording lease stays owned");
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), sink, version_offer())
        .await
        .unwrap();
    let (new_lease, new_generation) = harness
        .with_caller(|owner| (owner.lease_id.clone(), owner.lease_generation.clone()))
        .await;
    let replacement_payload = |entries: Vec<(&str, IpcValue)>| {
        let mut payload: BTreeMap<String, IpcValue> = entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        payload.insert("__expectedLeaseId".into(), string(new_lease.clone()));
        payload.insert(
            "__expectedLeaseGeneration".into(),
            string(new_generation.clone()),
        );
        payload
    };
    let connected = harness
        .dispatcher
        .execute(
            &harness.caller,
            "connection.connect",
            replacement_payload(vec![("peerId", string(link.peer_id.clone()))]),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let replacement = Link {
        handle: text(&connected, "handle"),
        peer_id: link.peer_id.clone(),
        connection_id: text(&connected, "connectionId"),
        owner_lease_id: text(&connected, "ownerLeaseId"),
        generation: text(&connected, "connectionGeneration"),
    };
    let discovered = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.discover",
            replacement_payload(Harness::link_entries(&replacement)),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let IpcValue::Array(records) = field(&discovered, "characteristics") else {
        panic!("replacement characteristics");
    };
    let replacement_database = Database {
        handle: text(&discovered, "handle"),
        id: text(&discovered, "databaseId"),
        generation: text(&discovered, "databaseGeneration"),
        characteristics: records
            .iter()
            .map(|record| (text(record, "characteristicUuid"), text(record, "handle")))
            .collect(),
    };
    let mut reserve_reclaim =
        Harness::gatt_entries(&replacement, &replacement_database, CONTROL_POINT);
    reserve_reclaim.push(("recoveryToken", string(recovery_token.clone())));
    reserve_reclaim.push(("callerKey", string("replacement-request")));
    let foreign = AuthenticatedCaller::new("foreign-app".into(), "main".into());
    assert!(
        harness
            .dispatcher
            .ordered_reclaim_route(
                &foreign,
                "gatt.ordered-recording.reclaim.reserve",
                replacement_payload(reserve_reclaim.clone())
            )
            .await
            .is_err(),
        "foreign window cannot claim recovery capability"
    );
    let reserved = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.reserve",
            replacement_payload(reserve_reclaim.clone()),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(
        harness
            .dispatcher
            .execute(
                &harness.caller,
                "gatt.ordered-recording.reclaim.reserve",
                replacement_payload(reserve_reclaim),
                None,
                OpControl::unbounded()
            )
            .await
            .unwrap(),
        reserved
    );
    let reclaim_handle = text(&reserved, "reclaimHandle");
    let reclaim_payload =
        || replacement_payload(vec![("reclaimHandle", string(reclaim_handle.clone()))]);
    harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.start",
            reclaim_payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let status = harness
                .dispatcher
                .execute(
                    &harness.caller,
                    "gatt.ordered-recording.reclaim.status",
                    reclaim_payload(),
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap()
                .into_wire();
            if status["status"]["state"] == "ready" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("replacement reclaim ready");
    let committed = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.commit",
            reclaim_payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap()
        .into_wire();
    // Publishing the replacement mapping consumes the native permit and
    // starts its asynchronous release. `commit` returns a post-action
    // snapshot, so a fast host may have completed that release before the
    // status is read. Both states truthfully retain the committed mapping;
    // the explicit release/status sequence below still proves eventual
    // cleanup rather than relying on scheduler timing.
    assert!(matches!(
        committed["status"]["state"].as_str(),
        Some("releasing" | "released")
    ));
    assert_eq!(committed["status"]["mappingCommitted"], true);
    harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.release",
            reclaim_payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let status = harness
                .dispatcher
                .execute(
                    &harness.caller,
                    "gatt.ordered-recording.reclaim.status",
                    reclaim_payload(),
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap()
                .into_wire();
            if status["status"]["state"] == "released" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("replacement reclaim released");
    for action in [
        "retire",
        "ack-retirement",
        "forget-retirement",
        "confirm-forgotten",
    ] {
        let command = format!("gatt.ordered-recording.reclaim.{action}");
        harness
            .dispatcher
            .execute(
                &harness.caller,
                &command,
                reclaim_payload(),
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
    }
    let mut lookup = Harness::gatt_entries(&replacement, &replacement_database, CONTROL_POINT);
    lookup.push(("recordingId", string("recording-dispatch")));
    lookup.push(("reclaimHandle", string(reclaim_handle)));
    let marker_owner = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.marker-owner",
            replacement_payload(lookup),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&marker_owner, "startHandle"), start_handle);
    let mut revoked_reserve =
        Harness::gatt_entries(&replacement, &replacement_database, CONTROL_POINT);
    revoked_reserve.push(("recoveryToken", string(recovery_token)));
    revoked_reserve.push(("callerKey", string("revoked-request")));
    let revoked = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.reserve",
            replacement_payload(revoked_reserve),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let revoked_handle = text(&revoked, "reclaimHandle");
    harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.start",
            replacement_payload(vec![("reclaimHandle", string(revoked_handle.clone()))]),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let status = harness
                .dispatcher
                .execute(
                    &harness.caller,
                    "gatt.ordered-recording.reclaim.status",
                    replacement_payload(vec![("reclaimHandle", string(revoked_handle.clone()))]),
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap()
                .into_wire();
            if status["status"]["state"] == "ready" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("revoked reclaim acquired native permit");
    let release_count = count(&harness.radio().calls(), "reclaim_release");
    harness.radio().block_op(FaultOp::ReclaimRelease);
    crate::IpcDispatcher::release_caller(&harness.dispatcher, harness.caller.clone());
    harness
        .wait_calls("reclaim_release", release_count + 1)
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        harness
            .dispatcher
            .inner
            .lock()
            .await
            .callers
            .contains_key(&harness.key()),
        "pending native release retains caller cleanup debt"
    );
    harness.radio().unblock_op(FaultOp::ReclaimRelease);
    tokio::time::timeout(WAIT, async {
        loop {
            if !harness
                .dispatcher
                .inner
                .lock()
                .await
                .callers
                .contains_key(&harness.key())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host retries revoked reclaim cleanup without a client");
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), sink, version_offer())
        .await
        .unwrap();
    let (lease, generation) = harness
        .with_caller(|owner| (owner.lease_id.clone(), owner.lease_generation.clone()))
        .await;
    let recovered_payload = || {
        [
            ("__expectedLeaseId".to_owned(), string(lease.clone())),
            (
                "__expectedLeaseGeneration".to_owned(),
                string(generation.clone()),
            ),
            ("reclaimHandle".to_owned(), string(revoked_handle.clone())),
        ]
        .into_iter()
        .collect()
    };
    let recovered = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reclaim.pending",
            recovered_payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap()
        .into_wire();
    assert!(recovered["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["reclaimHandle"] == revoked_handle));
    for action in [
        "retire",
        "ack-retirement",
        "forget-retirement",
        "confirm-forgotten",
    ] {
        let command = format!("gatt.ordered-recording.reclaim.{action}");
        harness
            .dispatcher
            .execute(
                &harness.caller,
                &command,
                recovered_payload(),
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
    }
    harness
        .radio()
        .fail_next(FaultOp::Disconnect, "recording lease release refused once");
    tokio::time::timeout(WAIT, async {
        loop {
            let receipt = harness
                .dispatcher
                .continuation_recording_stop_ordered("recording-dispatch")
                .await
                .unwrap();
            if receipt["hostCleanup"]["state"] == "release-failed" {
                assert_eq!(
                    receipt["hostCleanup"]["failures"][0]["resourceKind"],
                    "ordered-recording-connection-lease"
                );
                break;
            }
            assert_ne!(
                receipt["hostCleanup"]["state"], "released",
                "failed lease release is reported"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("lease failure reported");
    let before = count(&harness.radio().calls(), "disconnect");
    harness.radio().block_op(FaultOp::Disconnect);
    let dispatcher = harness.dispatcher.clone();
    let waiter = tokio::spawn(async move {
        dispatcher
            .continuation_recording_stop_ordered("recording-dispatch")
            .await
    });
    harness.wait_calls("disconnect", before + 1).await;
    waiter.abort();
    harness.radio().unblock_op(FaultOp::Disconnect);
    let stopped = tokio::time::timeout(WAIT, async {
        loop {
            let receipt = harness
                .dispatcher
                .continuation_recording_stop_ordered("recording-dispatch")
                .await
                .unwrap();
            if receipt["recording"]["state"] == "finished"
                && receipt["hostCleanup"]["state"] == "released"
            {
                break receipt;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("detached lease release survives lost waiter");
    assert_eq!(stopped["recording"]["state"], "finished");
    assert_eq!(stopped["hostCleanup"]["state"], "released");
    assert_eq!(
        harness
            .dispatcher
            .continuation_recording_stop_ordered("recording-dispatch")
            .await
            .unwrap(),
        stopped
    );
    let batch = harness
        .dispatcher
        .continuation_recording_prepare("recording-dispatch", 128, 65536)
        .await
        .unwrap();
    assert!(
        batch["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["record"]["kind"] == "value"
                && entry["record"]["bytes"] == serde_json::json!([7])),
        "recorded source value remains readable"
    );
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), sink, version_offer())
        .await
        .unwrap();
    let (lease_id, lease_generation) = harness
        .with_caller(|owner| (owner.lease_id.clone(), owner.lease_generation.clone()))
        .await;
    let payload = || {
        [
            ("startHandle".to_owned(), string(start_handle.clone())),
            ("__expectedLeaseId".to_owned(), string(lease_id.clone())),
            (
                "__expectedLeaseGeneration".to_owned(),
                string(lease_generation.clone()),
            ),
        ]
        .into_iter()
        .collect()
    };
    let recovery_payload = || {
        [
            ("__expectedLeaseId".to_owned(), string(lease_id.clone())),
            (
                "__expectedLeaseGeneration".to_owned(),
                string(lease_generation.clone()),
            ),
        ]
        .into_iter()
        .collect()
    };
    let recovered = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.starts",
            recovery_payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(recovered["starts"][0]["startHandle"], start_handle);
    assert_eq!(recovered["starts"][0]["recordingId"], "recording-dispatch");
    let foreign = AuthenticatedCaller::new("other-app".into(), "main".into());
    let isolated = harness
        .dispatcher
        .recover_ordered_recording_starts(&foreign, Default::default())
        .await
        .unwrap()
        .into_wire();
    assert_eq!(isolated["starts"], serde_json::json!([]));
    {
        let mut slots = harness.dispatcher.ordered_recording_starts.lock().await;
        let slot = slots
            .get(&(harness.key(), start_handle.clone()))
            .unwrap()
            .clone();
        for id in 10000..10070 {
            slots.insert((harness.key(), id.to_string()), slot.clone());
        }
    }
    let page = harness
        .dispatcher
        .recover_ordered_recording_starts(&harness.caller, Default::default())
        .await
        .unwrap()
        .into_wire();
    assert_eq!(page["starts"].as_array().unwrap().len(), 64);
    assert_eq!(page["truncated"], true);
    let cursor = page["nextCursor"].as_str().unwrap();
    let after = [("after".to_owned(), string(cursor))].into_iter().collect();
    let rest = harness
        .dispatcher
        .recover_ordered_recording_starts(&harness.caller, after)
        .await
        .unwrap()
        .into_wire();
    assert_eq!(rest["starts"].as_array().unwrap().len(), 7);
    assert_eq!(rest["truncated"], false);
    harness
        .dispatcher
        .ordered_recording_starts
        .lock()
        .await
        .retain(|(_, handle), _| handle.parse::<u64>().unwrap() < 10000);
    let status = harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.status",
            payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&status, "state"), "result");
    assert_eq!(field(&status, "result"), &first);
    harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.retire",
            payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.retire",
            payload(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    recording_fixture::complete_fixture_process(&directory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_recording_unused_reservations_are_reclaimed_on_revoke() {
    let harness = Harness::new().await;
    for index in 0..(super::MAX_PENDING_EVENTS + 1) {
        let (lease, generation) = harness
            .with_caller(|owner| (owner.lease_id.clone(), owner.lease_generation.clone()))
            .await;
        let payload = [
            (
                "reservationToken".to_owned(),
                string(format!("unused-{index}")),
            ),
            ("__expectedLeaseId".to_owned(), string(lease)),
            ("__expectedLeaseGeneration".to_owned(), string(generation)),
        ]
        .into_iter()
        .collect();
        harness
            .dispatcher
            .execute(
                &harness.caller,
                "gatt.ordered-recording.reserve",
                payload,
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        crate::IpcDispatcher::release_caller(&harness.dispatcher, harness.caller.clone());
        tokio::time::timeout(WAIT, async {
            loop {
                if !harness
                    .dispatcher
                    .inner
                    .lock()
                    .await
                    .callers
                    .contains_key(&harness.key())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
        harness
            .dispatcher
            .bootstrap(harness.caller.clone(), sink, version_offer())
            .await
            .unwrap();
    }
    let guard = harness
        .dispatcher
        .ordered_recording_reservations
        .lock()
        .await;
    let (lease, generation) = harness
        .with_caller(|owner| (owner.lease_id.clone(), owner.lease_generation.clone()))
        .await;
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = [
        ("reservationToken".to_owned(), string("racing")),
        ("__expectedLeaseId".to_owned(), string(lease)),
        ("__expectedLeaseGeneration".to_owned(), string(generation)),
    ]
    .into_iter()
    .collect();
    let racing = tokio::spawn(async move {
        dispatcher
            .execute(
                &caller,
                "gatt.ordered-recording.reserve",
                payload,
                None,
                OpControl::unbounded(),
            )
            .await
    });
    tokio::task::yield_now().await;
    crate::IpcDispatcher::release_caller(&harness.dispatcher, harness.caller.clone());
    drop(guard);
    assert!(
        racing.await.unwrap().is_err(),
        "revocation wins before reservation admission"
    );
    tokio::time::timeout(WAIT, async {
        loop {
            if !harness
                .dispatcher
                .inner
                .lock()
                .await
                .callers
                .contains_key(&harness.key())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let sink = IpcEventSink::new(Channel::new(|_| Ok(())));
    harness
        .dispatcher
        .bootstrap(harness.caller.clone(), sink, version_offer())
        .await
        .unwrap();
    let (lease, generation) = harness
        .with_caller(|owner| (owner.lease_id.clone(), owner.lease_generation.clone()))
        .await;
    let payload = [
        ("reservationToken".to_owned(), string("after-churn")),
        ("__expectedLeaseId".to_owned(), string(lease)),
        ("__expectedLeaseGeneration".to_owned(), string(generation)),
    ]
    .into_iter()
    .collect();
    harness
        .dispatcher
        .execute(
            &harness.caller,
            "gatt.ordered-recording.reserve",
            payload,
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_recording_worker_panic_replays_failure_and_retains_cleanup() {
    let module = module_path!().split_once("::").unwrap().1;
    let test =
        format!("{module}::ordered_recording_worker_panic_replays_failure_and_retains_cleanup");
    let Some(directory) = recording_fixture::isolated_fixture_process(&test) else {
        return;
    };
    let harness = Harness::new().await;
    let link = harness.connect("ordered-recording-panic-peer").await;
    let database = harness.discover(&link).await;
    harness
        .dispatcher
        .continuation_configure_recording_directory(&directory)
        .await
        .unwrap();
    for phase in 1..=3 {
        let reserved = harness
            .execute(
                "gatt.ordered-recording.reserve",
                vec![("reservationToken", string(format!("panic-{phase}")))],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        let handle = text(&reserved, "startHandle");
        let id = format!("panic-{phase}");
        let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
        entries.push(("startHandle", string(handle.clone())));
        entries.push((
            "recording",
            object([
                ("id", string(id.clone())),
                ("maxBytes", number(1 << 20)),
                ("maxRecords", number(128)),
            ]),
        ));
        entries.push((
            "members",
            IpcValue::Array(vec![object([
                ("memberId", string("a")),
                (
                    "characteristicHandle",
                    string(database.characteristics[NOTIFY_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-notification")),
            ])]),
        ));
        harness
            .dispatcher
            .ordered_recording_panic_phase
            .store(phase, std::sync::atomic::Ordering::SeqCst);
        let result = harness
            .execute(
                "gatt.ordered-recording.start",
                entries,
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        harness
            .dispatcher
            .ordered_recording_panic_phase
            .store(0, std::sync::atomic::Ordering::SeqCst);
        let raw = result.clone().into_wire();
        assert_eq!(
            if phase >= 2 {
                &raw["error"]["kind"]
            } else {
                &raw["receipt"]["error"]["kind"]
            },
            "worker-failed"
        );
        assert_eq!(
            raw["kind"],
            if phase >= 2 {
                "owner-failed"
            } else {
                "prepare-failed"
            }
        );
        let status = harness
            .execute(
                "gatt.ordered-recording.status",
                vec![("startHandle", string(handle.clone()))],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        assert_eq!(
            field(&status, "result"),
            &result,
            "lost result replays terminal failure"
        );
        let stopped = tokio::time::timeout(WAIT, async {
            loop {
                let stopped = harness
                    .dispatcher
                    .continuation_recording_stop_ordered(&id)
                    .await
                    .unwrap();
                if matches!(
                    stopped["hostCleanup"]["state"].as_str(),
                    Some("released" | "not-applicable")
                ) {
                    break stopped;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("panic cleanup remains retryable");
        assert_eq!(
            stopped["hostCleanup"]["state"],
            if phase < 3 {
                "not-applicable"
            } else {
                "released"
            }
        );
        harness
            .execute(
                "gatt.ordered-recording.retire",
                vec![("startHandle", string(handle))],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
    }
    recording_fixture::complete_fixture_process(&directory);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_recording_start_racing_revoke_has_no_source_effect() {
    let harness = Harness::new().await;
    let link = harness.connect("ordered-revocation-race-peer").await;
    let database = harness.discover(&link).await;
    let reserved = harness
        .execute(
            "gatt.ordered-recording.reserve",
            vec![("reservationToken", string("race-start"))],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let handle = text(&reserved, "startHandle");
    let mut entries = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    entries.push(("startHandle", string(handle)));
    entries.push((
        "recording",
        object([
            ("id", string("race-start")),
            ("maxBytes", number(1 << 20)),
            ("maxRecords", number(128)),
        ]),
    ));
    entries.push((
        "members",
        IpcValue::Array(vec![object([
            ("memberId", string("a")),
            (
                "characteristicHandle",
                string(database.characteristics[NOTIFY_ONLY].clone()),
            ),
            ("deliveryMode", string("prefer-notification")),
        ])]),
    ));
    let guard = harness
        .dispatcher
        .ordered_recording_reservations
        .lock()
        .await;
    let dispatcher = harness.dispatcher.clone();
    let caller = harness.caller.clone();
    let payload = Harness::lease_payload(entries);
    let start = tokio::spawn(async move {
        dispatcher
            .execute(
                &caller,
                "gatt.ordered-recording.start",
                payload,
                None,
                OpControl::unbounded(),
            )
            .await
    });
    tokio::task::yield_now().await;
    crate::IpcDispatcher::release_caller(&harness.dispatcher, harness.caller.clone());
    drop(guard);
    assert!(start.await.unwrap().is_err());
    assert_eq!(count(&harness.radio().calls(), "ordered_open"), 0);
    assert_eq!(count(&harness.radio().calls(), "set_notifications"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordered_marker_route_preserves_transaction_and_fifo_credit() {
    let harness = Harness::new().await;
    harness.radio().set_gatt_snapshot_identity(
        "ordered-marker-route-peer",
        ubm_desktop::boundary::GattSnapshotIdentity {
            owner: "fake".into(),
            attachment: 1,
            revision: 1,
        },
    );
    let link = harness.connect("ordered-marker-route-peer").await;
    let database = harness.discover(&link).await;
    let mut open = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
    open.push(("orderedHandle", string("marker-route")));
    open.push((
        "members",
        IpcValue::Array(vec![object([
            ("memberId", string("a")),
            (
                "characteristicHandle",
                string(database.characteristics[NOTIFY_ONLY].clone()),
            ),
            ("deliveryMode", string("prefer-notification")),
        ])]),
    ));
    let opened = harness
        .execute("gatt.ordered.open", open, None, OpControl::unbounded())
        .await
        .unwrap();
    let collection: u64 = text(&opened, "collectionId").parse().unwrap();
    let reservation = harness
        .execute(
            "gatt.ordered.marker.reserve",
            vec![
                ("orderedHandle", string("marker-route")),
                ("reservationToken", string("local-marker")),
            ],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let marker_handle = text(&reservation, "markerHandle");
    assert_eq!(
        harness
            .execute(
                "gatt.ordered.marker.reserve",
                vec![
                    ("orderedHandle", string("marker-route")),
                    ("reservationToken", string("local-marker"))
                ],
                None,
                OpControl::unbounded()
            )
            .await
            .unwrap(),
        reservation
    );
    let mut marker = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    marker.push(("orderedHandle", string("marker-route")));
    marker.push(("markerHandle", string(marker_handle.clone())));
    marker.push(("withResponse", IpcValue::Bool(true)));
    let first = harness
        .execute(
            "gatt.ordered.marker.start",
            marker.clone(),
            Some(vec![1, 2]),
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert_eq!(text(&first, "statusFormat"), "native");
    assert!(matches!(
        first.clone().into_wire()["status"]["state"].as_str(),
        Some("pending" | "admitted")
    ));
    harness
        .execute(
            "gatt.ordered.marker.start",
            marker.clone(),
            Some(vec![1, 2]),
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    assert!(harness
        .execute(
            "gatt.ordered.marker.start",
            marker,
            Some(vec![9]),
            OpControl::unbounded()
        )
        .await
        .is_err());
    let args = || {
        vec![
            ("orderedHandle", string("marker-route")),
            ("markerHandle", string(marker_handle.clone())),
        ]
    };
    assert!(harness
        .execute(
            "gatt.ordered.marker.forget",
            args(),
            None,
            OpControl::unbounded()
        )
        .await
        .is_err());
    let item = tokio::time::timeout(WAIT, async {
        loop {
            let next = harness
                .execute(
                    "gatt.ordered.next",
                    vec![("orderedHandle", string("marker-route"))],
                    None,
                    OpControl::unbounded(),
                )
                .await
                .unwrap();
            if text(&next, "state") == "item" {
                break next;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(item.clone().into_wire()["nativeWire"]["kind"], "marker");
    let ack = text(&item, "deliveryId");
    harness
        .execute(
            "gatt.ordered.next",
            vec![
                ("orderedHandle", string("marker-route")),
                ("ackDeliveryId", string(ack)),
            ],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness.radio().settle_marker_write(collection, 0);
    let status = harness
        .execute(
            "gatt.ordered.marker.status",
            args(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap()
        .into_wire();
    assert_eq!(status["status"]["write"]["state"], "settled");
    harness
        .execute(
            "gatt.ordered.marker.forget",
            args(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    harness
        .execute(
            "gatt.ordered.marker.forget",
            args(),
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let reservation = harness
        .execute(
            "gatt.ordered.marker.reserve",
            vec![
                ("orderedHandle", string("marker-route")),
                ("reservationToken", string("cancelled-marker")),
            ],
            None,
            OpControl::unbounded(),
        )
        .await
        .unwrap();
    let mut cancelled = Harness::gatt_entries(&link, &database, CONTROL_POINT);
    cancelled.push(("orderedHandle", string("marker-route")));
    cancelled.push(("markerHandle", string(text(&reservation, "markerHandle"))));
    cancelled.push(("withResponse", IpcValue::Bool(true)));
    let ctl = OpControl::unbounded();
    ctl.ticket.request_cancel();
    let before = count(&harness.radio().calls(), "ordered_marker_start");
    let result = harness
        .execute("gatt.ordered.marker.start", cancelled, Some(vec![9]), ctl)
        .await;
    if let Ok(result) = result {
        assert_eq!(result.into_wire()["status"]["state"], "not-admitted");
    }
    assert_eq!(
        count(&harness.radio().calls(), "ordered_marker_start"),
        before,
        "pre-cancel never starts native write"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_marker_transaction_is_accounted_before_orphan_ack() {
    use crate::IpcDispatcher;
    for before_admission in [true, false] {
        let harness = Harness::new().await;
        let peer = if before_admission {
            "marker-revoke-before"
        } else {
            "marker-revoke-after"
        };
        harness.radio().set_gatt_snapshot_identity(
            peer,
            ubm_desktop::boundary::GattSnapshotIdentity {
                owner: "fake".into(),
                attachment: 1,
                revision: 1,
            },
        );
        let link = harness.connect(peer).await;
        let database = harness.discover(&link).await;
        let mut open = Harness::gatt_entries(&link, &database, NOTIFY_ONLY);
        open.push(("orderedHandle", string("marker-revoked")));
        open.push((
            "members",
            IpcValue::Array(vec![object([
                ("memberId", string("a")),
                (
                    "characteristicHandle",
                    string(database.characteristics[NOTIFY_ONLY].clone()),
                ),
                ("deliveryMode", string("prefer-notification")),
            ])]),
        ));
        let opened = harness
            .execute("gatt.ordered.open", open, None, OpControl::unbounded())
            .await
            .unwrap();
        let collection: u64 = text(&opened, "collectionId").parse().unwrap();
        let reserve = harness
            .execute(
                "gatt.ordered.marker.reserve",
                vec![
                    ("orderedHandle", string("marker-revoked")),
                    ("reservationToken", string("dead-marker")),
                ],
                None,
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        let marker_handle = text(&reserve, "markerHandle");
        let gate = if before_admission {
            FaultOp::OrderedMarkerBeforeAdmission
        } else {
            FaultOp::OrderedMarkerStart
        };
        let call = if before_admission {
            "ordered_marker_pending"
        } else {
            "ordered_marker_start"
        };
        harness.radio().block_op(gate);
        let mut start = Harness::gatt_entries(&link, &database, CONTROL_POINT);
        start.push(("orderedHandle", string("marker-revoked")));
        start.push(("markerHandle", string(marker_handle.clone())));
        start.push(("withResponse", IpcValue::Bool(true)));
        harness
            .execute(
                "gatt.ordered.marker.start",
                start,
                Some(vec![12]),
                OpControl::unbounded(),
            )
            .await
            .unwrap();
        harness.wait_calls(call, 1).await;
        harness.dispatcher.release_caller(harness.caller.clone());
        harness
            .dispatcher
            .settle_revoked_ordered(&harness.key())
            .await;
        let early = harness
            .dispatcher
            .inspect_ordered_orphans(&harness.caller)
            .await;
        if before_admission {
            if let Some(receipt) = early.first() {
                assert_eq!(
                    receipt.clone().into_wire()["markerWrites"][0]["status"]["state"],
                    "not-admitted"
                );
                assert_eq!(count(&harness.radio().calls(), "ordered_marker_start"), 0);
            }
        } else {
            assert!(
                early.is_empty(),
                "an admitted unsettled write cannot produce a complete receipt"
            );
        }
        harness.radio().unblock_op(gate);
        if !before_admission {
            harness.radio().settle_marker_write(collection, 0);
        }
        let receipt = tokio::time::timeout(WAIT, async {
            loop {
                harness.radio().settle_marker_write(collection, 0);
                harness
                    .dispatcher
                    .settle_revoked_ordered(&harness.key())
                    .await;
                if let Some(receipt) = harness
                    .dispatcher
                    .inspect_ordered_orphans(&harness.caller)
                    .await
                    .into_iter()
                    .next()
                {
                    break receipt;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("marker cleanup and source seal complete");
        let raw = receipt.clone().into_wire();
        assert_eq!(raw["markerWrites"].as_array().unwrap().len(), 1);
        assert_eq!(raw["markerWrites"][0]["markerHandle"], marker_handle);
        assert!(
            !raw.to_string().contains("[12]"),
            "orphan evidence contains no write payload"
        );
        assert_eq!(
            harness
                .dispatcher
                .inspect_ordered_orphans(&harness.caller)
                .await,
            vec![receipt],
            "lost inspection replays retained receipt"
        );
        let receipt_id = raw["receiptId"].as_str().unwrap();
        harness
            .dispatcher
            .acknowledge_ordered_orphan(&harness.caller, receipt_id)
            .await;
        assert!(harness
            .dispatcher
            .inspect_ordered_orphans(&harness.caller)
            .await
            .is_empty());
    }
}
