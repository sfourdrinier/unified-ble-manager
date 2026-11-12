//! Test-only, pre-process fixture writer for the Apple recovery harness.
//!
//! It writes ordinary durable outbox state and exits before the Swift process
//! starts.  The Swift test therefore proves production reopen/attachment; it
//! never gains a test-only authority or a way to manufacture a recovery owner.

use std::path::Path;
use std::sync::Arc;

use ubm_core::oversized_evidence::{
    EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureLimits,
    ScanFailureMember, ScanFailureOccurrence,
};
use ubm_desktop::continuation_journal::{ContinuationJournal, JournalQuota};
use ubm_desktop::continuation_outbox::{Outbox, WakeSink};

struct FixtureWake;

impl WakeSink for FixtureWake {
    fn wake(&self, _: u64) {}
}

fn limits(large: bool, raw: bool) -> ScanFailureLimits {
    ScanFailureLimits {
        evidence: EvidenceLimits {
            max_depth: 8,
            max_tokens: 32,
            max_encoded_bytes: if raw {
                1
            } else if large {
                128 * 1024
            } else {
                4096
            },
            max_chunks: 8,
            chunk_bytes: if large { 64 * 1024 } else { 64 },
        },
        max_recipients: 1,
        max_bookkeeping_bytes: if large { 128 * 1024 } else { 4096 },
    }
}

fn occurrence(index: u64, large: bool) -> ScanFailureOccurrence {
    // Recovery evidence is a three-field tuple. Keep this wire invariant
    // rather than using an arbitrary root merely because the fixture is
    // test-only: raw-token indexes below are part of the bridge assertion.
    let mut tokens = vec![EvidenceToken::Array(3)];
    if large {
        tokens.push(EvidenceToken::Array(9));
        tokens.extend([
            EvidenceToken::Null,
            EvidenceToken::Bool(true),
            EvidenceToken::I64(-2),
            EvidenceToken::U64(3),
            EvidenceToken::F64Bits(0x3ff0_0000_0000_0000),
            EvidenceToken::Map(0),
            EvidenceToken::Array(0),
            EvidenceToken::Text("apple-fixture".to_owned()),
            EvidenceToken::Bytes(vec![0x5a; 64 * 1024]),
        ]);
        tokens.extend([EvidenceToken::Null, EvidenceToken::Null]);
    } else {
        tokens.extend([
            EvidenceToken::Bytes(vec![0x5a]),
            EvidenceToken::Null,
            EvidenceToken::Null,
        ]);
    }
    ScanFailureOccurrence::new(
        ScanFailureIdentity {
            host: [1; 16],
            scan: [2; 16],
            start: [3; 16],
            source_generation: [4; 16],
            occurrence: index
                .to_be_bytes()
                .repeat(2)
                .try_into()
                .expect("16-byte occurrence"),
        },
        vec![ScanFailureMember {
            recipient: [0x11; 16],
            delivery: [0x22; 16],
        }],
        Arc::new(EvidenceSource::new(tokens)),
    )
}

fn seed(directory: &Path, recording_id: &str, owners: usize, large: bool) {
    let raw = recording_id == "apple-recovery-raw";
    let journal = Arc::new(
        ContinuationJournal::open(
            &directory.join(format!("{recording_id}.sqlite")),
            recording_id,
            &serde_json::json!({"version": 1, "source": "apple-fixture"}),
            JournalQuota {
                max_bytes: 128 * 1024 * 1024,
                max_records: 4096,
            },
        )
        .expect("create fixture journal"),
    );
    let outbox = Outbox::new(0x4150_504c_455f_5245, Arc::new(FixtureWake));
    outbox
        .configure_scan_failure_recovery_authority([0x51; 32])
        .expect("configure recovery authority");
    outbox
        .attach_journal(
            Arc::clone(&journal),
            serde_json::json!({"kind": "apple-fixture"}),
        )
        .expect("attach fixture journal");
    for index in 0..owners {
        let mut reservation = outbox
            .reserve_scan_failure(limits(large, raw))
            .expect("reserve fixture owner");
        let mut source = Some(occurrence(
            u64::try_from(index + 1).expect("fixture ordinal"),
            large,
        ));
        let committed = outbox.commit_scan_failure(&mut reservation, &mut source);
        if raw {
            assert!(
                committed.is_err() && source.is_none(),
                "raw fixture must remain unprepared"
            );
        } else {
            committed.expect("commit fixture owner");
        }
        // Each persisted source is a complete producer cut. Leaving an
        // earlier reservation open makes it ineligible for raw recovery and
        // does not model a durable post-crash fixture.
        outbox
            .finish_scan_failure_producer(&mut reservation)
            .expect("finish fixture producer");
    }
}

fn main() {
    let directory = std::env::args().nth(1).expect("fixture directory argument");
    let directory = Path::new(&directory);
    std::fs::create_dir_all(directory).expect("create fixture directory");
    seed(directory, "apple-recovery-two", 2, true);
    seed(directory, "apple-recovery-raw", 1, true);
}
