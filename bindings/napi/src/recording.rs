//! Trusted main-process durable access, including when no BLE authority exists.
use napi::Result;
use napi_derive::napi;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use ubm_desktop::continuation::{envelope, recording_failure, NativeContinuation};
use ubm_desktop::continuation_journal::{run_blocking, JournalRegistry};
use ubm_desktop::continuation_outbox::WakeSink;
use ubm_desktop::continuation_outbox::{
    PendingScanFailure, ScanFailureCleanupRetirement, ScanFailureHead,
};
use ubm_desktop::scan_failure_recovery::{ScanFailureRecoveryLease, ScanFailureRecoveryRegistry};

/// Scan recovery has no event stream on the Node main-process surface.  The
/// lease exists only to reopen and explicitly advance already-durable state;
/// a wake therefore has no JS-visible effect here.
struct ScanRecoveryNoWake;
impl WakeSink for ScanRecoveryNoWake {
    fn wake(&self, _: u64) {}
}

fn scan_failure_recovery_error(error: ubm_desktop::DesktopError) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}

async fn scan_blocking<T, F>(operation: F) -> napi::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> napi::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| napi::Error::from_reason("scan recovery worker did not complete"))?
}

struct RecoveryHandles {
    admission: Mutex<()>,
    limit: usize,
    next: AtomicU64,
    leases: Mutex<HashMap<String, Arc<ScanFailureRecoveryLease>>>,
    lease_recordings: Mutex<HashMap<String, String>>,
    owners: Mutex<HashMap<String, (String, PendingScanFailure)>>,
    heads: Mutex<HashMap<String, (String, ScanFailureHead)>>,
    cleanup: Mutex<HashMap<String, (String, ScanFailureCleanupRetirement)>>,
    cleanup_reservations: AtomicU64,
    #[cfg(test)]
    admission_barrier: Mutex<Option<Arc<TestAdmissionBarrier>>>,
}

#[cfg(test)]
struct TestAdmissionBarrier {
    barrier: std::sync::Barrier,
    remaining: AtomicU64,
}

/// Bounded process-local aliases prevent repeated inventory polling from
/// minting unbounded bearer-like strings.  We never evict a live alias: doing
/// so would turn a concurrent renderer operation into apparent data loss.
const MAX_SCAN_RECOVERY_HANDLES_PER_KIND: usize = 1_024;

impl RecoveryHandles {
    fn new() -> Self {
        Self {
            admission: Mutex::new(()),
            limit: MAX_SCAN_RECOVERY_HANDLES_PER_KIND,
            next: AtomicU64::new(1),
            leases: Mutex::new(HashMap::new()),
            lease_recordings: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            heads: Mutex::new(HashMap::new()),
            cleanup: Mutex::new(HashMap::new()),
            cleanup_reservations: AtomicU64::new(0),
            #[cfg(test)]
            admission_barrier: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_limit(limit: usize) -> Self {
        assert!(limit > 0, "test handle limit must be positive");
        Self {
            admission: Mutex::new(()),
            limit,
            next: AtomicU64::new(1),
            leases: Mutex::new(HashMap::new()),
            lease_recordings: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            heads: Mutex::new(HashMap::new()),
            cleanup: Mutex::new(HashMap::new()),
            cleanup_reservations: AtomicU64::new(0),
            admission_barrier: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn hold_next_admissions(&self, participants: u64) {
        assert!(
            participants > 0,
            "test admission barrier needs participants"
        );
        *self
            .admission_barrier
            .lock()
            .expect("test admission barrier poisoned") = Some(Arc::new(TestAdmissionBarrier {
            barrier: std::sync::Barrier::new(participants as usize),
            remaining: AtomicU64::new(participants),
        }));
    }

    #[cfg(test)]
    fn wait_for_test_admission(&self) {
        let barrier = {
            let mut configured = self
                .admission_barrier
                .lock()
                .expect("test admission barrier poisoned");
            let Some(barrier) = configured.as_ref() else {
                return;
            };
            let remaining = barrier.remaining.fetch_sub(1, Ordering::AcqRel);
            assert!(remaining > 0, "test admission barrier over-subscribed");
            let barrier = Arc::clone(barrier);
            if remaining == 1 {
                configured.take();
            }
            barrier
        };
        barrier.barrier.wait();
    }

    fn id(&self, kind: &str) -> String {
        let serial = self.next.fetch_add(1, Ordering::Relaxed);
        format!("scan-recovery-{kind}-{serial}")
    }

    fn lease(&self, id: &str) -> napi::Result<Arc<ScanFailureRecoveryLease>> {
        self.leases
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .get(id)
            .cloned()
            .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery handle"))
    }

    fn issue_lease(&self, lease: Arc<ScanFailureRecoveryLease>) -> napi::Result<String> {
        let mut leases = self
            .leases
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        if let Some((id, _)) = leases
            .iter()
            .find(|(_, existing)| Arc::ptr_eq(existing, &lease))
        {
            return Ok(id.clone());
        }
        if leases.len() >= self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery lease handle capacity exhausted",
            ));
        }
        let id = self.id("lease");
        leases.insert(id.clone(), lease);
        Ok(id)
    }

    fn attached_lease(&self, recording_id: &str) -> napi::Result<Option<String>> {
        Ok(self
            .lease_recordings
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .get(recording_id)
            .cloned())
    }

    fn admit_recording(&self, recording_id: &str) -> napi::Result<()> {
        let recordings = self
            .lease_recordings
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        if recordings.contains_key(recording_id) {
            return Ok(());
        }
        if recordings.len() >= self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery lease handle capacity exhausted",
            ));
        }
        Ok(())
    }

    fn remember_recording(&self, recording_id: String, lease_id: String) -> napi::Result<()> {
        self.lease_recordings
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .insert(recording_id, lease_id);
        Ok(())
    }

    fn owner(&self, id: &str) -> napi::Result<(Arc<ScanFailureRecoveryLease>, PendingScanFailure)> {
        let (lease, owner) = self
            .owners
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .get(id)
            .cloned()
            .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery owner"))?;
        Ok((self.lease(&lease)?, owner))
    }

    fn head(&self, id: &str) -> napi::Result<(Arc<ScanFailureRecoveryLease>, ScanFailureHead)> {
        let (lease, head) = self
            .heads
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .get(id)
            .cloned()
            .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery head"))?;
        Ok((self.lease(&lease)?, head))
    }

    fn cleanup(
        &self,
        id: &str,
    ) -> napi::Result<(Arc<ScanFailureRecoveryLease>, ScanFailureCleanupRetirement)> {
        let (lease, receipt) = self
            .cleanup
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .get(id)
            .cloned()
            .ok_or_else(|| {
                napi::Error::from_reason("unknown or revoked scan recovery cleanup receipt")
            })?;
        Ok((self.lease(&lease)?, receipt))
    }

    #[cfg(test)]
    fn issue_owner(&self, lease_id: &str, owner: PendingScanFailure) -> napi::Result<String> {
        let mut owners = self
            .owners
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        if let Some((id, _)) = owners.iter().find(|(_, (existing_lease, existing))| {
            existing_lease == lease_id && *existing == owner
        }) {
            return Ok(id.clone());
        }
        if owners.len() >= self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery owner handle capacity exhausted",
            ));
        }
        let id = self.id("owner");
        owners.insert(id.clone(), (lease_id.to_owned(), owner));
        Ok(id)
    }

    fn issue_owner_batch(
        &self,
        lease_id: &str,
        values: &[PendingScanFailure],
    ) -> napi::Result<Vec<String>> {
        let mut owners = self
            .owners
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        let mut missing = Vec::new();
        for owner in values {
            let exists = owners.iter().any(|(_, (existing_lease, existing))| {
                existing_lease == lease_id && existing == owner
            }) || missing
                .iter()
                .any(|existing: &PendingScanFailure| existing == owner);
            if !exists {
                missing.push(*owner);
            }
        }
        if owners.len() + missing.len() > self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery owner handle capacity exhausted",
            ));
        }
        let mut issued = Vec::with_capacity(values.len());
        for owner in values {
            if let Some((id, _)) = owners.iter().find(|(_, (existing_lease, existing))| {
                existing_lease == lease_id && existing == owner
            }) {
                issued.push(id.clone());
            } else {
                let id = self.id("owner");
                owners.insert(id.clone(), (lease_id.to_owned(), *owner));
                issued.push(id);
            }
        }
        Ok(issued)
    }

    fn issue_head(&self, lease_id: &str, head: ScanFailureHead) -> napi::Result<String> {
        let mut heads = self
            .heads
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        if let Some((id, _)) = heads.iter().find(|(_, (existing_lease, existing))| {
            existing_lease == lease_id
                && existing.ordinal == head.ordinal
                && existing.capability == head.capability
        }) {
            return Ok(id.clone());
        }
        if heads.len() >= self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery head handle capacity exhausted",
            ));
        }
        let id = self.id("head");
        heads.insert(id.clone(), (lease_id.to_owned(), head));
        Ok(id)
    }

    fn issue_cleanup_batch(
        &self,
        lease_id: &str,
        receipts: Vec<ScanFailureCleanupRetirement>,
    ) -> napi::Result<Vec<String>> {
        let mut cleanup = self
            .cleanup
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        let reserved = self.cleanup_reservations.load(Ordering::Acquire) as usize;
        let mut missing = Vec::new();
        for receipt in &receipts {
            let exists = cleanup.iter().any(|(_, (existing_lease, existing))| {
                existing_lease == lease_id && existing == receipt
            }) || missing
                .iter()
                .any(|existing: &ScanFailureCleanupRetirement| existing == receipt);
            if !exists {
                missing.push(receipt.clone());
            }
        }
        if cleanup.len() + reserved + missing.len() > self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery cleanup handle capacity exhausted",
            ));
        }
        let mut result = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            if let Some((id, _)) = cleanup.iter().find(|(_, (existing_lease, existing))| {
                existing_lease == lease_id && *existing == receipt
            }) {
                result.push(id.clone());
            } else {
                let id = self.id("cleanup");
                cleanup.insert(id.clone(), (lease_id.to_owned(), receipt));
                result.push(id);
            }
        }
        Ok(result)
    }

    /// Reserve the only resource that cannot be reconstructed after a
    /// successful terminal native transition: the immediate opaque cleanup
    /// receipt alias.  The map lock serializes reservation with inventory
    /// issuance, so a concurrent list cannot consume this reserved slot.
    fn reserve_cleanup(&self) -> napi::Result<()> {
        let cleanup = self
            .cleanup
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        let reserved = self.cleanup_reservations.load(Ordering::Acquire) as usize;
        if cleanup.len() + reserved >= self.limit {
            return Err(napi::Error::from_reason(
                "scan recovery cleanup handle capacity exhausted",
            ));
        }
        self.cleanup_reservations.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn release_cleanup_reservation(&self) {
        self.cleanup_reservations.fetch_sub(1, Ordering::AcqRel);
    }

    fn issue_reserved_cleanup(
        &self,
        lease_id: &str,
        receipt: ScanFailureCleanupRetirement,
    ) -> napi::Result<String> {
        let result = (|| {
            let mut cleanup = self.cleanup.lock().map_err(|_| {
                napi::Error::from_reason("scan recovery handle registry interrupted")
            })?;
            if let Some((id, _)) = cleanup.iter().find(|(_, (existing_lease, existing))| {
                existing_lease == lease_id && *existing == receipt
            }) {
                return Ok(id.clone());
            }
            let id = self.id("cleanup");
            cleanup.insert(id.clone(), (lease_id.to_owned(), receipt));
            Ok(id)
        })();
        self.release_cleanup_reservation();
        result
    }

    /// Finalization is the terminal native transition for this replay head.
    /// Reclaim aliases only after the native receipt is safely represented by
    /// a cleanup alias; ordinary release/retire keep their aliases because a
    /// caller may still need the explicit finalization step.
    fn retire_finalized_head(&self, head_id: &str) -> napi::Result<()> {
        let head = self
            .heads
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .remove(head_id)
            .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery head"))?;
        let mut owners = self
            .owners
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?;
        owners.retain(|_, (lease, owner)| {
            !(lease == &head.0
                && owner.ordinal == head.1.ordinal
                && owner.capability == head.1.capability)
        });
        Ok(())
    }

    fn retire_finalized_owner(&self, owner_id: &str) -> napi::Result<()> {
        self.owners
            .lock()
            .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
            .remove(owner_id)
            .map(|_| ())
            .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery owner"))
    }
}

fn diagnostic_id(value: &str) -> ubm_desktop::continuation::Result<[u8; 40]> {
    if value.len() != 80
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic occurrence identity","platform":null}),
        );
    }
    let mut id = [0; 40];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic occurrence identity","platform":null}))?;
    }
    Ok(id)
}

fn diagnostic_id_text(id: [u8; 40]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn diagnostic_offset(value: &str) -> ubm_desktop::continuation::Result<u64> {
    value.parse::<u64>().ok().filter(|number| number.to_string() == value && *number <= i64::MAX as u64)
        .ok_or_else(|| serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"invalid diagnostic offset","platform":null}))
}

fn diagnostic_action(
    value: &str,
) -> ubm_desktop::continuation::Result<ubm_desktop::continuation_journal::DiagnosticExactAction> {
    use ubm_desktop::continuation_journal::DiagnosticExactAction;
    match value {
        "acknowledge" => Ok(DiagnosticExactAction::Acknowledge),
        "retire" => Ok(DiagnosticExactAction::Retire),
        "confirm" => Ok(DiagnosticExactAction::Confirm),
        "release" => Ok(DiagnosticExactAction::Release),
        _ => Err(
            serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.diagnostic","detail":"unknown diagnostic lifecycle transition","platform":null}),
        ),
    }
}

/// Receipt schema emitted by both the ordered recording owner and offline store.
#[napi(catch_unwind)]
pub fn ordered_recording_receipt_version() -> u32 {
    2
}

#[napi]
pub struct ContinuationRecordingStore {
    registry: Arc<JournalRegistry>,
    engine: Option<NativeContinuation>,
    scan_recovery: Arc<ScanFailureRecoveryRegistry>,
}

impl ContinuationRecordingStore {
    pub(crate) fn from_engine(engine: NativeContinuation) -> Self {
        Self {
            registry: engine.recording_registry(),
            scan_recovery: Arc::new(ScanFailureRecoveryRegistry::new(
                engine.recording_registry(),
                Arc::new(ScanRecoveryNoWake),
            )),
            engine: Some(engine),
        }
    }
}

#[napi]
impl ContinuationRecordingStore {
    /// Exact receipt revision for callers using the store directly.
    #[napi(catch_unwind)]
    pub async fn ordered_receipt_version(&self, _id: String) -> u32 {
        ordered_recording_receipt_version()
    }
    /// Native-host attachment point. Returned handles are process-local and
    /// opaque; Electron's main broker must retain this object and expose only
    /// caller-bound IPC aliases. A Node-enabled Electron renderer in a
    /// separate process can nevertheless load native modules with the same
    /// OS-user authority granted by its embedding app, so N-API cannot
    /// honestly distinguish it from main with a JS global check.
    #[napi(catch_unwind)]
    pub fn scan_failure_recovery(&self) -> Result<ScanFailureRecoveryStore> {
        Ok(ScanFailureRecoveryStore {
            registry: Arc::clone(&self.scan_recovery),
            handles: Arc::new(RecoveryHandles::new()),
        })
    }
    #[napi(constructor, catch_unwind)]
    pub fn new() -> Self {
        Self {
            registry: Arc::default(),
            scan_recovery: Arc::new(ScanFailureRecoveryRegistry::new(
                Arc::default(),
                Arc::new(ScanRecoveryNoWake),
            )),
            engine: None,
        }
    }

    /// Host-chosen app-private directory. Electron main retains this native
    /// store; preload/renderer code must use the versioned IPC surface, never
    /// receive this object or its directory authority.
    #[napi(catch_unwind)]
    pub async fn configure_directory(&self, directory: String) -> Result<String> {
        self.configure_directory_trusted(directory).await
    }

    /// Releases only idle handles for this trusted store's configured
    /// directory. Durable rows and live owners remain intact; a later call
    /// reopens a released journal from its retained bytes.
    #[napi(catch_unwind)]
    pub async fn release_idle(&self) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .release_idle_after_host_shutdown()
                    .map(|_| serde_json::json!({"released": true}))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }

    /// Rust-only trusted-host entrypoint used by native integration tests and
    /// the central-owned main-process bridge. It is intentionally not an
    /// N-API export, so a renderer cannot bypass the environment guard.
    pub(crate) async fn configure_directory_trusted(&self, directory: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                #[cfg(windows)]
                let configured = registry.configure_directory_with_scan_recovery_storage(
                    std::path::Path::new(&directory),
                    Arc::new(
                        ubm_desktop::continuation_journal::WindowsCurrentUserScanRecoveryStorageProtector::new(),
                    ),
                );
                #[cfg(not(windows))]
                let configured = registry.configure_directory(std::path::Path::new(&directory));
                configured.map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn status(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.status())
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn prepare(&self, id: String, max_items: u32, max_bytes: u32) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.prepare(max_items, max_bytes))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn acknowledge(&self, id: String, token: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.acknowledge(&token))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    /// Trusted host-only durable diagnostic recovery. Every transition compares
    /// the complete exact payload before mutating ownership.
    #[napi(catch_unwind)]
    pub async fn diagnostic_summary_page(
        &self,
        id: String,
        offset: String,
        max_bytes: u32,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(run_blocking(move || {
            let offset = diagnostic_offset(&offset)?;
            let page = registry.get(&id).and_then(|journal| journal.diagnostic_summary_page(offset, max_bytes)).map_err(recording_failure)?;
            Ok(serde_json::json!({"bytes":page.bytes,"totalBytes":page.total_bytes.to_string()}))
        }).await))
    }
    #[napi(catch_unwind)]
    pub async fn diagnostic_occurrences_page(
        &self,
        id: String,
        after: Option<String>,
        limit: u32,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(run_blocking(move || {
            let after = after.as_deref().map(diagnostic_id).transpose()?;
            let ids = registry.get(&id).and_then(|journal| journal.diagnostic_occurrences_page(after, limit as usize)).map_err(recording_failure)?;
            Ok(serde_json::json!({"occurrences":ids.into_iter().map(diagnostic_id_text).collect::<Vec<_>>() }))
        }).await))
    }
    #[napi(catch_unwind)]
    pub async fn diagnostic_occurrence_page(
        &self,
        id: String,
        occurrence: String,
        offset: String,
        max_bytes: u32,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(run_blocking(move || {
            let occurrence = diagnostic_id(&occurrence)?;
            let offset = diagnostic_offset(&offset)?;
            let page = registry.get(&id).and_then(|journal| journal.diagnostic_occurrence_page(occurrence, offset, max_bytes)).map_err(recording_failure)?;
            Ok(serde_json::json!({"bytes":page.bytes,"totalBytes":page.total_bytes.to_string(),"durable":page.durable,"reclaimed":page.reclaimed}))
        }).await))
    }
    #[napi(catch_unwind)]
    pub async fn retry_diagnostic_occurrence(
        &self,
        id: String,
        occurrence: String,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                let occurrence = diagnostic_id(&occurrence)?;
                registry
                    .get(&id)
                    .and_then(|journal| journal.retry_production_loss(occurrence))
                    .map_err(recording_failure)?;
                Ok(serde_json::json!({"occurrence":diagnostic_id_text(occurrence),"retried":true}))
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn acknowledge_diagnostic_occurrence(
        &self,
        id: String,
        occurrence: String,
        payload: String,
    ) -> Result<String> {
        self.transition_diagnostic_occurrence(id, occurrence, payload, "acknowledge")
            .await
    }
    #[napi(catch_unwind)]
    pub async fn retire_diagnostic_occurrence(
        &self,
        id: String,
        occurrence: String,
        payload: String,
    ) -> Result<String> {
        self.transition_diagnostic_occurrence(id, occurrence, payload, "retire")
            .await
    }
    #[napi(catch_unwind)]
    pub async fn confirm_diagnostic_retirement(
        &self,
        id: String,
        occurrence: String,
        payload: String,
    ) -> Result<String> {
        self.transition_diagnostic_occurrence(id, occurrence, payload, "confirm")
            .await
    }
    #[napi(catch_unwind)]
    pub async fn release_diagnostic_confirmation(
        &self,
        id: String,
        occurrence: String,
        payload: String,
    ) -> Result<String> {
        self.transition_diagnostic_occurrence(id, occurrence, payload, "release")
            .await
    }
    /// Bounded, crash-resumable exact diagnostic lifecycle. Each call compares
    /// at most one 64KiB slice and persists its offset before returning.
    #[napi(catch_unwind)]
    pub async fn transition_diagnostic_occurrence_chunk(
        &self,
        id: String,
        occurrence: String,
        transition: String,
        offset: String,
        bytes: Vec<u8>,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(run_blocking(move || {
            let occurrence = diagnostic_id(&occurrence)?;
            let action = diagnostic_action(&transition)?;
            let offset = diagnostic_offset(&offset)?;
            let progress = registry.get(&id).and_then(|journal| journal.diagnostic_transition_chunk(occurrence, action, offset, &bytes)).map_err(recording_failure)?;
            Ok(serde_json::json!({"occurrence":diagnostic_id_text(occurrence),"transition":transition,"offset":progress.offset.to_string(),"totalBytes":progress.total.to_string(),"complete":progress.complete}))
        }).await))
    }
    /// Trusted host recovery of an already-prepared pre-v5 page. This does not
    /// migrate the journal or establish a v5 claim identity.
    #[napi(catch_unwind)]
    pub async fn legacy_recording_prepare(
        &self,
        id: String,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .legacy_recording_prepare(&id, max_items, max_bytes)
                    .map_err(recording_failure)
            })
            .await,
        ))
    }

    #[napi(catch_unwind)]
    pub async fn legacy_recording_acknowledge(&self, id: String, token: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .legacy_recording_acknowledge(&id, &token)
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    /// Safe desktop-only legacy protocol; the caller persists the returned raw page.
    #[napi(catch_unwind)]
    pub async fn legacy_recording_prepare_exact(
        &self,
        id: String,
        max_items: u32,
        max_bytes: u32,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .legacy_recording_prepare_exact(&id, max_items, max_bytes)
                    .map_err(recording_failure)
            })
            .await,
        ))
    }

    /// Reopens the raw exact page retained across a prior process failure. The
    /// store keeps the shared v1 authority while this page is outstanding.
    #[napi(catch_unwind)]
    pub async fn legacy_recording_recover_exact(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .legacy_recording_recover_exact(&id)
                    .map(|page| page.unwrap_or(serde_json::Value::Null))
                    .map_err(recording_failure)
            })
            .await,
        ))
    }

    #[napi(catch_unwind)]
    pub async fn legacy_recording_acknowledge_exact(
        &self,
        id: String,
        page: String,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                let value = parse_exact_page(&page)?;
                registry
                    .legacy_recording_acknowledge_exact(&id, &value)
                    .map_err(recording_failure)
            })
            .await,
        ))
    }

    #[napi(catch_unwind)]
    pub async fn legacy_recording_retire_exact(&self, id: String, page: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                let value = parse_exact_page(&page)?;
                registry
                    .legacy_recording_retire_exact(&id, &value)
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn legacy_recording_confirm_exact_retirement(
        &self,
        id: String,
        page: String,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                let value = parse_exact_page(&page)?;
                registry
                    .legacy_recording_confirm_exact_retirement(&id, &value)
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
    /// Read-only bounded raw recovery, including rows that cannot be decoded.
    #[napi(catch_unwind)]
    pub async fn inspect_raw_record(
        &self,
        id: String,
        source: String,
        ordinal: Option<String>,
        offset: String,
        max_bytes: u32,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(run_blocking(move || {
            let offset = offset.parse::<u64>().ok().filter(|n| n.to_string() == offset && *n <= i64::MAX as u64)
                .ok_or_else(|| serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.inspect-raw","detail":"invalid offset","platform":null}))?;
            registry.inspect_raw_record(&id, &source, ordinal.as_deref(), offset, max_bytes).map_err(recording_failure)
        }).await))
    }
    #[napi(catch_unwind)]
    pub async fn stop(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        let engine = self.engine.clone();
        Ok(envelope(
            run_blocking(move || {
                if let Some(engine) = engine {
                    engine.recording_stop(&id)
                } else {
                    let mut receipt = registry
                        .get(&id)
                        .and_then(|journal| journal.stop())
                        .map_err(recording_failure)?;
                    receipt["radioRelease"] = "not-requested".into();
                    Ok(receipt)
                }
            })
            .await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn ordered_context(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || registry.ordered_context(&id).map_err(recording_failure)).await,
        ))
    }
    #[napi(catch_unwind)]
    pub async fn stop_ordered(&self, id: String) -> Result<String> {
        Ok(envelope(self.registry.stop_ordered_host_receipt(&id).await))
    }

    #[napi(catch_unwind)]
    pub async fn clear(&self, id: String) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(
            run_blocking(move || {
                registry
                    .get(&id)
                    .and_then(|journal| journal.clear())
                    .map_err(recording_failure)
            })
            .await,
        ))
    }
}

impl Default for ContinuationRecordingStore {
    fn default() -> Self {
        Self::new()
    }
}

fn hex_id(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn scan_member_json(member: ubm_core::oversized_evidence::ScanFailureMember) -> serde_json::Value {
    serde_json::json!({"recipient":hex_id(&member.recipient),"delivery":hex_id(&member.delivery)})
}

fn scan_identity_json(
    identity: ubm_core::oversized_evidence::ScanFailureIdentity,
) -> serde_json::Value {
    serde_json::json!({
        "host":hex_id(&identity.host), "scan":hex_id(&identity.scan), "start":hex_id(&identity.start),
        "sourceGeneration":hex_id(&identity.source_generation), "occurrence":hex_id(&identity.occurrence)
    })
}

fn scan_head_json(handle: &str, head: &ScanFailureHead) -> serde_json::Value {
    serde_json::json!({
        "handleId":handle,
        "ordinal":head.ordinal.to_string(),
        "descriptor":{
            "protocol":head.evidence.protocol,
            "representation":head.evidence.representation,
            "identity":scan_identity_json(head.evidence.identity),
            "member":scan_member_json(head.evidence.member),
            "encodedLength":head.evidence.encoded_length.to_string(),
            "chunkBytes":head.evidence.chunk_bytes,
        }
    })
}

/// Trusted desktop recovery binding.  No returned string is a durable
/// capability: every identifier indexes process-private Rust state and is
/// invalid after this object or its host is released.
#[napi]
pub struct ScanFailureRecoveryStore {
    registry: Arc<ScanFailureRecoveryRegistry>,
    handles: Arc<RecoveryHandles>,
}

#[napi]
impl ScanFailureRecoveryStore {
    #[napi(catch_unwind)]
    pub async fn attach(&self, recording_id: String) -> Result<String> {
        let registry = Arc::clone(&self.registry);
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let _admission = handles
                .admission
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            if let Some(id) = handles.attached_lease(&recording_id)? {
                return Ok(serde_json::json!({"leaseId":id}).to_string());
            }
            // Admit before touching the core registry: a caller rejected by
            // the bounded process-local surface must not create a permanent
            // hidden registry entry.
            handles.admit_recording(&recording_id)?;
            let lease = registry
                .attach_trusted(&recording_id)
                .map_err(scan_failure_recovery_error)?;
            let id = handles.issue_lease(lease)?;
            handles.remember_recording(recording_id, id.clone())?;
            Ok(serde_json::json!({"leaseId":id}).to_string())
        })
        .await
    }

    /// Explicitly revoke all process-local authority for an idle lease. A
    /// lease with live owner, head, or cleanup aliases cannot be detached.
    #[napi(catch_unwind)]
    pub async fn detach(&self, lease_id: String) -> Result<()> {
        let registry = Arc::clone(&self.registry);
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            #[cfg(test)]
            handles.wait_for_test_admission();
            let _admission = handles
                .admission
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            let has_owner = {
                let owners = handles.owners.lock().map_err(|_| {
                    napi::Error::from_reason("scan recovery handle registry interrupted")
                })?;
                owners.values().any(|(lease, _)| lease == &lease_id)
            };
            let has_head = {
                let heads = handles.heads.lock().map_err(|_| {
                    napi::Error::from_reason("scan recovery handle registry interrupted")
                })?;
                heads.values().any(|(lease, _)| lease == &lease_id)
            };
            let has_cleanup = {
                let cleanup = handles.cleanup.lock().map_err(|_| {
                    napi::Error::from_reason("scan recovery handle registry interrupted")
                })?;
                cleanup.values().any(|(lease, _)| lease == &lease_id)
            };
            if has_owner || has_head || has_cleanup {
                return Err(napi::Error::from_reason(
                    "scan recovery lease still has dependent aliases",
                ));
            }
            let recording_id = handles
                .lease_recordings
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .iter()
                .find_map(|(recording, existing)| {
                    (existing == &lease_id).then(|| recording.clone())
                })
                .ok_or_else(|| {
                    napi::Error::from_reason("unknown or revoked scan recovery handle")
                })?;
            registry
                .detach_trusted(&recording_id)
                .map_err(scan_failure_recovery_error)?;
            handles
                .leases
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .remove(&lease_id);
            handles
                .lease_recordings
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .remove(&recording_id);
            Ok(())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn owners(&self, lease_id: String) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            #[cfg(test)]
            handles.wait_for_test_admission();
            let _admission = handles.admission.lock().map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            let lease = handles.lease(&lease_id)?;
            let owners = lease.owners();
            let aliases = handles.issue_owner_batch(
                &lease_id,
                &owners.iter().map(|owner| owner.key).collect::<Vec<_>>(),
            )?;
            let mut values = Vec::with_capacity(owners.len());
            for (owner, id) in owners.into_iter().zip(aliases) {
                values.push(serde_json::json!({
                    "handleId":id, "prepared":owner.prepared, "producerFinished":owner.producer_finished,
                    "rawRetired":owner.raw_retired, "rawReleased":owner.raw_released,
                    "memberCount":owner.member_count, "released":owner.released.is_some(),
                }));
            }
            Ok(serde_json::json!({"owners":values}).to_string())
        }).await
    }

    #[napi(catch_unwind)]
    pub async fn member(&self, owner_handle: String, index: u32) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, owner) = handles.owner(&owner_handle)?;
            let member = lease
                .member(&owner, index as usize)
                .map_err(scan_failure_recovery_error)?;
            Ok(serde_json::json!({"member":scan_member_json(member)}).to_string())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn head(&self, owner_handle: String, index: u32) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let _admission = handles
                .admission
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            let (lease, owner) = handles.owner(&owner_handle)?;
            let member = lease
                .member(&owner, index as usize)
                .map_err(scan_failure_recovery_error)?;
            let Some(head) = lease.head(member).map_err(scan_failure_recovery_error)? else {
                return Ok(serde_json::json!({"head":serde_json::Value::Null}).to_string());
            };
            let owner_lease = handles
                .owners
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .get(&owner_handle)
                .map(|(lease_id, _)| lease_id.clone())
                .ok_or_else(|| {
                    napi::Error::from_reason("unknown or revoked scan recovery owner")
                })?;
            let id = handles.issue_head(&owner_lease, head)?;
            Ok(serde_json::json!({"head":scan_head_json(&id, &head)}).to_string())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn read(&self, head_handle: String, offset: String) -> Result<String> {
        let offset = offset
            .parse::<u64>()
            .map_err(|_| napi::Error::from_reason("invalid scan recovery offset"))?;
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, head) = handles.head(&head_handle)?;
            let page = lease.read(&head, offset).map_err(scan_failure_recovery_error)?;
            Ok(serde_json::json!({"offset":page.offset.to_string(),"nextOffset":page.next_offset.to_string(),"bytes":page.bytes,"done":page.done}).to_string())
        }).await
    }

    #[napi(catch_unwind)]
    pub async fn acknowledge(&self, head_handle: String) -> Result<()> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, head) = handles.head(&head_handle)?;
            lease
                .acknowledge(&head)
                .map_err(scan_failure_recovery_error)
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn retire(&self, head_handle: String) -> Result<()> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, head) = handles.head(&head_handle)?;
            lease.retire(&head).map_err(scan_failure_recovery_error)
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn release(&self, head_handle: String) -> Result<()> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, head) = handles.head(&head_handle)?;
            lease.release(&head).map_err(scan_failure_recovery_error)
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn finalize_release(&self, head_handle: String) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let _admission = handles
                .admission
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            let (lease, head) = handles.head(&head_handle)?;
            let head_lease = handles
                .heads
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .get(&head_handle)
                .map(|(lease_id, _)| lease_id.clone())
                .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery head"))?;
            handles.reserve_cleanup()?;
            let receipt = lease.finalize_release(&head).map_err(|error| {
                handles.release_cleanup_reservation();
                scan_failure_recovery_error(error)
            })?;
            let id = handles.issue_reserved_cleanup(&head_lease, receipt)?;
            handles.retire_finalized_head(&head_handle)?;
            Ok(serde_json::json!({"cleanupReceiptId":id}).to_string())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn pending_cleanup_receipts(&self, lease_id: String) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let _admission = handles
                .admission
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            let lease = handles.lease(&lease_id)?;
            let receipts = lease
                .pending_cleanup_receipts()
                .map_err(scan_failure_recovery_error)?;
            let values = handles.issue_cleanup_batch(&lease_id, receipts)?;
            Ok(serde_json::json!({"cleanupReceiptIds":values}).to_string())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn retire_cleanup(&self, cleanup_receipt_id: String) -> Result<()> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, receipt) = handles.cleanup(&cleanup_receipt_id)?;
            lease
                .retire_cleanup(&receipt)
                .map_err(scan_failure_recovery_error)?;
            handles
                .cleanup
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .remove(&cleanup_receipt_id);
            Ok(())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn raw_info(&self, owner_handle: String) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || { let (lease, owner) = handles.owner(&owner_handle)?; let raw = lease.raw_info(&owner).map_err(scan_failure_recovery_error)?; Ok(serde_json::json!({"identity":scan_identity_json(raw.identity),"memberCount":raw.member_count,"tokenCount":raw.token_count}).to_string()) }).await
    }

    #[napi(catch_unwind)]
    pub async fn raw_member(&self, owner_handle: String, index: u32) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, owner) = handles.owner(&owner_handle)?;
            let member = lease
                .raw_member(&owner, index as usize)
                .map_err(scan_failure_recovery_error)?;
            Ok(serde_json::json!({"member":scan_member_json(member)}).to_string())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn raw_token(
        &self,
        owner_handle: String,
        index: u32,
        offset: u32,
        max_bytes: u32,
    ) -> Result<String> {
        if !(1..=65_536).contains(&max_bytes) {
            return Err(napi::Error::from_reason(
                "invalid scan recovery raw token byte limit",
            ));
        }
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, owner) = handles.owner(&owner_handle)?;
            let token = lease
                .raw_token(&owner, index as usize, offset as usize, max_bytes as usize)
                .map_err(scan_failure_recovery_error)?;
            let value = match token {
                ubm_desktop::continuation_outbox::ScanFailureRawToken::Null => {
                    serde_json::json!({"kind":"null"})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::Bool(value) => {
                    serde_json::json!({"kind":"bool","value":value})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::I64(value) => {
                    serde_json::json!({"kind":"i64","value":value.to_string()})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::U64(value) => {
                    serde_json::json!({"kind":"u64","value":value.to_string()})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::F64Bits(value) => {
                    serde_json::json!({"kind":"f64Bits","value":value.to_string()})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::Text { total, bytes } => {
                    serde_json::json!({"kind":"text","total":total.to_string(),"bytes":bytes})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::Bytes { total, bytes } => {
                    serde_json::json!({"kind":"bytes","total":total.to_string(),"bytes":bytes})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::Map(value) => {
                    serde_json::json!({"kind":"map","count":value.to_string()})
                }
                ubm_desktop::continuation_outbox::ScanFailureRawToken::Array(value) => {
                    serde_json::json!({"kind":"array","count":value.to_string()})
                }
            };
            Ok(value.to_string())
        })
        .await
    }

    #[napi(catch_unwind)]
    pub async fn raw_retire(&self, owner_handle: String) -> Result<()> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, owner) = handles.owner(&owner_handle)?;
            lease
                .raw_retire(&owner)
                .map_err(scan_failure_recovery_error)
        })
        .await
    }
    #[napi(catch_unwind)]
    pub async fn raw_release(&self, owner_handle: String) -> Result<()> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let (lease, owner) = handles.owner(&owner_handle)?;
            lease
                .raw_release(&owner)
                .map_err(scan_failure_recovery_error)
        })
        .await
    }
    #[napi(catch_unwind)]
    pub async fn raw_finalize(&self, owner_handle: String) -> Result<String> {
        let handles = Arc::clone(&self.handles);
        scan_blocking(move || {
            let _admission = handles
                .admission
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery admission interrupted"))?;
            let (lease, owner) = handles.owner(&owner_handle)?;
            let lease_id = handles
                .owners
                .lock()
                .map_err(|_| napi::Error::from_reason("scan recovery handle registry interrupted"))?
                .get(&owner_handle)
                .ok_or_else(|| napi::Error::from_reason("unknown or revoked scan recovery owner"))?
                .0
                .clone();
            handles.reserve_cleanup()?;
            let receipt = lease.raw_finalize(&owner).map_err(|error| {
                handles.release_cleanup_reservation();
                scan_failure_recovery_error(error)
            })?;
            let id = handles.issue_reserved_cleanup(&lease_id, receipt)?;
            handles.retire_finalized_owner(&owner_handle)?;
            Ok(serde_json::json!({"cleanupReceiptId":id}).to_string())
        })
        .await
    }
}

#[cfg(test)]
#[expect(clippy::items_after_test_module, reason = "legacy")]
mod scan_recovery_handle_tests {
    use super::{
        PendingScanFailure, RecoveryHandles, ScanFailureRecoveryStore, ScanRecoveryNoWake,
        MAX_SCAN_RECOVERY_HANDLES_PER_KIND,
    };
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    use ubm_core::oversized_evidence::{
        EvidenceLimits, EvidenceSource, EvidenceToken, ScanFailureIdentity, ScanFailureLimits,
        ScanFailureMember, ScanFailureOccurrence,
    };
    use ubm_desktop::continuation_journal::{
        JournalError, JournalQuota, JournalRegistry, ScanRecoveryStorageProtector,
    };
    use ubm_desktop::continuation_outbox::{Outbox, ScanFailureOutboxError, WakeSink};
    use ubm_desktop::scan_failure_recovery::ScanFailureRecoveryRegistry;

    struct TestProtector;

    impl ScanRecoveryStorageProtector for TestProtector {
        fn provision(&self, _: &std::path::Path) -> Result<(), JournalError> {
            Ok(())
        }

        fn validate(&self, _: &std::path::Path) -> Result<(), JournalError> {
            Ok(())
        }
    }

    struct TestWake;

    impl WakeSink for TestWake {
        fn wake(&self, _: u64) {}
    }

    static TEMPORARY_DIRECTORY_SERIAL: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ubm-napi-scan-recovery-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            TEMPORARY_DIRECTORY_SERIAL.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).expect("fixture directory");
        #[cfg(unix)]
        std::fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("private fixture directory");
        directory
    }

    fn create_recording_with_real_v2_recovery(
        registry: &Arc<JournalRegistry>,
        recording_id: &str,
        occurrence_serial: u64,
        max_encoded_bytes: u64,
    ) {
        let journal = registry
            .open(
                recording_id,
                &serde_json::json!({}),
                JournalQuota {
                    max_bytes: 1 << 20,
                    max_records: 8,
                },
            )
            .expect("create trusted recording");
        // Session identity belongs to this recording's durable recovery
        // domain.  The fixture varies the source identity, not that stable
        // session identity, so all 1,025 records use a valid real session.
        let outbox = Outbox::new(41, Arc::new(TestWake));
        outbox
            .attach_journal(Arc::clone(&journal), serde_json::json!({}))
            .expect("attach durable outbox");
        outbox
            .configure_scan_failure_recovery_authority([0xA5; 32])
            .expect("configure recovery authority");
        let mut reservation = outbox
            .reserve_scan_failure(ScanFailureLimits {
                evidence: EvidenceLimits {
                    max_depth: 8,
                    max_tokens: 20,
                    // One byte creates an unprepared raw-only occurrence;
                    // normal limits exercise prepared cleanup receipts.
                    max_encoded_bytes,
                    max_chunks: 8,
                    chunk_bytes: 64,
                },
                max_recipients: 1,
                max_bookkeeping_bytes: 4_096,
            })
            .expect("reserve real scan failure");
        let mut occurrence = [0; 16];
        occurrence[8..].copy_from_slice(&occurrence_serial.to_be_bytes());
        let mut source = Some(ScanFailureOccurrence::new(
            ScanFailureIdentity {
                host: [1; 16],
                scan: [2; 16],
                start: [3; 16],
                source_generation: [4; 16],
                occurrence,
            },
            vec![ScanFailureMember {
                recipient: [5; 16],
                delivery: [6; 16],
            }],
            Arc::new(EvidenceSource::new(vec![
                EvidenceToken::Array(3),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Array(9),
                EvidenceToken::Null,
                EvidenceToken::Bool(true),
                EvidenceToken::I64(-7),
                EvidenceToken::U64(9),
                EvidenceToken::F64Bits(0x3ff0_0000_0000_0000),
                EvidenceToken::Text("napi-real-recovery".to_owned()),
                EvidenceToken::Bytes(vec![1, 2, 3]),
                EvidenceToken::Map(1),
                EvidenceToken::Null,
                EvidenceToken::Null,
                EvidenceToken::Array(1),
                EvidenceToken::Null,
            ])),
        ));
        let committed = outbox.commit_scan_failure(&mut reservation, &mut source);
        if max_encoded_bytes == 1 {
            assert!(matches!(
                committed,
                Err(ScanFailureOutboxError::Recovery(_))
            ));
        } else {
            committed.expect("prepare real scan failure");
        }
        outbox
            .finish_scan_failure_producer(&mut reservation)
            .expect("finish durable scan failure");
        drop(reservation);
        drop(outbox);
        drop(journal);
    }

    fn real_store(
        recording_ids: &[String],
        max_encoded_bytes: u64,
    ) -> (ScanFailureRecoveryStore, Arc<JournalRegistry>, PathBuf) {
        let directory = temporary_directory("fixture");
        let journals = Arc::new(JournalRegistry::default());
        journals
            .configure_directory_with_scan_recovery_storage(&directory, Arc::new(TestProtector))
            .expect("trusted scan recovery storage");
        for (index, recording_id) in recording_ids.iter().enumerate() {
            create_recording_with_real_v2_recovery(
                &journals,
                recording_id,
                index as u64 + 1,
                max_encoded_bytes,
            );
        }
        (
            ScanFailureRecoveryStore {
                registry: Arc::new(ScanFailureRecoveryRegistry::new(
                    Arc::clone(&journals),
                    Arc::new(ScanRecoveryNoWake),
                )),
                handles: Arc::new(RecoveryHandles::new()),
            },
            journals,
            directory,
        )
    }

    fn owner(ordinal: u64, capability: u8) -> PendingScanFailure {
        PendingScanFailure {
            ordinal,
            capability: [capability; 16],
        }
    }

    async fn finalized_cleanup_receipt(
        store: &ScanFailureRecoveryStore,
        recording_id: String,
    ) -> ubm_desktop::continuation_outbox::ScanFailureCleanupRetirement {
        let attached = store
            .attach(recording_id)
            .await
            .expect("attach authenticated recovery");
        let lease_id = serde_json::from_str::<serde_json::Value>(&attached)
            .expect("attach response")
            .get("leaseId")
            .and_then(serde_json::Value::as_str)
            .expect("lease id")
            .to_owned();
        let owners = serde_json::from_str::<serde_json::Value>(
            &store.owners(lease_id).await.expect("owners response"),
        )
        .expect("owners JSON");
        let owner_id = owners
            .get("owners")
            .and_then(serde_json::Value::as_array)
            .and_then(|owners| owners.first())
            .and_then(|owner| owner.get("handleId"))
            .and_then(serde_json::Value::as_str)
            .expect("owner handle")
            .to_owned();
        let head = serde_json::from_str::<serde_json::Value>(
            &store.head(owner_id, 0).await.expect("head response"),
        )
        .expect("head JSON");
        let head_id = head
            .get("head")
            .and_then(|head| head.get("handleId"))
            .and_then(serde_json::Value::as_str)
            .expect("prepared head handle")
            .to_owned();
        let mut offset = "0".to_owned();
        loop {
            let page = serde_json::from_str::<serde_json::Value>(
                &store
                    .read(head_id.clone(), offset.clone())
                    .await
                    .expect("read head"),
            )
            .expect("read response");
            if page.get("done").and_then(serde_json::Value::as_bool) == Some(true) {
                break;
            }
            let next_offset = page
                .get("nextOffset")
                .and_then(serde_json::Value::as_str)
                .expect("next recovery page offset")
                .to_owned();
            assert_ne!(
                next_offset, offset,
                "unfinished recovery reads must advance"
            );
            offset = next_offset;
        }
        store
            .acknowledge(head_id.clone())
            .await
            .expect("acknowledge delivered source head");
        store.retire(head_id.clone()).await.expect("retire head");
        store.release(head_id.clone()).await.expect("release head");
        let finalized = serde_json::from_str::<serde_json::Value>(
            &store
                .finalize_release(head_id)
                .await
                .expect("finalize released head"),
        )
        .expect("finalize response");
        let cleanup_id = finalized
            .get("cleanupReceiptId")
            .and_then(serde_json::Value::as_str)
            .expect("cleanup handle")
            .to_owned();
        let (_, receipt) = store
            .handles
            .cleanup(&cleanup_id)
            .expect("opaque cleanup receipt");
        store
            .retire_cleanup(cleanup_id)
            .await
            .expect("retire durable cleanup receipt");
        receipt
    }

    #[test]
    fn repeated_owner_inventory_reuses_its_process_local_alias() {
        let handles = RecoveryHandles::new();
        let first = handles.issue_owner("lease-a", owner(1, 9)).unwrap();
        let repeated = handles.issue_owner("lease-a", owner(1, 9)).unwrap();
        assert_eq!(first, repeated);
    }

    #[test]
    fn another_active_owner_is_not_replaced_by_repeated_inventory() {
        let handles = RecoveryHandles::new();
        let first = handles.issue_owner("lease-a", owner(1, 9)).unwrap();
        let second = handles.issue_owner("lease-a", owner(2, 9)).unwrap();
        assert_ne!(first, second);
        assert_eq!(first, handles.issue_owner("lease-a", owner(1, 9)).unwrap());
        assert_eq!(second, handles.issue_owner("lease-a", owner(2, 9)).unwrap());
    }

    #[test]
    fn aliases_are_scoped_to_the_recovery_lease() {
        let handles = RecoveryHandles::new();
        let first = handles.issue_owner("lease-a", owner(1, 9)).unwrap();
        let second = handles.issue_owner("lease-b", owner(1, 9)).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn finalized_owner_reclamation_allows_more_than_the_live_capacity_without_evicting_another_owner(
    ) {
        let handles = RecoveryHandles::new();
        let active = handles.issue_owner("lease-a", owner(99_999, 9)).unwrap();
        for ordinal in 0..1_025 {
            let id = handles.issue_owner("lease-a", owner(ordinal, 7)).unwrap();
            handles.retire_finalized_owner(&id).unwrap();
        }
        // Sequential finalized generations did not consume the live bound,
        // and the concurrent owner was never evicted to make room.
        assert_eq!(
            active,
            handles.issue_owner("lease-a", owner(99_999, 9)).unwrap()
        );
    }

    #[tokio::test]
    async fn real_v2_lease_detach_releases_the_alias_and_core_lease_for_reattach() {
        let recording_id = "napi-detach-reuse".to_owned();
        let (store, journals, directory) = real_store(std::slice::from_ref(&recording_id), 1);
        let attached = store
            .attach(recording_id.clone())
            .await
            .expect("attach authenticated recovery");
        let lease_id = serde_json::from_str::<serde_json::Value>(&attached)
            .expect("attach response")
            .get("leaseId")
            .and_then(serde_json::Value::as_str)
            .expect("lease id")
            .to_owned();
        store.detach(lease_id).await.expect("detach idle recovery");
        let reattached = store
            .attach(recording_id)
            .await
            .expect("reattach after explicit detach");
        assert_ne!(attached, reattached, "a revoked alias is never revived");
        drop(store);
        journals
            .release_idle_after_host_shutdown()
            .expect("release idle fixture journals");
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[tokio::test]
    async fn real_v2_lease_cannot_detach_while_an_inventory_alias_is_live() {
        let recording_id = "napi-detach-live-owner".to_owned();
        let (store, journals, directory) = real_store(std::slice::from_ref(&recording_id), 1);
        let attached = store
            .attach(recording_id)
            .await
            .expect("attach authenticated recovery");
        let lease_id = serde_json::from_str::<serde_json::Value>(&attached)
            .expect("attach response")
            .get("leaseId")
            .and_then(serde_json::Value::as_str)
            .expect("lease id")
            .to_owned();
        let owners = store
            .owners(lease_id.clone())
            .await
            .expect("inventory durable owner");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&owners)
                .expect("owners response")
                .get("owners")
                .and_then(serde_json::Value::as_array)
                .expect("owner array")
                .len(),
            1
        );
        let error = store
            .detach(lease_id)
            .await
            .expect_err("live owner alias prevents revocation");
        assert!(
            error.reason.contains("dependent aliases"),
            "detach reports why the durable lease remains live: {}",
            error.reason
        );
        drop(store);
        journals
            .release_idle_after_host_shutdown()
            .expect("release idle fixture journals");
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[tokio::test]
    async fn real_v2_raw_tokens_use_the_canonical_node_json_codec_and_bounds() {
        let recording_id = "napi-raw-token-codec".to_owned();
        let (store, journals, directory) = real_store(std::slice::from_ref(&recording_id), 1);
        let lease = serde_json::from_str::<serde_json::Value>(
            &store.attach(recording_id).await.expect("attach"),
        )
        .expect("lease json")["leaseId"]
            .as_str()
            .expect("lease id")
            .to_owned();
        let owners =
            serde_json::from_str::<serde_json::Value>(&store.owners(lease).await.expect("owners"))
                .expect("owners json");
        let owner = owners["owners"][0]["handleId"]
            .as_str()
            .expect("owner id")
            .to_owned();
        let expected = [
            "array", "null", "null", "array", "null", "bool", "i64", "u64", "f64Bits", "text",
            "bytes", "map", "null", "null", "array", "null",
        ];
        for (index, kind) in expected.into_iter().enumerate() {
            let token = serde_json::from_str::<serde_json::Value>(
                &store
                    .raw_token(owner.clone(), index as u32, 0, 64)
                    .await
                    .expect("raw token"),
            )
            .expect("token json");
            assert_eq!(token["kind"], kind);
            match kind {
                "f64Bits" => assert_eq!(token["value"], "4607182418800017408"),
                "text" | "bytes" => {
                    assert!(token["total"].as_str().is_some());
                    assert!(token["bytes"].as_array().is_some());
                }
                "map" | "array" => assert!(token["count"].as_str().is_some()),
                _ => {}
            }
        }
        assert!(store.raw_token(owner.clone(), 0, 0, 0).await.is_err());
        assert!(store.raw_token(owner, 0, 0, 65_537).await.is_err());
        drop(store);
        journals
            .release_idle_after_host_shutdown()
            .expect("release idle fixture journals");
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[tokio::test]
    async fn real_v2_detach_and_inventory_serialize_without_returning_a_revoked_owner() {
        let recording_id = "napi-detach-inventory-interleave".to_owned();
        let (store, journals, directory) = real_store(std::slice::from_ref(&recording_id), 1);
        let store = Arc::new(store);
        let attached = store
            .attach(recording_id)
            .await
            .expect("attach authenticated recovery");
        let lease_id = serde_json::from_str::<serde_json::Value>(&attached)
            .expect("attach response")
            .get("leaseId")
            .and_then(serde_json::Value::as_str)
            .expect("lease id")
            .to_owned();

        // Force both N-API worker closures to reach the admission boundary
        // before either may acquire the lifecycle mutex. This is deliberately
        // test-only: production has no scheduling hook.
        store.handles.hold_next_admissions(2);
        let inventory_store = Arc::clone(&store);
        let detach_store = Arc::clone(&store);
        let inventory_lease_id = lease_id.clone();
        let detach_lease_id = lease_id.clone();
        let (inventory, detached) = tokio::join!(
            inventory_store.owners(inventory_lease_id),
            detach_store.detach(detach_lease_id),
        );

        match (inventory, detached) {
            (Ok(owners), Err(error)) => {
                assert!(
                    error.reason.contains("dependent aliases"),
                    "detach must report the newly published owner alias: {}",
                    error.reason
                );
                let owner_id = serde_json::from_str::<serde_json::Value>(&owners)
                    .expect("owners response")
                    .get("owners")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|owners| owners.first())
                    .and_then(|owner| owner.get("handleId"))
                    .and_then(serde_json::Value::as_str)
                    .expect("owner handle")
                    .to_owned();
                store
                    .member(owner_id, 0)
                    .await
                    .expect("a returned owner remains usable when detach was rejected");
            }
            (Err(error), Ok(())) => assert!(
                error
                    .reason
                    .contains("unknown or revoked scan recovery handle"),
                "inventory after detach must observe revocation: {}",
                error.reason
            ),
            (inventory, detached) => panic!(
                "admission must serialize detach and inventory; inventory={inventory:?}, detach={detached:?}"
            ),
        }
        drop(store);
        journals
            .release_idle_after_host_shutdown()
            .expect("release idle fixture journals");
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[tokio::test]
    async fn real_v2_distinct_recordings_reclaim_lease_capacity_after_detach() {
        let (store, journals, directory) = real_store(&[], 1);
        for index in 0..=MAX_SCAN_RECOVERY_HANDLES_PER_KIND {
            let recording_id = format!("napi-detach-capacity-{index}");
            create_recording_with_real_v2_recovery(&journals, &recording_id, index as u64 + 1, 1);
            let attached = store
                .attach(recording_id)
                .await
                .expect("attach each authenticated recovery");
            let lease_id = serde_json::from_str::<serde_json::Value>(&attached)
                .expect("attach response")
                .get("leaseId")
                .and_then(serde_json::Value::as_str)
                .expect("lease id")
                .to_owned();
            store
                .detach(lease_id)
                .await
                .expect("detach each idle recovery");
        }
        drop(store);
        journals
            .release_idle_after_host_shutdown()
            .expect("release idle fixture journals");
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[tokio::test]
    async fn real_cleanup_receipts_preflight_atomically_and_deduplicate_aliases() {
        let recording_ids = [
            "napi-cleanup-batch-first".to_owned(),
            "napi-cleanup-batch-second".to_owned(),
        ];
        let (store, journals, directory) = real_store(&recording_ids, 4_096);
        let first = finalized_cleanup_receipt(&store, recording_ids[0].clone()).await;
        let second = finalized_cleanup_receipt(&store, recording_ids[1].clone()).await;

        let duplicate_handles = RecoveryHandles::with_limit(1);
        let duplicate = duplicate_handles
            .issue_cleanup_batch("lease", vec![first.clone(), first.clone()])
            .expect("duplicate receipt uses one bounded alias");
        assert_eq!(duplicate[0], duplicate[1]);
        assert_eq!(
            duplicate_handles
                .cleanup
                .lock()
                .expect("cleanup handles")
                .len(),
            1,
            "duplicate inventory must not consume another live alias"
        );

        let constrained_handles = RecoveryHandles::with_limit(1);
        let error = constrained_handles
            .issue_cleanup_batch("lease", vec![first, second])
            .expect_err("two new receipts cannot partially mint through one-slot capacity");
        assert!(error.reason.contains("capacity exhausted"));
        assert!(
            constrained_handles
                .cleanup
                .lock()
                .expect("cleanup handles")
                .is_empty(),
            "capacity rejection leaves no hidden partial cleanup aliases"
        );
        drop(store);
        journals
            .release_idle_after_host_shutdown()
            .expect("release idle fixture journals");
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[cfg(feature = "reclaim-test-support")]
    #[tokio::test]
    async fn capacity_rejection_does_not_attach_a_hidden_core_lease() {
        let recording_ids = [
            "napi-capacity-no-core-leak-first".to_owned(),
            "napi-capacity-no-core-leak-second".to_owned(),
        ];
        let (mut store, _, directory) = real_store(&recording_ids, 1);
        store.handles = Arc::new(RecoveryHandles::with_limit(1));
        for (index, recording_id) in recording_ids.iter().take(1).enumerate() {
            store
                .attach(recording_id.clone())
                .await
                .unwrap_or_else(|error| panic!("fill public lease capacity at {index}: {error}"));
        }
        assert_eq!(
            store.registry.lease_count(),
            1,
            "every public alias maps to exactly one core lease"
        );
        let error = store
            .attach(recording_ids[1].clone())
            .await
            .expect_err("public capacity rejects the next recording before core attach");
        assert!(error.reason.contains("capacity exhausted"));
        assert_eq!(
            store.registry.lease_count(),
            1,
            "a rejected public alias must not leave a hidden registry lease"
        );
        drop(store);
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }
}

impl ContinuationRecordingStore {
    async fn transition_diagnostic_occurrence(
        &self,
        id: String,
        occurrence: String,
        payload: String,
        transition: &'static str,
    ) -> Result<String> {
        let registry = self.registry.clone();
        Ok(envelope(run_blocking(move || {
            let occurrence = diagnostic_id(&occurrence)?;
            let journal = registry.get(&id).map_err(recording_failure)?;
            match transition {
                "acknowledge" => journal.acknowledge_diagnostic(occurrence, &payload),
                "retire" => journal.retire_diagnostic(occurrence, &payload),
                "confirm" => journal.confirm_diagnostic_retirement(occurrence, &payload).map(|()| occurrence),
                "release" => journal.release_diagnostic_confirmation(occurrence, &payload).map(|()| occurrence),
                _ => unreachable!("fixed diagnostic transition"),
            }.map_err(recording_failure)?;
            Ok(serde_json::json!({"occurrence":diagnostic_id_text(occurrence),"transition":transition}))
        }).await))
    }
}

fn parse_exact_page(text: &str) -> ubm_desktop::continuation::Result<serde_json::Value> {
    if text.len() > 18 * 1024 * 1024 {
        return Err(
            serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.legacy-exact","detail":"recovery page too large","platform":null}),
        );
    }
    serde_json::from_str(text).map_err(|_| serde_json::json!({"code":"argument.invalid","domain":"restoration","operation":"continuation.recording.legacy-exact","detail":"invalid recovery page","platform":null}))
}
