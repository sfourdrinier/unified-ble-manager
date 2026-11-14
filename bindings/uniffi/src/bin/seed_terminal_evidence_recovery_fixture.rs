//! Test-support-only durable terminal-evidence fixture for the generated
//! Swift boundary test.  It exits before Swift starts, so the foreign test
//! can only reopen the trusted SQLite ledger through the production binding.

use std::path::Path;

use sha2::{Digest, Sha256};
use ubm_desktop::terminal_evidence_ledger::{TerminalEvidenceLedger, TerminalEvidenceScope};

fn scope(producer: &str, operation_id: &str) -> TerminalEvidenceScope {
    TerminalEvidenceScope {
        producer: producer.to_owned(),
        manager_id: "swift-terminal-manager".to_owned(),
        operation_id: operation_id.to_owned(),
        incarnation: "1".to_owned(),
        scope_utf16le: None,
    }
}

fn main() {
    let directory = std::env::args().nth(1).expect("fixture directory argument");
    let directory = Path::new(&directory);
    std::fs::create_dir_all(directory).expect("create fixture directory");
    let ledger = TerminalEvidenceLedger::open(&directory.join("terminal-evidence.sqlite"), 65_536)
        .expect("open terminal evidence ledger");

    let staged = ledger
        .reserve(&scope("scan", "staged"), 1024)
        .expect("reserve staged");
    ledger
        .stage(
            &staged.reservation_id,
            "swift-terminal-recovery",
            b"staged-terminal-evidence",
        )
        .expect("stage terminal evidence");

    let committed_bytes = b"committed-terminal-evidence";
    let committed = ledger
        .reserve(&scope("subscription", "committed"), 1024)
        .expect("reserve committed");
    ledger
        .stage(
            &committed.reservation_id,
            "swift-terminal-recovery",
            committed_bytes,
        )
        .expect("stage committed terminal evidence");
    ledger
        .commit(
            &committed.reservation_id,
            committed_bytes,
            Sha256::digest(committed_bytes).into(),
        )
        .expect("commit terminal evidence");
}
