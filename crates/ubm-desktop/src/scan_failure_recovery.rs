//! Host-neutral ownership of durable scan-failure recovery.
//!
//! A scan recovery is deliberately attached by a trusted native host using a
//! recording identifier that its private [`JournalRegistry`] resolves.  The
//! resulting lease is opaque to renderer, JavaScript, and foreign callers:
//! they may be given a host-specific handle to this object, but never the
//! persisted authority or any outbox capability.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::DesktopError;
use crate::continuation_journal::JournalRegistry;
use crate::continuation_outbox::{
    Outbox, PendingScanFailure, ScanFailureCleanupRetirement, ScanFailureHead,
    ScanFailureOutboxError, ScanFailureRawInfo, ScanFailureRawToken, ScanFailureRecoveryOwner,
    WakeSink,
};
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};
use ubm_core::oversized_evidence::{EvidenceChunk, ScanFailureMember};

fn recovery_error(operation: &'static str, detail: &'static str) -> DesktopError {
    DesktopError::new(
        BleErrorCode::LifecycleInvalidState,
        BleErrorDomain::Core,
        operation,
    )
    .with_detail(detail)
}

fn map_outbox(operation: &'static str, error: ScanFailureOutboxError) -> DesktopError {
    let detail = match error {
        ScanFailureOutboxError::Sealed => "recovery lease is sealed",
        ScanFailureOutboxError::Capacity => "recovery capacity is exhausted",
        ScanFailureOutboxError::InvalidReservation => "recovery transition is invalid",
        ScanFailureOutboxError::OrdinalExhausted => "recovery ordinal space is exhausted",
        ScanFailureOutboxError::IdentityUnavailable => "recovery identity is unavailable",
        ScanFailureOutboxError::NotHead => "recovery handle is stale or foreign",
        ScanFailureOutboxError::Recovery(_) => "recovery evidence transition failed",
        ScanFailureOutboxError::Durability(_) => "recovery durable state could not be advanced",
    };
    recovery_error(operation, detail)
}

/// A process-owned, host-neutral recovery authority.  It has no filesystem
/// path API: only a configured [`JournalRegistry`] can resolve a recording.
pub struct ScanFailureRecoveryRegistry {
    journals: Arc<JournalRegistry>,
    wake: Arc<dyn WakeSink>,
    leases: Mutex<BTreeMap<String, Arc<ScanFailureRecoveryLease>>>,
}

impl ScanFailureRecoveryRegistry {
    #[must_use]
    pub fn new(journals: Arc<JournalRegistry>, wake: Arc<dyn WakeSink>) -> Self {
        Self {
            journals,
            wake,
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    /// Attach a recording after the native host has accepted its identifier
    /// under its own caller policy.  This is intentionally not a renderer or
    /// IPC authorization mechanism.
    pub fn attach_trusted(
        &self,
        recording_id: &str,
    ) -> Result<Arc<ScanFailureRecoveryLease>, DesktopError> {
        if recording_id.is_empty() {
            return Err(DesktopError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "scan.failure-recovery.attach",
            )
            .with_detail("recording identifier is empty"));
        }
        let mut leases = self.leases.lock().map_err(|_| {
            recovery_error(
                "scan.failure-recovery.attach",
                "recovery registry interrupted",
            )
        })?;
        if let Some(lease) = leases.get(recording_id) {
            return Ok(Arc::clone(lease));
        }
        let journal = self.journals.get(recording_id).map_err(|_| {
            recovery_error(
                "scan.failure-recovery.attach",
                "recording is unavailable to this trusted host",
            )
        })?;
        let identity = Outbox::recovery_authority_from_journal(&journal)
            .map_err(|_| {
                recovery_error(
                    "scan.failure-recovery.attach",
                    "recording recovery is invalid",
                )
            })?
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::CapabilityUnsupported,
                    BleErrorDomain::Core,
                    "scan.failure-recovery.attach",
                )
                .with_detail("recording has no authenticated v2 scan recovery authority")
            })?;
        // Recovery must keep the persisted source session identity. A fresh
        // process may create a new lease object, but inventing a new outbox
        // session would let it merge source ordinals with unrelated ingress.
        let outbox = Arc::new(Outbox::new(identity.session_id, Arc::clone(&self.wake)));
        outbox
            .configure_scan_failure_recovery_authority(identity.authority)
            .map_err(|error| map_outbox("scan.failure-recovery.attach", error))?;
        outbox
            .attach_journal(
                journal,
                json!({"kind": "scan-failure-recovery", "recordingId": recording_id}),
            )
            .map_err(|_| {
                recovery_error(
                    "scan.failure-recovery.attach",
                    "recording recovery could not be reopened",
                )
            })?;
        let lease = Arc::new(ScanFailureRecoveryLease { outbox });
        leases.insert(recording_id.to_owned(), Arc::clone(&lease));
        Ok(lease)
    }

    /// Drop one process-local lease after the host has revoked every alias
    /// that can still name it. Durable recovery state is not changed.
    pub fn detach_trusted(&self, recording_id: &str) -> Result<(), DesktopError> {
        let mut leases = self.leases.lock().map_err(|_| {
            recovery_error(
                "scan.failure-recovery.detach",
                "recovery registry interrupted",
            )
        })?;
        if leases.remove(recording_id).is_none() {
            return Err(recovery_error(
                "scan.failure-recovery.detach",
                "recording has no attached recovery lease",
            ));
        }
        Ok(())
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub fn lease_count(&self) -> usize {
        self.leases
            .lock()
            .expect("recovery registry mutex poisoned")
            .len()
    }
}

/// Opaque host-side lease returned by [`ScanFailureRecoveryRegistry`].
///
/// All methods accept only opaque owner/head values obtained from this exact
/// lease.  The underlying durable authority and confirmation authenticator
/// never leave this type.
pub struct ScanFailureRecoveryLease {
    outbox: Arc<Outbox>,
}

impl ScanFailureRecoveryLease {
    #[must_use]
    pub fn owners(&self) -> Vec<ScanFailureRecoveryOwner> {
        self.outbox.scan_failure_recovery_owners()
    }

    pub fn member(
        &self,
        owner: &PendingScanFailure,
        index: usize,
    ) -> Result<ScanFailureMember, DesktopError> {
        self.outbox
            .scan_failure_recovery_member(owner, index)
            .map_err(|error| map_outbox("scan.failure-recovery.member", error))
    }

    pub fn head(&self, member: ScanFailureMember) -> Result<Option<ScanFailureHead>, DesktopError> {
        self.outbox
            .scan_failure_head(member)
            .map_err(|error| map_outbox("scan.failure-recovery.head", error))
    }

    pub fn read(&self, head: &ScanFailureHead, offset: u64) -> Result<EvidenceChunk, DesktopError> {
        self.outbox
            .read_scan_failure(head, offset)
            .map_err(|error| map_outbox("scan.failure-recovery.read", error))
    }

    pub fn acknowledge(&self, head: &ScanFailureHead) -> Result<(), DesktopError> {
        self.outbox
            .acknowledge_scan_failure(head)
            .map_err(|error| map_outbox("scan.failure-recovery.acknowledge", error))
    }

    pub fn retire(&self, head: &ScanFailureHead) -> Result<(), DesktopError> {
        self.outbox
            .retire_scan_failure(head)
            .map_err(|error| map_outbox("scan.failure-recovery.retire", error))
    }

    pub fn release(&self, head: &ScanFailureHead) -> Result<(), DesktopError> {
        self.outbox
            .release_scan_failure_receipt(head)
            .map_err(|error| map_outbox("scan.failure-recovery.release", error))
    }

    /// Finalize one released descriptor and return an opaque retirement
    /// receipt. The caller must durably retain its own application outcome
    /// before calling [`Self::retire_cleanup`]; finalization never silently
    /// removes the exact replay receipt.
    pub fn finalize_release(
        &self,
        head: &ScanFailureHead,
    ) -> Result<ScanFailureCleanupRetirement, DesktopError> {
        let confirmation = self
            .outbox
            .confirm_scan_failure_receipt_release(head)
            .map_err(|error| map_outbox("scan.failure-recovery.finalize", error))?;
        self.outbox
            .finalize_scan_failure_release_with_retirement(&confirmation)
            .map_err(|error| map_outbox("scan.failure-recovery.finalize", error))
    }

    /// Reacquire every active opaque cleanup receipt after a host restart.
    pub fn pending_cleanup_receipts(
        &self,
    ) -> Result<Vec<ScanFailureCleanupRetirement>, DesktopError> {
        self.outbox
            .scan_failure_cleanup_retirements()
            .map_err(|error| map_outbox("scan.failure-recovery.cleanup-receipts", error))
    }

    /// True only when no durable owner or exact cleanup replay receipt remains.
    /// Hosts may use this to reclaim their process-local registry lease.
    pub fn is_fully_retired(&self) -> Result<bool, DesktopError> {
        self.outbox
            .scan_failure_recovery_is_fully_retired()
            .map_err(|error| map_outbox("scan.failure-recovery.fully-retired", error))
    }

    /// Explicitly retire one opaque cleanup receipt. Exact retry is
    /// idempotent, including after a cold reopen.
    pub fn retire_cleanup(
        &self,
        receipt: &ScanFailureCleanupRetirement,
    ) -> Result<(), DesktopError> {
        self.outbox
            .retire_scan_failure_cleanup(receipt)
            .map_err(|error| map_outbox("scan.failure-recovery.cleanup-retire", error))
    }

    pub fn raw_info(&self, owner: &PendingScanFailure) -> Result<ScanFailureRawInfo, DesktopError> {
        self.outbox
            .inspect_scan_failure(owner)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-info", error))
    }

    pub fn raw_member(
        &self,
        owner: &PendingScanFailure,
        index: usize,
    ) -> Result<ScanFailureMember, DesktopError> {
        self.outbox
            .inspect_scan_failure_member(owner, index)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-member", error))
    }

    pub fn raw_token(
        &self,
        owner: &PendingScanFailure,
        index: usize,
        offset: usize,
        max_bytes: usize,
    ) -> Result<ScanFailureRawToken, DesktopError> {
        self.outbox
            .inspect_scan_failure_token(owner, index, offset, max_bytes)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-token", error))
    }

    pub fn raw_retire(&self, owner: &PendingScanFailure) -> Result<(), DesktopError> {
        self.outbox
            .retire_unprepared_scan_failure(owner)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-retire", error))
    }

    pub fn raw_release(&self, owner: &PendingScanFailure) -> Result<(), DesktopError> {
        self.outbox
            .release_unprepared_scan_failure_receipt(owner)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-release", error))
    }

    /// Raw-source counterpart to [`Self::finalize_release`].
    pub fn raw_finalize(
        &self,
        owner: &PendingScanFailure,
    ) -> Result<ScanFailureCleanupRetirement, DesktopError> {
        let confirmation = self
            .outbox
            .confirm_unprepared_scan_failure_release(owner)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-finalize", error))?;
        self.outbox
            .finalize_scan_failure_release_with_retirement(&confirmation)
            .map_err(|error| map_outbox("scan.failure-recovery.raw-finalize", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoWake;
    impl WakeSink for NoWake {
        fn wake(&self, _session_id: u64) {}
    }

    #[test]
    fn trusted_attachment_refuses_an_empty_recording_id_before_registry_lookup() {
        let registry = ScanFailureRecoveryRegistry::new(Arc::default(), Arc::new(NoWake));
        let Err(error) = registry.attach_trusted("") else {
            panic!("empty recording id must never become a storage lookup");
        };
        assert_eq!(error.code_str(), "argument.invalid");
        assert_eq!(error.operation(), "scan.failure-recovery.attach");
    }

    #[test]
    fn failed_attachment_never_consumes_a_registry_lease_slot() {
        let registry = ScanFailureRecoveryRegistry::new(Arc::default(), Arc::new(NoWake));
        for index in 0..1_100 {
            assert!(
                registry
                    .attach_trusted(&format!("missing-{index}"))
                    .is_err()
            );
        }
        assert_eq!(registry.lease_count(), 0);
    }
}
